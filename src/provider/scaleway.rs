use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use time::OffsetDateTime;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Cloud, Machine};
use crate::secret;

const API: &str = "https://api.scaleway.com/instance/v1";
const BLOCK_API: &str = "https://api.scaleway.com/block/v1";
const MARKETPLACE_API: &str = "https://api.scaleway.com/marketplace/v2";

const AUTH_HEADER: &str = "X-Auth-Token";
const CLOUD_INIT_KEY: &str = "cloud-init";
const ROOT_VOLUME_BYTES: u64 = 80 * 1_000_000_000;
const ROOT_VOLUME_TYPE: &str = "sbs_volume";
const PER_PAGE: u32 = 100;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const DETACH_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Scaleway {
	http: reqwest::Client,
	token: String,
	project_id: String,
	zones: Vec<String>,
}

#[derive(Deserialize)]
struct Wrapped {
	server: Server,
}

#[derive(Deserialize)]
struct Listed {
	servers: Vec<Server>,
}

#[derive(Deserialize)]
struct Server {
	id: String,
	#[serde(default)]
	name: String,
	#[serde(default)]
	creation_date: Option<String>,
	#[serde(default)]
	state: Option<String>,
	#[serde(default)]
	volumes: BTreeMap<String, Volume>,
}

#[derive(Deserialize)]
struct Volume {
	id: String,
	#[serde(default)]
	volume_type: String,
}

#[derive(Deserialize)]
struct LocalImages {
	local_images: Vec<LocalImage>,
}

#[derive(Deserialize)]
struct LocalImage {
	id: String,
	#[serde(default)]
	compatible_commercial_types: Vec<String>,
}

impl Scaleway {
	pub fn from_env(locations: &[String]) -> Result<Self> {
		Ok(Self {
			http: super::http_client()?,
			token: secret::load("SCALEWAY_TOKEN")?,
			project_id: secret::load("SCALEWAY_PROJECT_ID")?,
			zones: locations.to_vec(),
		})
	}

	fn scope(&self, zone: &str) -> String {
		format!("{API}/zones/{zone}/servers")
	}

	/// Create takes a local image id, not a label, and only the instance_sbs one
	/// clones into volume 0 instead of leaving it raw: https://github.com/scaleway/scaleway-sdk-go/blob/master/api/marketplace/v2/marketplace_utils.go
	async fn local_image(
		&self,
		zone: &str,
		plan: &str,
		image: &str,
	) -> Result<String> {
		if is_uuid(image) {
			return Ok(image.to_owned());
		}
		let label = image.replace('-', "_");
		let response = self
			.http
			.get(format!("{MARKETPLACE_API}/local-images"))
			.query(&[
				("image_label", label.as_str()),
				("zone", zone),
				("type", root_image_type()),
			])
			.header(AUTH_HEADER, &self.token)
			.send()
			.await
			.context("scaleway: resolving image")?;
		let listed: LocalImages =
			super::decode(response, "scaleway: resolving image").await?;
		compatible_image(&listed.local_images, plan).with_context(|| {
			format!(
				"scaleway: no {} image {label} for {plan} in {zone}",
				root_image_type()
			)
		})
	}

	/// cloud-init is no field of the create body, and only ever plain text: https://www.scaleway.com/en/docs/instances/how-to/use-cloud-init/
	async fn set_user_data(
		&self,
		zone: &str,
		server: &str,
		user_data: &str,
	) -> Result<()> {
		let response = self
			.http
			.patch(format!(
				"{}/{server}/user_data/{CLOUD_INIT_KEY}",
				self.scope(zone)
			))
			.header(AUTH_HEADER, &self.token)
			.header(reqwest::header::CONTENT_TYPE, "text/plain")
			.body(user_data.to_owned())
			.send()
			.await
			.context("scaleway: setting cloud-init user data")?;
		accepted(response, "scaleway: setting cloud-init user data").await
	}

