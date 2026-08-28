use time::OffsetDateTime;

use anyhow::{Context, Result};
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

	async fn architecture_of(&self, plan: &str) -> Result<String> {
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
			.map(|server_type| server_type.architecture)
			.with_context(|| format!("hetzner: unknown server type {plan}"))
	}

	/// One image name covers both architectures under different ids, and the API
	/// picks neither: https://docs.hetzner.cloud/#images-get-all-images
	async fn image_for(&self, image: &str, plan: &str) -> Result<Value> {
		if let Ok(id) = image.parse::<i64>() {
			return Ok(json!(id));
		}
		let architecture = self.architecture_of(plan).await?;
		let response = self
			.http
			.get(format!("{API}/images"))
			.query(&[("name", image), ("architecture", &architecture)])
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
		let mut body = json!({
			"name": name,
			"server_type": plan,
			"location": location,
			"image": self.image_for(image, plan).await?,
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

	#[test]
	fn reads_the_id_of_a_resolved_image() {
		let listed: Images = serde_json::from_value(json!({
			"images": [{ "id": 114690389, "name": "debian-12" }],
		}))
		.unwrap();
		assert_eq!(listed.images[0].id, 114690389);
	}
}
