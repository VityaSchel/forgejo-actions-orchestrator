use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use time::OffsetDateTime;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Cloud, Machine};
use crate::secret;

const API: &str = "https://api.gcore.com/cloud/v1";
/// Only instance creation moved to v2; v1 still owns list, delete and tasks:
/// https://docs.gcore.com/api-reference/cloud/instances/create-instance
const CREATE_API: &str = "https://api.gcore.com/cloud/v2";

const BOOT_VOLUME_GB: u32 = 80;
/// Each region sells a different subset and 400s on the rest; Tokyo's create
/// offered only the first two: https://docs.gcore.com/api-reference/cloud/regions/get-region
const BOOT_VOLUME_TYPES: [&str; 3] =
	["ssd_lowlatency", "ssd_hiiops", "standard"];
const PAGE_LIMIT: u32 = 1000;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const CREATE_TIMEOUT: Duration = Duration::from_secs(600);

pub struct Gcore {
	http: reqwest::Client,
	token: String,
	project_id: String,
	locations: Vec<String>,
	volume_types: Mutex<HashMap<String, &'static str>>,
}

#[derive(Deserialize)]
struct Tasks {
	tasks: Vec<String>,
}

#[derive(Deserialize)]
struct Task {
	#[serde(default)]
	state: String,
	#[serde(default)]
	error: Option<String>,
	#[serde(default)]
	created_resources: Option<Created>,
}

#[derive(Deserialize)]
struct Created {
	#[serde(default)]
	instances: Vec<String>,
}

#[derive(Deserialize)]
struct Listed {
	results: Vec<Instance>,
}

/// Older v1 payloads name these instance_*: https://docs.gcore.com/api-reference/cloud/instances/list-instances
#[derive(Deserialize)]
struct Instance {
	#[serde(alias = "instance_id")]
	id: String,
	#[serde(default, alias = "instance_name")]
	name: String,
	#[serde(default, alias = "instance_created")]
	created_at: Option<String>,
	#[serde(default)]
	volumes: Vec<Volume>,
}

#[derive(Deserialize)]
struct Volume {
	id: String,
}

#[derive(Deserialize)]
struct Region {
	#[serde(default)]
	available_volume_types: Option<Vec<String>>,
}

impl Gcore {
	pub fn from_env(locations: &[String]) -> Result<Self> {
		Ok(Self {
			http: super::http_client()?,
			token: secret::load("GCORE_TOKEN")?,
			project_id: secret::load("GCORE_PROJECT_ID")?,
			locations: locations.to_vec(),
			volume_types: Mutex::new(HashMap::new()),
		})
	}

	fn auth(&self) -> String {
		format!("APIKey {}", self.token)
	}

	fn scope(&self, region: &str) -> String {
		format!("{API}/instances/{}/{region}", self.project_id)
	}

	fn create_scope(&self, region: &str) -> String {
		format!("{CREATE_API}/instances/{}/{region}", self.project_id)
	}