	/// A created Instance stays stopped until it is powered on: https://www.scaleway.com/en/developers/api/instance/#path-instances-perform-action
	async fn power_on(&self, zone: &str, server: &str) -> Result<()> {
		let response = self
			.http
			.post(format!("{}/{server}/action", self.scope(zone)))
			.header(AUTH_HEADER, &self.token)
			.json(&json!({ "action": "poweron" }))
			.send()
			.await
			.context("scaleway: starting instance")?;
		accepted(response, "scaleway: starting instance").await
	}

	async fn inspect(
		&self,
		zone: &str,
		server: &str,
	) -> Result<Option<Server>> {
		let response = self
			.http
			.get(format!("{}/{server}", self.scope(zone)))
			.header(AUTH_HEADER, &self.token)
			.send()
			.await
			.context("scaleway: reading instance")?;
		if response.status() == reqwest::StatusCode::NOT_FOUND {
			return Ok(None);
		}
		let fetched: Wrapped =
			super::decode(response, "scaleway: reading instance").await?;
		Ok(Some(fetched.server))
	}

	async fn act(&self, zone: &str, server: &str, action: &str) -> Result<()> {
		let response = self
			.http
			.post(format!("{}/{server}/action", self.scope(zone)))
			.header(AUTH_HEADER, &self.token)
			.json(&json!({ "action": action }))
			.send()
			.await
			.context("scaleway: acting on instance")?;
		super::expect_gone(response, "scaleway: acting on instance").await
	}

	async fn delete_server(&self, zone: &str, server: &str) -> Result<()> {
		let response = self
			.http
			.delete(format!("{}/{server}", self.scope(zone)))
			.header(AUTH_HEADER, &self.token)
			.send()
			.await
			.context("scaleway: deleting instance")?;
		super::expect_gone(response, "scaleway: deleting instance").await
	}

	/// A volume is refused while it is still in_use, and terminate detaches asynchronously: https://www.scaleway.com/en/developers/api/block/#path-volumes-delete-a-detached-volume
	async fn delete_volume(&self, zone: &str, volume: &str) -> Result<()> {
		let deadline = Instant::now() + DETACH_TIMEOUT;
		loop {
			let response = self
				.http
				.delete(format!("{BLOCK_API}/zones/{zone}/volumes/{volume}"))
				.header(AUTH_HEADER, &self.token)
				.send()
				.await
				.with_context(|| {
					format!("scaleway: deleting root volume {zone}/{volume}")
				})?;
			let status = response.status();
			if status.is_success()
				|| status == reqwest::StatusCode::NOT_FOUND
				|| !retryable(status)
				|| Instant::now() >= deadline
			{
				return super::expect_gone(
					response,
					&format!("scaleway: deleting root volume {zone}/{volume}"),
				)
				.await;
			}
			tokio::time::sleep(POLL_INTERVAL).await;
		}
	}
}

impl Cloud for Scaleway {
	async fn create(
		&self,
		name: &str,
		plan: &str,
		location: &str,
		image: &str,
		ssh_key: Option<&str>,
		user_data: &str,
	) -> Result<Machine> {
		let image = self.local_image(location, plan, image).await?;
		let body = create_body(name, plan, &image, &self.project_id, ssh_key)?;
		let response = self
			.http
			.post(self.scope(location))
			.header(AUTH_HEADER, &self.token)
			.json(&body)
			.send()
			.await
			.context("scaleway: creating instance")?;
		let created: Wrapped =
			super::decode(response, "scaleway: creating instance").await?;
		let server = created.server;
		self.set_user_data(location, &server.id, user_data).await?;
		self.power_on(location, &server.id).await?;
		Ok(Machine {
			id: machine_id(location, &server.id),
			name: server.name,
			created_at: server
				.creation_date
				.as_deref()
				.and_then(super::timestamp)
				.or_else(|| Some(OffsetDateTime::now_utc())),
		})
	}

