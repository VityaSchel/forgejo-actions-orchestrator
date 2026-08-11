use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
	pub forgejo: Forgejo,
	pub daemon: Daemon,
	#[serde(rename = "repo", default)]
	pub repos: Vec<Repo>,
	#[serde(rename = "label", default)]
	pub labels: Vec<Label>,
	#[serde(default)]
	pub alert: Option<Alert>,
}

#[derive(Debug, Deserialize)]
pub struct Alert {
	pub webhook_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Forgejo {
	pub url: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq, Hash)]
pub struct Repo {
	pub owner: String,
	pub name: String,
}

impl std::fmt::Display for Repo {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}/{}", self.owner, self.name)
	}
}

#[derive(Debug, Deserialize)]
pub struct Daemon {
	#[serde(default = "default_poll_interval")]
	pub poll_interval_secs: u64,
	#[serde(default = "default_reconcile_grace")]
	pub reconcile_grace_secs: u64,
	pub runner_version: String,
	pub runner_sha256: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Label {
	pub labels: Vec<String>,
	pub provider: Provider,
	pub plans: Vec<String>,
	pub locations: Vec<String>,
	pub image: String,
	#[serde(default)]
	pub ssh_key: Option<String>,
	#[serde(default = "default_max_vms")]
	pub max_vms: usize,
	#[serde(default = "default_lifetime_minutes")]
	pub lifetime_minutes: u64,
	#[serde(default)]
	pub allow_fork_pull_request: bool,
	pub allowed_events: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
	Cherry,
	Hetzner,
	Vultr,
}

const HOSTNAME_LIMIT: usize = 63;

fn default_poll_interval() -> u64 {
	15
}

fn default_reconcile_grace() -> u64 {
	300
}

fn default_max_vms() -> usize {
	1
}

fn default_lifetime_minutes() -> u64 {
	90
}

impl Config {
	pub fn load(path: &Path) -> Result<Self> {
		let text = std::fs::read_to_string(path)
			.with_context(|| format!("reading {}", path.display()))?;
		Self::parse(&text)
	}

	pub fn parse(text: &str) -> Result<Self> {
		let config: Self = toml::from_str(text).context("parsing config")?;
		config.validate()?;
		Ok(config)
	}

	fn validate(&self) -> Result<()> {
		if self.repos.is_empty() {
			bail!("no [[repo]] entries: the orchestrator would watch nothing");
		}
		let mut repos = HashSet::new();
		for repo in &self.repos {
			if repo.owner.is_empty() || repo.name.is_empty() {
				bail!("every [[repo]] needs a non-empty owner and name");
			}
			if !repos.insert(repo) {
				bail!("duplicate repository {repo}");
			}
		}
		if self.labels.is_empty() {
			bail!("no [[label]] entries: the orchestrator would never provision anything");
		}
		if self.daemon.runner_version.is_empty()
			|| self.daemon.runner_sha256.is_empty()
		{
			bail!(
				"daemon.runner_version and daemon.runner_sha256 are required"
			);
		}
		let mut seen = HashSet::new();
		for label in &self.labels {
			if label.labels.is_empty()
				|| label.labels.iter().any(|one| one.is_empty())
			{
				bail!("every [[label]] needs a non-empty labels list");
			}
			if !seen.insert(label.name()) {
				bail!("duplicate label set {:?}", label.labels);
			}
			if label.plans.is_empty() || label.locations.is_empty() {
				bail!(
					"label {} needs at least one plan and one location",
					label.name()
				);
			}
			if label.allowed_events.is_empty() {
				bail!(
					"label {} has no allowed_events, so nothing could ever use it",
					label.name()
				);
			}
			if let Some(other) =
				self.labels.iter().map(Label::name).find(|other| {
					*other != label.name() && other.starts_with(&label.name())
				}) {
				bail!(
					"label {} is a prefix of label {other}: a machine name could not be attributed to one of them",
					label.name()
				);
			}
			if label.max_vms == 0 {
				bail!("label {} has max_vms = 0", label.name());
			}
			let longest =
				crate::naming::machine_name(&label.name(), &"0".repeat(36));
			if longest.len() > HOSTNAME_LIMIT {
				bail!(
                    "label {} makes a {}-character machine name, over the {HOSTNAME_LIMIT} hostname limit",
                    label.name(),
                    longest.len()
                );
			}
		}
		Ok(())
	}

	pub fn label(&self, name: &str) -> Option<&Label> {
		self.labels.iter().find(|label| label.name() == name)
	}

	pub fn label_for(&self, runs_on: &[String]) -> Option<&Label> {
		self.labels
			.iter()
			.find(|label| label_set(&label.labels) == label_set(runs_on))
	}

	pub fn all_labels(&self) -> Vec<String> {
		let mut all: Vec<String> = self
			.labels
			.iter()
			.flat_map(|label| label.labels.iter().cloned())
			.collect();
		all.sort();
		all.dedup();
		all
	}

	pub fn entry_names(&self) -> Vec<String> {
		self.labels.iter().map(Label::name).collect()
	}

	pub fn poll_interval(&self) -> Duration {
		Duration::from_secs(self.daemon.poll_interval_secs)
	}

	pub fn reconcile_grace(&self) -> Duration {
		Duration::from_secs(self.daemon.reconcile_grace_secs)
	}
}

impl Label {
	pub fn name(&self) -> String {
		label_set(&self.labels).join("-")
	}
}

fn label_set(labels: &[String]) -> Vec<&str> {
	let mut set: Vec<&str> = labels.iter().map(String::as_str).collect();
	set.sort_unstable();
	set.dedup();
	set
}

#[cfg(test)]
mod tests {
	use super::*;