	fn cached_volume_type(&self, region: &str) -> Option<&'static str> {
		self.volume_types.lock().ok()?.get(region).copied()
	}

	/// available_volume_types stays null unless show_volume_types is set: https://docs.gcore.com/api-reference/cloud/regions/get-region
	async fn volume_type(&self, region: &str) -> Result<&'static str> {
		if let Some(cached) = self.cached_volume_type(region) {
			return Ok(cached);
		}
		let response = self
			.http
			.get(format!("{API}/regions/{region}"))
			.query(&[("show_volume_types", "true")])
			.header("Authorization", self.auth())
			.send()
			.await
			.context("gcore: reading region volume types")?;
		let found: Region =
			super::decode(response, "gcore: reading region volume types")
				.await?;
		let offered = found.available_volume_types.unwrap_or_default();
		let chosen = preferred_volume_type(&offered).with_context(|| {
			format!(
				"gcore: region {region} offers none of {}, only {}",
				BOOT_VOLUME_TYPES.join(", "),
				offered.join(", ")
			)
		})?;
		if let Ok(mut cache) = self.volume_types.lock() {
			cache.insert(region.to_owned(), chosen);
		}
		Ok(chosen)
	}

	async fn wait_for_instance(&self, task: &str) -> Result<String> {
		let deadline = Instant::now() + CREATE_TIMEOUT;
		loop {
			let response = self
				.http
				.get(format!("{API}/tasks/{task}"))
				.header("Authorization", self.auth())
				.send()
				.await
				.context("gcore: polling create task")?;
			let polled: Task =
				super::decode(response, "gcore: polling create task").await?;
			match polled.state.as_str() {
				"FINISHED" => return instance_of(polled, task),
				"ERROR" => bail!(
					"gcore: create task {task} failed: {}",
					polled.error.as_deref().unwrap_or("no detail")
				),
				_ => {}
			}
			if Instant::now() >= deadline {
				bail!(
					"gcore: create task {task} unfinished after {}s",
					CREATE_TIMEOUT.as_secs()
				);
			}
			tokio::time::sleep(POLL_INTERVAL).await;
		}
	}

	/// Delete keeps every volume not named in ?volumes=: https://docs.gcore.com/api-reference/cloud/instances/delete-instance
	async fn volume_ids(
		&self,
		region: &str,
		instance: &str,
	) -> Result<Vec<String>> {
		let response = self
			.http
			.get(format!("{}/{instance}", self.scope(region)))
			.header("Authorization", self.auth())
			.send()
			.await
			.context("gcore: reading instance volumes")?;
		if response.status() == reqwest::StatusCode::NOT_FOUND {
			return Ok(Vec::new());
		}
		let found: Instance =
			super::decode(response, "gcore: reading instance volumes").await?;
		Ok(found.volumes.into_iter().map(|volume| volume.id).collect())
	}
}

impl Cloud for Gcore {
	async fn create(
		&self,
		name: &str,
		plan: &str,
		location: &str,
		image: &str,
		ssh_key: Option<&str>,
		user_data: &str,
	) -> Result<Machine> {
		let volume_type = self.volume_type(location).await?;
		let response = self
			.http
			.post(self.create_scope(location))
			.header("Authorization", self.auth())
			.json(&create_body(
				name,
				plan,
				image,
				volume_type,
				ssh_key,
				user_data,
			))
			.send()
			.await
			.context("gcore: creating instance")?;
		let accepted: Tasks =
			super::decode(response, "gcore: creating instance").await?;
		let task = accepted
			.tasks
			.into_iter()
			.next()
			.context("gcore: creating instance returned no task")?;
		let instance = self.wait_for_instance(&task).await?;
		Ok(Machine {
			id: machine_id(location, &instance),
			name: name.to_owned(),
			created_at: Some(OffsetDateTime::now_utc()),
		})
	}

	async fn destroy(&self, id: &str) -> Result<()> {
		let (region, instance) = split_id(id)?;
		let volumes = self.volume_ids(region, instance).await?;
		let mut request = self
			.http
			.delete(format!("{}/{instance}", self.scope(region)))
			.header("Authorization", self.auth())
			.query(&[("delete_floatings", "true")]);
		if !volumes.is_empty() {
			request = request.query(&[("volumes", volumes.join(","))]);
		}
		let response =
			request.send().await.context("gcore: deleting instance")?;
		super::expect_gone(response, "gcore: deleting instance").await
	}

	async fn list(&self, prefix: &str) -> Result<Vec<Machine>> {
		let mut machines = Vec::new();
		for region in &self.locations {
			let response = self
				.http
				.get(self.scope(region))
				.query(&[("limit", PAGE_LIMIT.to_string())])
				.header("Authorization", self.auth())
				.send()
				.await
				.context("gcore: listing instances")?;
			let listed: Listed =
				super::decode(response, "gcore: listing instances").await?;
			machines.extend(
				listed
					.results
					.into_iter()
					.filter(|found| found.name.starts_with(prefix))
					.map(|found| Machine {
						id: machine_id(region, &found.id),
						name: found.name,
						created_at: found
							.created_at
							.as_deref()
							.and_then(super::timestamp),
					}),
			);
		}
		Ok(machines)
	}
}

