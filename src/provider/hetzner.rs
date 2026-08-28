use time::OffsetDateTime;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Cloud, Machine};
use crate::secret;

const API: &str = "https://api.hetzner.cloud/v1";

const PER_PAGE: u32 = 50;

pub struct Hetzner {
	http: reqwest::Client,
	token: String,
}

#[derive(Deserialize)]
struct Created {
	server: Server,
}

#[derive(Deserialize)]
struct Listed {
	servers: Vec<Server>,
	#[serde(default)]
	meta: Option<Meta>,
}

#[derive(Deserialize)]
struct Meta {
	pagination: Pagination,
}

#[derive(Deserialize)]
struct Pagination {
	#[serde(default)]
	next_page: Option<u32>,
}

#[derive(Deserialize)]
struct ServerTypes {
	server_types: Vec<ServerType>,
}

#[derive(Deserialize)]
struct ServerType {
	architecture: String,
	#[serde(default)]
	locations: Vec<TypeLocation>,
}

#[derive(Deserialize)]
struct TypeLocation {
	name: String,
	#[serde(default)]
	available: bool,
}

#[derive(Deserialize)]
struct Images {
	images: Vec<Image>,
}

#[derive(Deserialize)]
struct Image {
	id: i64,
}

#[derive(Deserialize)]
struct Server {
	id: i64,
	name: String,
	#[serde(default)]
	created: Option<String>,
}

impl Hetzner {
	pub fn from_env() -> Result<Self> {
		Ok(Self {
			http: super::http_client()?,
			token: secret::load("HETZNER_TOKEN")?,
		})
	}

	fn auth(&self) -> String {
		format!("Bearer {}", self.token)
	}

	async fn server_type(&self, plan: &str) -> Result<ServerType> {
		let response = self
			.http
			.get(format!("{API}/server_types"))
			.query(&[("name", plan)])
			.header("Authorization", self.auth())
			.send()
			.await
			.context("hetzner: reading server type")?;
		let listed: ServerTypes =
			super::decode(response, "hetzner: reading server type").await?;
		listed
			.server_types
			.into_iter()
			.next()
			.with_context(|| format!("hetzner: unknown server type {plan}"))
	}

	/// One image name covers both architectures under different ids, and the API
	/// picks neither: https://docs.hetzner.cloud/#images-get-all-images
	async fn image_for(
		&self,
		image: &str,
		architecture: &str,
	) -> Result<Value> {
		if let Ok(id) = image.parse::<i64>() {
			return Ok(json!(id));
		}
		let response = self
			.http
			.get(format!("{API}/images"))
			.query(&[("name", image), ("architecture", architecture)])
			.header("Authorization", self.auth())
			.send()
			.await
			.context("hetzner: resolving image")?;
		let listed: Images =
			super::decode(response, "hetzner: resolving image").await?;
		listed
			.images
			.into_iter()
			.next()
			.map(|found| json!(found.id))
			.with_context(|| {
				format!("hetzner: no {architecture} image named {image}")
			})
	}
}

impl Cloud for Hetzner {
	async fn create(
		&self,
		name: &str,
		plan: &str,
		location: &str,
		image: &str,
		ssh_key: Option<&str>,
		user_data: &str,
	) -> Result<Machine> {
		let server_type = self.server_type(plan).await?;
		if !offered_in(&server_type, location) {
			bail!("hetzner: {plan} is out of stock in {location}");
		}
		let image = self.image_for(image, &server_type.architecture).await?;
		let mut body = json!({
			"name": name,
			"server_type": plan,
			"location": location,
			"image": image,
			"user_data": user_data,
			"start_after_create": true,
		});
		if let Some(key) = ssh_key {
			body["ssh_keys"] = json!([key]);
		}
		let response = self
			.http
			.post(format!("{API}/servers"))
			.header("Authorization", self.auth())
			.json(&body)
			.send()
			.await
			.context("hetzner: creating server")?;
		let created: Created =
			super::decode(response, "hetzner: creating server").await?;
		Ok(Machine {
			id: created.server.id.to_string(),
			name: created.server.name,
			created_at: created
				.server
				.created
				.as_deref()
				.and_then(super::timestamp)
				.or_else(|| Some(OffsetDateTime::now_utc())),
		})
	}

	async fn destroy(&self, id: &str) -> Result<()> {
		let response = self
			.http
			.delete(format!("{API}/servers/{id}"))
			.header("Authorization", self.auth())
			.send()
			.await
			.context("hetzner: deleting server")?;
		super::expect_gone(response, "hetzner: deleting server").await
	}

	async fn list(&self, prefix: &str) -> Result<Vec<Machine>> {
		let mut machines = Vec::new();
		let mut page = 1;
		loop {
			let response = self
				.http
				.get(format!("{API}/servers"))
				.query(&[
					("per_page", PER_PAGE.to_string()),
					("page", page.to_string()),
				])
				.header("Authorization", self.auth())
				.send()
				.await
				.context("hetzner: listing servers")?;
			let listed: Listed =
				super::decode(response, "hetzner: listing servers").await?;
			machines.extend(
				listed
					.servers
					.into_iter()
					.filter(|server| server.name.starts_with(prefix))
					.map(|server| Machine {
						id: server.id.to_string(),
						name: server.name,
						created_at: server
							.created
							.as_deref()
							.and_then(super::timestamp),
					}),
			);
			match listed.meta.and_then(|meta| meta.pagination.next_page) {
				Some(next) => page = next,
				None => return Ok(machines),
			}
		}
	}
}

/// Only an explicit `available: false` blocks a placement: the field is absent on
/// older API versions, and an unlisted location is left for the API to judge.
fn offered_in(server_type: &ServerType, location: &str) -> bool {
	server_type
		.locations
		.iter()
		.find(|candidate| candidate.name == location)
		.is_none_or(|candidate| candidate.available)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reads_the_architecture_of_a_server_type() {
		let listed: ServerTypes = serde_json::from_value(json!({
			"server_types": [{ "name": "cax31", "architecture": "arm" }],
		}))
		.unwrap();
		assert_eq!(listed.server_types[0].architecture, "arm");
	}

	fn cax31(available: bool) -> ServerType {
		serde_json::from_value(json!({
			"architecture": "arm",
			"locations": [
				{ "name": "fsn1", "available": available },
				{ "name": "hel1", "available": false },
			],
		}))
		.unwrap()
	}

	#[test]
	fn refuses_a_location_the_api_reports_as_out_of_stock() {
		assert!(!offered_in(&cax31(false), "fsn1"));
		assert!(offered_in(&cax31(true), "fsn1"));
		assert!(!offered_in(&cax31(true), "hel1"));
	}

	#[test]
	fn lets_the_api_judge_a_location_it_never_listed() {
		assert!(offered_in(&cax31(false), "nbg1"));
		let unknown: ServerType =
			serde_json::from_value(json!({ "architecture": "x86" })).unwrap();
		assert!(offered_in(&unknown, "fsn1"));
	}

	#[test]
	fn reads_the_id_of_a_resolved_image() {
		let listed: Images = serde_json::from_value(json!({
			"images": [{ "id": 114690389, "name": "debian-12" }],
		}))
		.unwrap();
		assert_eq!(listed.images[0].id, 114690389);
	}
}
