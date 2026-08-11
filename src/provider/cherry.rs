use time::OffsetDateTime;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

use super::{Cloud, Machine};
use crate::secret;

const API: &str = "https://api.cherryservers.com/v1";

pub struct Cherry {
	http: reqwest::Client,
	token: String,
	project_id: String,
}

#[derive(Deserialize)]
struct Server {
	id: i64,
	#[serde(default)]
	hostname: String,
	#[serde(default)]
	created_at: Option<String>,
}

impl Cherry {
	pub fn from_env() -> Result<Self> {
		Ok(Self {
			http: super::http_client()?,
			token: secret::load("CHERRY_TOKEN")?,
			project_id: secret::load("CHERRY_PROJECT_ID")?,
		})
	}

	fn auth(&self) -> String {
		format!("Bearer {}", self.token)
	}
}

impl Cloud for Cherry {
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
			"hostname": name,
			"plan": plan,
			"region": location,
			"image": image,
			"user_data": super::base64(user_data),
		});
		if let Some(key) = ssh_key {
			body["ssh_keys"] = json!([key]);
		}
		let response = self
			.http
			.post(format!("{API}/projects/{}/servers", self.project_id))
			.header("Authorization", self.auth())
			.json(&body)
			.send()
			.await
			.context("cherry: creating server")?;
		let server: Server =
			super::decode(response, "cherry: creating server").await?;
		Ok(Machine {
			id: server.id.to_string(),
			name: name.to_owned(),
			created_at: Some(OffsetDateTime::now_utc()),
		})
	}

	async fn destroy(&self, id: &str) -> Result<()> {
		let response = self
			.http
			.delete(format!("{API}/servers/{id}"))
			.header("Authorization", self.auth())
			.send()
			.await
			.context("cherry: deleting server")?;
		super::expect_gone(response, "cherry: deleting server").await
	}

	async fn list(&self, prefix: &str) -> Result<Vec<Machine>> {
		let response = self
			.http
			.get(format!("{API}/projects/{}/servers", self.project_id))
			.header("Authorization", self.auth())
			.send()
			.await
			.context("cherry: listing servers")?;
		let servers: Vec<Server> =
			super::decode(response, "cherry: listing servers").await?;
		Ok(servers
			.into_iter()
			.filter(|server| server.hostname.starts_with(prefix))
			.map(|server| Machine {
				id: server.id.to_string(),
				name: server.hostname,
				created_at: server
					.created_at
					.as_deref()
					.and_then(super::timestamp),
			})
			.collect())
	}
}