/// A password makes Linux ignore user_data: https://docs.gcore.com/api-reference/cloud/instances/create-instance
fn create_body(
	name: &str,
	plan: &str,
	image: &str,
	volume_type: &str,
	ssh_key: Option<&str>,
	user_data: &str,
) -> Value {
	let mut body = json!({
		"flavor": plan,
		"name": name,
		"interfaces": [{ "type": "external", "ip_family": "ipv4" }],
		"volumes": [{
			"source": "image",
			"boot_index": 0,
			"image_id": image,
			"size": BOOT_VOLUME_GB,
			"type_name": volume_type,
			"delete_on_termination": true,
		}],
		"user_data": super::base64(user_data),
	});
	if let Some(key) = ssh_key {
		body["ssh_key_name"] = json!(key);
	}
	body
}

fn preferred_volume_type(offered: &[String]) -> Option<&'static str> {
	BOOT_VOLUME_TYPES
		.into_iter()
		.find(|wanted| offered.iter().any(|found| found == wanted))
}

fn machine_id(region: &str, instance: &str) -> String {
	format!("{region}/{instance}")
}

fn split_id(id: &str) -> Result<(&str, &str)> {
	id.split_once('/').ok_or_else(|| {
		anyhow!("gcore: instance id {id:?} is not <region>/<uuid>")
	})
}

