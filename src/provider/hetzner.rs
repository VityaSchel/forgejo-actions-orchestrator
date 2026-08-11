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
			"image": image_by_id_or_name(image),
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

/// Hetzner accepts an image by numeric id or by name, and rejects a numeric id sent as a string.
fn image_by_id_or_name(image: &str) -> Value {
	match image.parse::<i64>() {
		Ok(id) => json!(id),
		Err(_) => json!(image),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn sends_a_numeric_image_as_a_number() {
		assert_eq!(image_by_id_or_name("12345"), json!(12345));
	}

	#[test]
	fn sends_a_named_image_as_a_string() {
		assert_eq!(image_by_id_or_name("debian-12"), json!("debian-12"));
	}
}