	const VALID: &str = include_str!("../fixtures/config.toml");
	const EXAMPLE: &str = include_str!("../config.example.toml");

	mod invalid {
		pub const NO_REPOS: &str =
			include_str!("../fixtures/invalid/no-repos.toml");
		pub const DUPLICATE_REPO: &str =
			include_str!("../fixtures/invalid/duplicate-repo.toml");
		pub const EMPTY_REPO_NAME: &str =
			include_str!("../fixtures/invalid/empty-repo-name.toml");
		pub const NO_LABELS: &str =
			include_str!("../fixtures/invalid/no-labels.toml");
		pub const DUPLICATE_LABEL: &str =
			include_str!("../fixtures/invalid/duplicate-label.toml");
		pub const NO_ALLOWED_EVENTS: &str =
			include_str!("../fixtures/invalid/no-allowed-events.toml");
		pub const NO_PLANS: &str =
			include_str!("../fixtures/invalid/no-plans.toml");
		pub const NO_LOCATIONS: &str =
			include_str!("../fixtures/invalid/no-locations.toml");
		pub const PREFIX_LABEL: &str =
			include_str!("../fixtures/invalid/prefix-label.toml");
		pub const ZERO_MAX_VMS: &str =
			include_str!("../fixtures/invalid/zero-max-vms.toml");
		pub const UNKNOWN_PROVIDER: &str =
			include_str!("../fixtures/invalid/unknown-provider.toml");
		pub const LABEL_NAME_TOO_LONG: &str =
			include_str!("../fixtures/invalid/label-name-too-long.toml");
		pub const NO_RUNNER_VERSION: &str =
			include_str!("../fixtures/invalid/no-runner-version.toml");
	}

	fn rejects(text: &str, expected: &str) {
		let error = Config::parse(text)
			.expect_err("config should have been rejected")
			.to_string();
		assert!(
			error.contains(expected),
			"expected {expected:?}, got {error:?}"
		);
	}

	#[test]
	fn parses_a_valid_config() {
		let config = Config::parse(VALID).unwrap();
		assert_eq!(
			config.repos.iter().map(Repo::to_string).collect::<Vec<_>>(),
			vec!["acme/widgets", "acme/gadgets"]
		);
		assert_eq!(
			config.entry_names(),
			vec!["check", "builder-cherry", "roomy"]
		);
		assert_eq!(config.poll_interval(), Duration::from_secs(15));
		assert!(config.label("check").unwrap().allow_fork_pull_request);
		assert!(
			!config
				.label("builder-cherry")
				.unwrap()
				.allow_fork_pull_request
		);
	}

	#[test]
	fn defaults_max_vms_to_one() {
		let config = Config::parse(VALID).unwrap();
		assert_eq!(config.label("check").unwrap().max_vms, 1);
	}

	#[test]
	fn the_shipped_example_is_valid() {
		Config::parse(EXAMPLE).unwrap();
	}

	#[test]
	fn rejects_a_config_with_no_repositories() {
		rejects(invalid::NO_REPOS, "no [[repo]] entries");
	}

	#[test]
	fn rejects_a_repository_listed_twice() {
		rejects(invalid::DUPLICATE_REPO, "duplicate repository acme/widgets");
	}

	#[test]
	fn rejects_a_repository_with_an_empty_name() {
		rejects(invalid::EMPTY_REPO_NAME, "non-empty owner and name");
	}

	#[test]
	fn rejects_an_empty_label_table() {
		rejects(invalid::NO_LABELS, "no [[label]] entries");
	}

	#[test]
	fn rejects_duplicate_labels() {
		rejects(invalid::DUPLICATE_LABEL, "duplicate label set");
	}

	#[test]
	fn rejects_a_label_with_no_allowed_events() {
		rejects(invalid::NO_ALLOWED_EVENTS, "no allowed_events");
	}

	#[test]
	fn rejects_a_label_with_no_plans() {
		rejects(invalid::NO_PLANS, "at least one plan and one location");
	}

	#[test]
	fn rejects_a_label_with_no_locations() {
		rejects(invalid::NO_LOCATIONS, "at least one plan and one location");
	}

	#[test]
	fn matches_a_label_set_in_any_order() {
		let config = Config::parse(VALID).unwrap();
		let matched = config
			.label_for(&["cherry".into(), "builder".into()])
			.expect("order must not matter");
		assert_eq!(matched.name(), "builder-cherry");
	}

	#[test]
	fn does_not_match_a_subset_or_a_superset() {
		let config = Config::parse(VALID).unwrap();
		assert!(config.label_for(&["builder".into()]).is_none());
		assert!(config
			.label_for(&["builder".into(), "cherry".into(), "extra".into()])
			.is_none());
	}

	#[test]
	fn polls_for_the_union_of_every_label() {
		let config = Config::parse(VALID).unwrap();
		assert_eq!(
			config.all_labels(),
			vec!["builder", "check", "cherry", "roomy"]
		);
	}

	#[test]
	fn rejects_a_label_that_prefixes_another() {
		rejects(invalid::PREFIX_LABEL, "is a prefix of label");
	}

	#[test]
	fn rejects_a_label_that_can_hold_no_machines() {
		rejects(invalid::ZERO_MAX_VMS, "max_vms = 0");
	}

	#[test]
	fn rejects_an_unknown_provider() {
		rejects(invalid::UNKNOWN_PROVIDER, "parsing config");
	}

	#[test]
	fn rejects_a_label_whose_machine_name_would_not_fit() {
		rejects(invalid::LABEL_NAME_TOO_LONG, "hostname limit");
	}

	#[test]
	fn rejects_a_missing_runner_version() {
		rejects(invalid::NO_RUNNER_VERSION, "runner_version");
	}
}