fn instance_of(task: Task, id: &str) -> Result<String> {
	task.created_resources
		.and_then(|created| created.instances.into_iter().next())
		.ok_or_else(|| anyhow!("gcore: create task {id} created no instance"))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn body() -> Value {
		create_body(
			"runner-1",
			"a1-standard-8-16",
			"d52b7e87-50f2-4ad0-a5f8-8b9f80591384",
			"ssd_hiiops",
			None,
			"#cloud-config\n",
		)
	}

	fn offered(types: &[&str]) -> Vec<String> {
		types.iter().map(|found| found.to_string()).collect()
	}

	#[test]
	fn splits_a_composite_id_into_region_and_instance() {
		let (region, instance) = split_id("30/abc-123").unwrap();
		assert_eq!(region, "30");
		assert_eq!(instance, "abc-123");
	}

	#[test]
	fn splits_only_on_the_first_separator() {
		assert_eq!(split_id("30/a/b").unwrap(), ("30", "a/b"));
	}

	#[test]
	fn rejects_an_id_that_carries_no_region() {
		let error = split_id("abc-123").unwrap_err().to_string();
		assert!(error.contains("<region>/<uuid>"), "{error}");
	}

	#[test]
	fn builds_the_composite_id_create_and_list_share() {
		let built = machine_id("30", "abc-123");
		assert_eq!(built, "30/abc-123");
		assert_eq!(split_id(&built).unwrap(), ("30", "abc-123"));
	}

	#[test]
	fn boots_from_the_image_and_frees_the_volume_on_delete() {
		let volume = body()["volumes"][0].clone();
		assert_eq!(volume["source"], "image");
		assert_eq!(volume["image_id"], "d52b7e87-50f2-4ad0-a5f8-8b9f80591384");
		assert_eq!(volume["boot_index"], 0);
		assert_eq!(volume["size"], 80);
		assert_eq!(volume["type_name"], "ssd_hiiops");
		assert_eq!(volume["delete_on_termination"], true);
	}

	#[test]
	fn asks_for_the_volume_type_the_region_was_polled_for() {
		let tokyo = create_body(
			"runner-1",
			"a1-standard-8-16",
			"d52b7e87-50f2-4ad0-a5f8-8b9f80591384",
			"ssd_lowlatency",
			None,
			"#cloud-config\n",
		);
		assert_eq!(tokyo["volumes"][0]["type_name"], "ssd_lowlatency");
		assert_eq!(tokyo["volumes"][0]["size"], 80);
		assert_eq!(tokyo["volumes"][0]["delete_on_termination"], true);
	}

	#[test]
	fn takes_the_fastest_volume_type_a_region_offers() {
		assert_eq!(
			preferred_volume_type(&offered(&["standard", "ssd_hiiops"])),
			Some("ssd_hiiops")
		);
		assert_eq!(
			preferred_volume_type(&offered(&["ssd_lowlatency", "standard"])),
			Some("ssd_lowlatency")
		);
		assert_eq!(
			preferred_volume_type(&offered(&["standard"])),
			Some("standard")
		);
	}

	#[test]
	fn refuses_a_region_that_offers_no_type_we_want() {
		assert_eq!(preferred_volume_type(&offered(&["cold", "ultra"])), None);
		assert_eq!(preferred_volume_type(&[]), None);
	}

	#[test]
	fn reads_the_volume_types_a_region_reports() {
		let tokyo: Region = serde_json::from_value(json!({
			"id": 30,
			"display_name": "Tokyo",
			"available_volume_types": ["ssd_lowlatency", "standard"],
		}))
		.unwrap();
		assert_eq!(
			preferred_volume_type(&tokyo.available_volume_types.unwrap()),
			Some("ssd_lowlatency")
		);
	}

	#[test]
	fn treats_an_unpolled_region_as_offering_nothing() {
		let unpolled: Region = serde_json::from_value(json!({
			"id": 30,
			"available_volume_types": null,
		}))
		.unwrap();
		assert!(unpolled.available_volume_types.is_none());
		let absent: Region =
			serde_json::from_value(json!({ "id": 30 })).unwrap();
		assert!(absent.available_volume_types.is_none());
	}

	#[test]
	fn asks_for_a_single_internet_facing_interface() {
		assert_eq!(
			body()["interfaces"],
			json!([{ "type": "external", "ip_family": "ipv4" }])
		);
	}

	#[test]
	fn sends_user_data_base64_encoded_and_never_a_password() {
		let body = body();
		assert_eq!(
			body["user_data"],
			json!(super::super::base64("#cloud-config\n"))
		);
		assert!(body.get("password").is_none());
		assert!(body.get("username").is_none());
	}

	#[test]
	fn names_a_keypair_only_when_one_is_configured() {
		assert!(body().get("ssh_key_name").is_none());
		let keyed = create_body(
			"runner-1",
			"flavor",
			"image",
			"standard",
			Some("ci"),
			"user-data",
		);
		assert_eq!(keyed["ssh_key_name"], "ci");
	}

	/// CreateInstanceSerializerV2 takes a single `name`, not v1's `names` array.
	#[test]
	fn names_the_instance_with_the_v2_field() {
		let body = body();
		assert_eq!(body["name"], "runner-1");
		assert!(body.get("names").is_none());
	}

	#[test]
	fn reads_the_instance_id_out_of_a_finished_task() {
		let task: Task = serde_json::from_value(json!({
			"state": "FINISHED",
			"created_resources": { "instances": ["abc-123"] },
		}))
		.unwrap();
		assert_eq!(instance_of(task, "task-1").unwrap(), "abc-123");
	}

	#[test]
	fn fails_when_a_finished_task_created_no_instance() {
		let task: Task = serde_json::from_value(json!({
			"state": "FINISHED",
			"created_resources": null,
		}))
		.unwrap();
		let error = instance_of(task, "task-1").unwrap_err().to_string();
		assert!(error.contains("task-1"), "{error}");
	}

	#[test]
	fn reads_an_instance_in_either_documented_field_naming() {
		let current: Instance = serde_json::from_value(json!({
			"id": "abc-123",
			"name": "runner-1",
			"created_at": "2026-08-27T10:00:00Z",
		}))
		.unwrap();
		let legacy: Instance = serde_json::from_value(json!({
			"instance_id": "abc-123",
			"instance_name": "runner-1",
			"instance_created": "2026-08-27T10:00:00Z",
		}))
		.unwrap();
		assert_eq!(current.id, legacy.id);
		assert_eq!(current.name, legacy.name);
		assert_eq!(current.created_at, legacy.created_at);
	}
}