	async fn destroy(&self, id: &str) -> Result<()> {
		let (zone, server) = split_id(id)?;
		let Some(found) = self.inspect(zone, server).await? else {
			return Ok(());
		};
		let volumes = block_volumes_of(&found);
		// terminate is offered only while the Instance runs, and cloud-init powers
		// the guest off at lifetime_minutes, before the sweep ever looks at it.
		if terminable(found.state.as_deref()) {
			self.act(zone, server, "terminate").await?;
		} else {
			self.delete_server(zone, server).await?;
		}
		for volume in volumes {
			self.delete_volume(zone, &volume).await?;
		}
		Ok(())
	}

	async fn list(&self, prefix: &str) -> Result<Vec<Machine>> {
		let mut machines = Vec::new();
		for zone in &self.zones {
			let mut page = 1;
			loop {
				let response = self
					.http
					.get(self.scope(zone))
					.query(&[
						("project", self.project_id.clone()),
						("per_page", PER_PAGE.to_string()),
						("page", page.to_string()),
					])
					.header(AUTH_HEADER, &self.token)
					.send()
					.await
					.context("scaleway: listing instances")?;
				let listed: Listed =
					super::decode(response, "scaleway: listing instances")
						.await?;
				let read = listed.servers.len();
				machines.extend(
					listed
						.servers
						.into_iter()
						.filter(|server| server.name.starts_with(prefix))
						.map(|server| Machine {
							id: machine_id(zone, &server.id),
							name: server.name,
							created_at: server
								.creation_date
								.as_deref()
								.and_then(super::timestamp),
						}),
				);
				if read < PER_PAGE as usize {
					break;
				}
				page += 1;
			}
		}
		Ok(machines)
	}
}

/// Volume 0 carries the type and the size and nothing else, a name included: https://github.com/scaleway/scaleway-cli/blob/master/internal/namespaces/instance/v1/custom_server_create.go
fn create_body(
	name: &str,
	plan: &str,
	image: &str,
	project: &str,
	ssh_key: Option<&str>,
) -> Result<Value> {
	if ssh_key.is_some() {
		bail!(
			"scaleway: the Instance API takes no ssh key, every key of the \
			 project is injected at boot; drop ssh_key from this label"
		);
	}
	Ok(json!({
		"name": name,
		"commercial_type": plan,
		"image": image,
		"project": project,
		"dynamic_ip_required": true,
		"volumes": {
			"0": {
				"volume_type": ROOT_VOLUME_TYPE,
				"size": ROOT_VOLUME_BYTES,
			},
		},
	}))
}

/// A label holds one image per architecture; the commercial type is what tells
/// them apart: https://github.com/scaleway/scaleway-sdk-go/blob/master/api/marketplace/v2/marketplace_utils.go
fn compatible_image(images: &[LocalImage], plan: &str) -> Option<String> {
	images
		.iter()
		.find(|image| {
			image
				.compatible_commercial_types
				.iter()
				.any(|candidate| candidate.eq_ignore_ascii_case(plan))
		})
		.map(|image| image.id.clone())
}

fn is_uuid(value: &str) -> bool {
	value.len() == 36
		&& value
			.chars()
			.enumerate()
			.all(|(index, character)| match index {
				8 | 13 | 18 | 23 => character == '-',
				_ => character.is_ascii_hexdigit(),
			})
}

fn machine_id(zone: &str, server: &str) -> String {
	format!("{zone}/{server}")
}

fn split_id(id: &str) -> Result<(&str, &str)> {
	id.split_once('/').ok_or_else(|| {
		anyhow!("scaleway: instance id {id:?} is not <zone>/<uuid>")
	})
}

/// terminate deletes l_ssd and scratch volumes, an sbs_volume it only detaches: https://www.scaleway.com/en/developers/api/instance/#path-instances-perform-action
/// Derived, not a second constant, so the pair cannot drift apart: https://github.com/scaleway/scaleway-cli/blob/master/internal/namespaces/instance/v1/custom_server_create_builder.go
fn root_image_type() -> &'static str {
	match ROOT_VOLUME_TYPE {
		"sbs_volume" => "instance_sbs",
		_ => "instance_local",
	}
}

