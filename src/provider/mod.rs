mod cherry;
mod hetzner;
mod vultr;

use std::collections::HashMap;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use anyhow::{bail, Context, Result};

use crate::config::{Label, Provider as Kind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
	pub id: String,
	pub name: String,
	pub created_at: Option<OffsetDateTime>,
}

pub struct Placement {
	pub plan: String,
	pub location: String,
}

#[allow(async_fn_in_trait)]
pub trait Cloud {
	async fn create(
		&self,
		name: &str,
		plan: &str,
		location: &str,
		image: &str,
		ssh_key: Option<&str>,
		user_data: &str,
	) -> Result<Machine>;
	async fn destroy(&self, id: &str) -> Result<()>;
	async fn list(&self, prefix: &str) -> Result<Vec<Machine>>;
}

pub enum Backend {
	Cherry(cherry::Cherry),
	Hetzner(hetzner::Hetzner),
	Vultr(vultr::Vultr),
}

impl Backend {
	pub async fn create(
		&self,
		name: &str,
		plan: &str,
		location: &str,
		image: &str,
		ssh_key: Option<&str>,
		user_data: &str,
	) -> Result<Machine> {
		match self {
			Self::Cherry(c) => {
				c.create(name, plan, location, image, ssh_key, user_data)
					.await
			}
			Self::Hetzner(c) => {
				c.create(name, plan, location, image, ssh_key, user_data)
					.await
			}
			Self::Vultr(c) => {
				c.create(name, plan, location, image, ssh_key, user_data)
					.await
			}
		}
	}

	pub async fn destroy(&self, id: &str) -> Result<()> {
		match self {
			Self::Cherry(c) => c.destroy(id).await,
			Self::Hetzner(c) => c.destroy(id).await,
			Self::Vultr(c) => c.destroy(id).await,
		}
	}

	pub async fn list(&self, prefix: &str) -> Result<Vec<Machine>> {
		match self {
			Self::Cherry(c) => c.list(prefix).await,
			Self::Hetzner(c) => c.list(prefix).await,
			Self::Vultr(c) => c.list(prefix).await,
		}
	}
}

#[allow(async_fn_in_trait)]
pub trait Fleet {
	fn kinds(&self) -> Vec<Kind>;
	async fn list(&self, kind: Kind, prefix: &str) -> Result<Vec<Machine>>;
	#[allow(clippy::too_many_arguments)]
	async fn create(
		&self,
		kind: Kind,
		name: &str,
		plan: &str,
		location: &str,
		image: &str,
		ssh_key: Option<&str>,
		user_data: &str,
	) -> Result<Machine>;
	async fn destroy(&self, kind: Kind, id: &str) -> Result<()>;
}

impl Fleet for Clouds {
	fn kinds(&self) -> Vec<Kind> {
		self.backends.keys().copied().collect()
	}
	async fn list(&self, kind: Kind, prefix: &str) -> Result<Vec<Machine>> {
		self.get(kind)?.list(prefix).await
	}
	async fn create(
		&self,
		kind: Kind,
		name: &str,
		plan: &str,
		location: &str,
		image: &str,
		ssh_key: Option<&str>,
		user_data: &str,
	) -> Result<Machine> {
		self.get(kind)?
			.create(name, plan, location, image, ssh_key, user_data)
			.await
	}
	async fn destroy(&self, kind: Kind, id: &str) -> Result<()> {
		self.get(kind)?.destroy(id).await
	}
}

pub struct Clouds {
	backends: HashMap<Kind, Backend>,
}

impl Clouds {
	pub fn from_env(labels: &[Label]) -> Result<Self> {
		let mut backends = HashMap::new();
		for kind in labels.iter().map(|label| label.provider) {
			if backends.contains_key(&kind) {
				continue;
			}
			let backend = match kind {
				Kind::Cherry => Backend::Cherry(cherry::Cherry::from_env()?),
				Kind::Hetzner => {
					Backend::Hetzner(hetzner::Hetzner::from_env()?)
				}
				Kind::Vultr => Backend::Vultr(vultr::Vultr::from_env()?),
			};
			backends.insert(kind, backend);
		}
		Ok(Self { backends })
	}

	pub fn get(&self, kind: Kind) -> Result<&Backend> {
		self.backends.get(&kind).ok_or_else(|| {
			anyhow::anyhow!("no credentials loaded for {kind:?}")
		})
	}
}

pub fn placements(label: &Label) -> impl Iterator<Item = Placement> + '_ {
	label.plans.iter().flat_map(move |plan| {
		label.locations.iter().map(move |location| Placement {
			plan: plan.clone(),
			location: location.clone(),
		})
	})
}

fn http_client() -> Result<reqwest::Client> {
	Ok(reqwest::Client::builder()
		.timeout(std::time::Duration::from_secs(60))
		.build()?)
}

fn base64(value: &str) -> String {
	use base64::Engine;
	base64::engine::general_purpose::STANDARD.encode(value)
}

async fn decode<T: serde::de::DeserializeOwned>(
	response: reqwest::Response,
	what: &str,
) -> Result<T> {
	let status = response.status();
	let body = response.text().await.unwrap_or_default();
	if !status.is_success() {
		bail!("{what}: HTTP {status}: {}", clip(&body));
	}
	serde_json::from_str(&body)
		.with_context(|| format!("{what}: decoding {}", clip(&body)))
}

async fn expect_gone(response: reqwest::Response, what: &str) -> Result<()> {
	let status = response.status();
	if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
		return Ok(());
	}
	bail!(
		"{what}: HTTP {status}: {}",
		clip(&response.text().await.unwrap_or_default())
	)
}

fn clip(body: &str) -> String {
	body.chars().take(200).collect()
}

pub fn timestamp(value: &str) -> Option<OffsetDateTime> {
	OffsetDateTime::parse(value, &Rfc3339).ok()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_each_provider_timestamp_format() {
		for value in [
			"2026-07-28T22:31:00+00:00",
			"2026-07-28T22:31:00Z",
			"2026-07-28T22:31:00.123456Z",
		] {
			assert!(timestamp(value).is_some(), "{value}");
		}
		assert!(timestamp("").is_none());
		assert!(timestamp("not a time").is_none());
	}

	use crate::config::Provider;

	fn label() -> Label {
		Label {
			labels: vec!["check".into()],
			provider: Provider::Hetzner,
			plans: vec!["cx43".into(), "cpx42".into()],
			locations: vec!["fsn1".into(), "nbg1".into()],
			image: "snapshot".into(),
			ssh_key: None,
			max_vms: 1,
			lifetime_minutes: 90,
			job_timeout_minutes: None,
			allow_fork_pull_request: true,
			allowed_events: vec!["pull_request".into()],
		}
	}

	#[test]
	fn walks_every_plan_against_every_location() {
		let label = label();
		let combos: Vec<_> = placements(&label)
			.map(|p| format!("{}/{}", p.plan, p.location))
			.collect();
		assert_eq!(
			combos,
			vec!["cx43/fsn1", "cx43/nbg1", "cpx42/fsn1", "cpx42/nbg1"]
		);
	}
}
