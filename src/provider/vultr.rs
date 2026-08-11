use time::OffsetDateTime;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

use super::{Cloud, Machine};
use crate::secret;

const API: &str = "https://api.vultr.com/v2";

pub struct Vultr {
	http: reqwest::Client,
	token: String,
}

#[derive(Deserialize)]
struct Created {
	instance: Instance,
}

#[derive(Deserialize)]
struct Listed {
	instances: Vec<Instance>,
}

#[derive(Deserialize)]
struct Instance {
	id: String,
	#[serde(default)]
	label: String,
	#[serde(default)]
	date_created: Option<String>,
}

impl Vultr {
	pub fn from_env() -> Result<Self> {
		Ok(Self {
			http: super::http_client()?,
			token: secret::load("VULTR_TOKEN")?,
		})
	}

	fn auth(&self) -> String {
		format!("Bearer {}", self.token)
	}
}

impl Cloud for Vultr {
	async fn create(
		&self,
		name: &str,
		plan: &str,
		location: &str,
		image: &str,
		ssh_key: Option<&str>,
		user_data: &str,
	) -> Result<Machine> {
		let os_id: i64 = image.parse().with_context(|| {
			format!("vultr: image must be a numeric os_id, got {image:?}")
		})?;
		let mut body = json!({
			"region": location,
			"plan": plan,
			"os_id": os_id,
			"label": name,
			"hostname": name,
			"user_data": super::base64(user_data),
			"activation_email": false,
		});
		if let Some(key) = ssh_key {
			body["sshkey_id"] = json!([key]);
		}
		let response = self
			.http
			.post(format!("{API}/instances"))
			.header("Authorization", self.auth())
			.json(&body)
			.send()
			.await
			.context("vultr: creating instance")?;
		let created: Created =
			super::decode(response, "vultr: creating instance").await?;
		Ok(Machine {
			id: created.instance.id,
			name: name.to_owned(),
			created_at: Some(OffsetDateTime::now_utc()),
		})
	}

	async fn destroy(&self, id: &str) -> Result<()> {
		let response = self
			.http
			.delete(format!("{API}/instances/{id}"))
			.header("Authorization", self.auth())
			.send()
			.await
			.context("vultr: deleting instance")?;
		super::expect_gone(response, "vultr: deleting instance").await
	}

	async fn list(&self, prefix: &str) -> Result<Vec<Machine>> {
		let response = self
			.http
			.get(format!("{API}/instances"))
			.query(&[("per_page", "500")])
			.header("Authorization", self.auth())
			.send()
			.await
			.context("vultr: listing instances")?;
		let listed: Listed =
			super::decode(response, "vultr: listing instances").await?;
		Ok(listed
			.instances
			.into_iter()
			.filter(|instance| instance.label.starts_with(prefix))
			.map(|instance| Machine {
				id: instance.id,
				name: instance.label,
				created_at: instance
					.date_created
					.as_deref()
					.and_then(super::timestamp),
			})
			.collect())
	}
}