/// A detach resolves on a retry; a rejected token never does.
fn retryable(status: reqwest::StatusCode) -> bool {
	!matches!(
		status,
		reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
	)
}

fn block_volumes_of(server: &Server) -> Vec<String> {
	server
		.volumes
		.values()
		.filter(|volume| volume.volume_type == ROOT_VOLUME_TYPE)
		.map(|volume| volume.id.clone())
		.collect()
}

/// Only a running Instance offers terminate; a stopped one has to be deleted:
/// https://www.scaleway.com/en/developers/api/instance/#path-instances-perform-action
fn terminable(state: Option<&str>) -> bool {
	matches!(state, Some("running"))
}

async fn accepted(response: reqwest::Response, what: &str) -> Result<()> {
	let status = response.status();
	if status.is_success() {
		return Ok(());
	}
	bail!(
		"{what}: HTTP {status}: {}",
		super::clip(&response.text().await.unwrap_or_default())
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	const LOCAL_IMAGE: &str = "65ce6135-f6d5-4e90-82ec-ef09d29d1cff";

	fn body() -> Value {
		create_body(
			"ci-orc-check-a1b2c3",
			"BASIC2-A8C-16G",
			LOCAL_IMAGE,
			"11111111-1111-1111-1111-111111111111",
			None,
		)
		.unwrap()
	}

	fn images() -> Vec<LocalImage> {
		let listed: LocalImages = serde_json::from_value(json!({
			"local_images": [
				{
					"id": LOCAL_IMAGE,
					"arch": "arm64",
					"compatible_commercial_types": [
						"COPARM1-2C-8G",
						"BASIC2-A8C-16G",
					],
				},
				{
					"id": "6d3c053e-c728-4294-b23a-560b62a4d592",
					"arch": "x86_64",
					"compatible_commercial_types": ["DEV1-S", "PRO2-XXS"],
				},
			],
		}))
		.unwrap();
		listed.local_images
	}

	#[test]
	fn splits_a_composite_id_into_zone_and_server() {
		let (zone, server) = split_id("fr-par-1/abc-123").unwrap();
		assert_eq!(zone, "fr-par-1");
		assert_eq!(server, "abc-123");
	}

	#[test]
	fn splits_only_on_the_first_separator() {
		assert_eq!(split_id("fr-par-1/a/b").unwrap(), ("fr-par-1", "a/b"));
	}

	#[test]
	fn rejects_an_id_that_carries_no_zone() {
		let error = split_id("abc-123").unwrap_err().to_string();
		assert!(error.contains("<zone>/<uuid>"), "{error}");
	}

	#[test]
	fn builds_the_composite_id_create_and_list_share() {
		let built = machine_id("nl-ams-1", "abc-123");
		assert_eq!(built, "nl-ams-1/abc-123");
		assert_eq!(split_id(&built).unwrap(), ("nl-ams-1", "abc-123"));
	}

	#[test]
	fn sizes_the_root_volume_itself_instead_of_inheriting_the_image() {
		let root = body()["volumes"]["0"].clone();
		assert_eq!(root["volume_type"], "sbs_volume");
		assert_eq!(root["size"], 80_000_000_000u64);
		assert_eq!(ROOT_VOLUME_BYTES % 512, 0);
	}

	#[test]
	fn describes_the_root_volume_by_nothing_but_its_type_and_size() {
		let root = body()["volumes"]["0"].clone();
		let described: Vec<&str> = root
			.as_object()
			.unwrap()
			.keys()
			.map(String::as_str)
			.collect();
		assert_eq!(described, vec!["size", "volume_type"]);
	}

	#[test]
	fn sends_a_resolved_local_image_and_lets_the_type_pick_the_arch() {
		let body = body();
		assert_eq!(body["image"], LOCAL_IMAGE);
		assert_eq!(body["commercial_type"], "BASIC2-A8C-16G");
		assert!(body.get("arch").is_none());
	}

	#[test]
	fn picks_the_local_image_the_commercial_type_can_boot() {
		let images = images();
		assert_eq!(
			compatible_image(&images, "BASIC2-A8C-16G").as_deref(),
			Some(LOCAL_IMAGE)
		);
		assert_eq!(
			compatible_image(&images, "basic2-a8c-16g").as_deref(),
			Some(LOCAL_IMAGE)
		);
		assert_eq!(
			compatible_image(&images, "DEV1-S").as_deref(),
			Some("6d3c053e-c728-4294-b23a-560b62a4d592")
		);
	}

	#[test]
	fn resolves_no_image_for_a_type_no_label_variant_lists() {
		assert!(compatible_image(&images(), "GP1-XS").is_none());
		assert!(compatible_image(&[], "BASIC2-A8C-16G").is_none());
	}

	#[test]
	fn tells_a_local_image_id_apart_from_a_marketplace_label() {
		assert!(is_uuid(LOCAL_IMAGE));
		assert!(!is_uuid("debian_bookworm"));
		assert!(!is_uuid("debian-bookworm"));
		assert!(!is_uuid("65ce6135f6d54e9082ecef09d29d1cff"));
		assert!(!is_uuid("65ce6135-f6d5-4e90-82ec-ef09d29d1cfg"));
		assert!(!is_uuid("65ce6135-f6d5-4e90-82ec-ef09d29d1cff-"));
	}

	#[test]
	fn asks_for_the_dynamic_ip_that_cloud_init_needs() {
		assert_eq!(body()["dynamic_ip_required"], true);
	}

	#[test]
	fn keeps_user_data_out_of_the_create_body() {
		let body = body();
		assert!(body.get("user_data").is_none());
		assert!(body.get("cloud_init").is_none());
	}

	#[test]
	fn refuses_an_ssh_key_the_api_has_no_field_for() {
		let error = create_body("n", "t", "i", "p", Some("ci"))
			.unwrap_err()
			.to_string();
		assert!(error.contains("ssh_key"), "{error}");
	}

	#[test]
	fn deletes_only_the_volumes_terminate_leaves_behind() {
		let server: Server = serde_json::from_value(json!({
			"id": "abc-123",
			"name": "ci-orc-check-a1b2c3",
			"volumes": {
				"0": { "id": "root-1", "volume_type": "sbs_volume" },
				"1": { "id": "local-1", "volume_type": "l_ssd" },
				"2": { "id": "root-2", "volume_type": "sbs_volume" },
			},
		}))
		.unwrap();
		assert_eq!(block_volumes_of(&server), vec!["root-1", "root-2"]);
	}

	#[test]
	fn waits_out_a_detach_but_not_a_rejected_token() {
		use reqwest::StatusCode;
		assert!(retryable(StatusCode::CONFLICT));
		assert!(retryable(StatusCode::PRECONDITION_FAILED));
		assert!(retryable(StatusCode::TOO_MANY_REQUESTS));
		assert!(retryable(StatusCode::INTERNAL_SERVER_ERROR));
		assert!(!retryable(StatusCode::UNAUTHORIZED));
		assert!(!retryable(StatusCode::FORBIDDEN));
	}

	#[test]
	fn terminates_only_a_running_instance() {
		assert!(terminable(Some("running")));
		// cloud-init powers the guest off at lifetime_minutes, so every swept
		// Instance is stopped by the time destroy runs.
		assert!(!terminable(Some("stopped")));
		assert!(!terminable(Some("stopping")));
		assert!(!terminable(Some("starting")));
		assert!(!terminable(None));
	}

	#[test]
	fn reads_a_listed_instance_without_its_optional_fields() {
		let server: Server =
			serde_json::from_value(json!({ "id": "abc-123" })).unwrap();
		assert_eq!(server.name, "");
		assert!(server.creation_date.is_none());
		assert!(block_volumes_of(&server).is_empty());
	}
}
