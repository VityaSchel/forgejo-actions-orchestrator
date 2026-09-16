use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
	pub forgejo: Forgejo,
	pub daemon: Daemon,
	#[serde(rename = "provider", default)]
	pub providers: BTreeMap<Provider, Host>,
	#[serde(rename = "image", default)]
	pub images: BTreeMap<String, BTreeMap<Provider, String>>,
	#[serde(rename = "plans", default)]
	pub plans: BTreeMap<String, BTreeMap<Provider, Vec<String>>>,
	#[serde(rename = "machine", default)]
	pub machines: Vec<Spec>,
	#[serde(rename = "repo", default)]
	pub grants: BTreeMap<String, Grant>,
	#[serde(default)]
	pub alert: Option<Alert>,
	#[serde(skip)]
	pub repos: Vec<Repo>,
	#[serde(skip)]
	pub classes: Vec<Class>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alert {
	pub webhook_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forgejo {
	pub url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Daemon {
	#[serde(default = "default_poll_interval")]
	pub poll_interval_secs: u64,
	#[serde(default = "default_reconcile_grace")]
	pub reconcile_grace_secs: u64,
	pub runner_version: String,
	pub runner_sha256_amd64: String,
	pub runner_sha256_arm64: String,
	#[serde(default = "default_machine_prefix")]
	pub machine_prefix: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Host {
	pub locations: Vec<String>,
	#[serde(default)]
	pub ssh_key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
	pub labels: Labels,
	pub image: String,
	pub plans: String,
	pub lifetime_minutes: u64,
	pub job_timeout_minutes: u64,
	pub allowed_events: Vec<String>,
	#[serde(default)]
	pub allow_fork_pull_request: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged, expecting = "a label set, or a list of label sets")]
pub enum Labels {
	One(Vec<String>),
	Many(Vec<Vec<String>>),
}

impl Labels {
	fn sets(&self) -> Vec<&[String]> {
		match self {
			Self::One(set) => vec![set.as_slice()],
			Self::Many(sets) => sets.iter().map(Vec::as_slice).collect(),
		}
	}
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
	pub max_vms: BTreeMap<Provider, usize>,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq, Hash)]
pub struct Repo {
	pub owner: String,
	pub name: String,
}

impl fmt::Display for Repo {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}/{}", self.owner, self.name)
	}
}

/// One label set with everything it names already resolved
#[derive(Debug, Clone)]
pub struct Class {
	pub labels: Vec<String>,
	pub provider: Provider,
	pub plans: Vec<String>,
	pub locations: Vec<String>,
	pub image: String,
	pub ssh_key: Option<String>,
	pub lifetime_minutes: u64,
	pub job_timeout_minutes: u64,
	pub allowed_events: Vec<String>,
	pub allow_fork_pull_request: bool,
}

impl Class {
	pub fn name(&self) -> String {
		self.labels.join("-")
	}

	pub fn job_timeout(&self) -> u64 {
		self.job_timeout_minutes
	}

	/// Unordered and exact: a job naming fewer or more labels is not this one
	fn matches(&self, runs_on: &[String]) -> bool {
		runs_on.len() == self.labels.len()
			&& self.labels.iter().all(|label| runs_on.contains(label))
	}
}

#[derive(
	Debug, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
	Cherry,
	Gcore,
	Hetzner,
	Scaleway,
	Vultr,
}

impl Provider {
	pub const ALL: [Self; 5] = [
		Self::Cherry,
		Self::Gcore,
		Self::Hetzner,
		Self::Scaleway,
		Self::Vultr,
	];

	pub fn as_str(self) -> &'static str {
		match self {
			Self::Cherry => "cherry",
			Self::Gcore => "gcore",
			Self::Hetzner => "hetzner",
			Self::Scaleway => "scaleway",
			Self::Vultr => "vultr",
		}
	}
}

impl fmt::Display for Provider {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}

const HOSTNAME_LIMIT: usize = 63;

/// Both a token and the machine_prefix are copied into a machine name
fn is_hostname_label(name: &str) -> bool {
	!name.is_empty()
		&& name
			.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
		&& !name.ends_with('-')
		&& name
			.chars()
			.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn default_machine_prefix() -> String {
	crate::naming::DEFAULT_PREFIX.to_owned()
}

fn default_poll_interval() -> u64 {
	15
}

fn default_reconcile_grace() -> u64 {
	300
}

impl Config {
	pub fn load(path: &Path) -> Result<Self> {
		let text = std::fs::read_to_string(path)
			.with_context(|| format!("reading {}", path.display()))?;
		Self::parse(&text)
	}

	pub fn parse(text: &str) -> Result<Self> {
		let mut config: Self =
			toml::from_str(text).context("parsing config")?;
		config.repos = config.parse_repos()?;
		config.validate()?;
		config.classes = config.enumerate()?;
		config.validate_classes()?;
		Ok(config)
	}

	fn parse_repos(&self) -> Result<Vec<Repo>> {
		let mut repos = Vec::new();
		for key in self.grants.keys() {
			let Some((owner, name)) = key.split_once('/') else {
				bail!("[repo.{key:?}] must be named \"owner/name\"");
			};
			if owner.is_empty() || name.is_empty() || name.contains('/') {
				bail!("[repo.{key:?}] must be named \"owner/name\"");
			}
			repos.push(Repo {
				owner: owner.to_owned(),
				name: name.to_owned(),
			});
		}
		Ok(repos)
	}

	fn validate(&self) -> Result<()> {
		let prefix = self.machine_prefix();
		if !is_hostname_label(prefix) {
			bail!(
				"machine_prefix {prefix:?} must be a hostname label: lower case letters, digits and hyphens, starting with a letter or digit and not ending in a hyphen"
			);
		}
		if self.daemon.runner_version.is_empty()
			|| self.daemon.runner_sha256_amd64.is_empty()
			|| self.daemon.runner_sha256_arm64.is_empty()
		{
			bail!(
				"daemon.runner_version, daemon.runner_sha256_amd64 and \
				 daemon.runner_sha256_arm64 are required"
			);
		}
		if self.repos.is_empty() {
			bail!("no [repo] entries: the orchestrator would watch nothing");
		}
		if self.providers.is_empty() {
			bail!(
				"no [provider] entries: there is nowhere to create a machine"
			);
		}
		if self.machines.is_empty() {
			bail!("no [[machine]] entries: no job could ask for anything");
		}
		self.validate_providers()?;
		self.validate_images()?;
		self.validate_plans()?;
		self.validate_specs()?;
		self.validate_grants()
	}

	fn validate_providers(&self) -> Result<()> {
		for (kind, host) in &self.providers {
			if host.locations.is_empty() {
				bail!("[provider.{kind}] has no locations");
			}
			if host.locations.iter().any(String::is_empty) {
				bail!("[provider.{kind}] has an empty location");
			}
		}
		Ok(())
	}

	fn validate_images(&self) -> Result<()> {
		for (alias, ids) in &self.images {
			if ids.is_empty() {
				bail!("[image.{alias}] names no provider");
			}
			for (kind, id) in ids {
				self.declared(*kind, &format!("[image.{alias}]"))?;
				if id.is_empty() {
					bail!("[image.{alias}] has an empty id for {kind}");
				}
			}
		}
		Ok(())
	}

	fn validate_plans(&self) -> Result<()> {
		for (name, ladders) in &self.plans {
			if ladders.is_empty() {
				bail!("[plans.{name}] names no provider");
			}
			for (kind, plans) in ladders {
				self.declared(*kind, &format!("[plans.{name}]"))?;
				if plans.is_empty() {
					bail!("[plans.{name}] has no plans for {kind}");
				}
			}
		}
		Ok(())
	}

	fn validate_specs(&self) -> Result<()> {
		for spec in &self.machines {
			let whose = spec.whose();
			if spec.allowed_events.is_empty() {
				bail!(
					"{whose} has no allowed_events, so nothing could ever use it"
				);
			}
			if spec.job_timeout_minutes == 0
				|| spec.job_timeout_minutes >= spec.lifetime_minutes
			{
				bail!(
					"{whose} has job_timeout_minutes = {}, which must be between 1 and lifetime_minutes ({}) exclusive, or the machine is destroyed before the runner can fail the job cleanly",
					spec.job_timeout_minutes,
					spec.lifetime_minutes
				);
			}
		}
		Ok(())
	}

	fn validate_grants(&self) -> Result<()> {
		for (key, grant) in &self.grants {
			if grant.max_vms.is_empty() {
				bail!(
					"[repo.{key:?}] grants no provider, so it could never get a machine"
				);
			}
			for (kind, max) in &grant.max_vms {
				self.declared(*kind, &format!("[repo.{key:?}]"))?;
				if *max == 0 {
					bail!("[repo.{key:?}] has max_vms.{kind} = 0");
				}
			}
		}
		Ok(())
	}

	fn declared(&self, kind: Provider, whose: &str) -> Result<()> {
		if !self.providers.contains_key(&kind) {
			bail!("{whose} names {kind}, which has no [provider.{kind}] table");
		}
		Ok(())
	}

	fn enumerate(&self) -> Result<Vec<Class>> {
		let mut classes = Vec::new();
		for spec in &self.machines {
			for set in spec.labels.sets() {
				classes.push(self.resolve(spec, set)?);
			}
		}
		Ok(classes)
	}

	fn resolve(&self, spec: &Spec, set: &[String]) -> Result<Class> {
		let whose = format!("[[machine]] labels = [{}]", set.join(", "));
		if set.is_empty() {
			bail!(
				"a [[machine]] has an empty label set: no job could ask for it"
			);
		}
		for (at, label) in set.iter().enumerate() {
			if !is_hostname_label(label) {
				bail!(
					"{whose} carries {label:?}, which is not a hostname label: a label is copied into a machine name, so it takes lower case letters, digits and hyphens, starting with a letter or digit and not ending in a hyphen"
				);
			}
			if set[..at].contains(label) {
				bail!("{whose} names {label} twice");
			}
		}
		let provider = self.provider_of(set, &whose)?;
		let host = &self.providers[&provider];
		let Some(ids) = self.images.get(&spec.image) else {
			bail!(
				"{whose} wants image {:?}, which no [image] table defines",
				spec.image
			);
		};
		let Some(image) = ids.get(&provider) else {
			bail!(
				"{whose} wants image {alias}, which [image.{alias}] does not define for {provider}",
				alias = spec.image
			);
		};
		let Some(ladders) = self.plans.get(&spec.plans) else {
			bail!(
				"{whose} wants plans {:?}, which no [plans] table defines",
				spec.plans
			);
		};
		let Some(plans) = ladders.get(&provider) else {
			bail!(
				"{whose} wants plans {name}, which [plans.{name}] does not define for {provider}",
				name = spec.plans
			);
		};
		Ok(Class {
			labels: set.to_vec(),
			provider,
			plans: plans.clone(),
			locations: host.locations.clone(),
			image: image.clone(),
			ssh_key: host.ssh_key.clone(),
			lifetime_minutes: spec.lifetime_minutes,
			job_timeout_minutes: spec.job_timeout_minutes,
			allowed_events: spec.allowed_events.clone(),
			allow_fork_pull_request: spec.allow_fork_pull_request,
		})
	}

	/// The daemon never picks a vendor, so every set names its own
	fn provider_of(&self, set: &[String], whose: &str) -> Result<Provider> {
		let named: Vec<Provider> = Provider::ALL
			.iter()
			.copied()
			.filter(|kind| set.iter().any(|label| label == kind.as_str()))
			.collect();
		match named.as_slice() {
			[] => {
				let all: Vec<&str> = Provider::ALL
					.iter()
					.copied()
					.map(Provider::as_str)
					.collect();
				bail!(
					"{whose} names no provider: one label must be one of {}",
					all.join(", ")
				)
			}
			[one] => {
				self.declared(*one, whose)?;
				Ok(*one)
			}
			many => bail!(
				"{whose} names {} providers; a set must name exactly one",
				many.len()
			),
		}
	}

	fn validate_classes(&self) -> Result<()> {
		let names: Vec<String> = self.classes.iter().map(Class::name).collect();
		let mut seen: BTreeMap<Vec<String>, &str> = BTreeMap::new();
		for (class, name) in self.classes.iter().zip(&names) {
			let mut key = class.labels.clone();
			key.sort();
			if let Some(first) = seen.insert(key, name.as_str()) {
				bail!(
					"two [[machine]] entries answer to the same label set: {first} and {name}"
				);
			}
			if let Some(other) = names
				.iter()
				.find(|other| *other != name && other.starts_with(name))
			{
				bail!(
					"machine name {name} is a prefix of {other}: a machine name could not be attributed to one of them; write one of the two sets in another order"
				);
			}
			let longest = crate::naming::machine_name(
				self.machine_prefix(),
				name,
				&"0".repeat(36),
			);
			if longest.len() > HOSTNAME_LIMIT {
				bail!(
					"machine_prefix {} and labels {name} make a {}-character machine name, over the {HOSTNAME_LIMIT} hostname limit",
					self.machine_prefix(),
					longest.len()
				);
			}
		}
		Ok(())
	}

	pub fn machine_prefix(&self) -> &str {
		&self.daemon.machine_prefix
	}

	pub fn class(&self, name: &str) -> Option<&Class> {
		self.classes.iter().find(|class| class.name() == name)
	}

	/// A job's runs-on is a set, and exactly one [[machine]] answers to it
	pub fn class_for(&self, runs_on: &[String]) -> Result<&Class, String> {
		if let Some(class) =
			self.classes.iter().find(|class| class.matches(runs_on))
		{
			return Ok(class);
		}
		let listed = runs_on.join(", ");
		match self.nearest(runs_on) {
			Some(near) => Err(format!(
				"no [[machine]] answers to runs-on [{listed}]; the closest set configured is [{near}]"
			)),
			None => {
				Err(format!("no [[machine]] answers to runs-on [{listed}]"))
			}
		}
	}

	fn nearest(&self, runs_on: &[String]) -> Option<String> {
		self.classes
			.iter()
			.map(|class| {
				let shared = class
					.labels
					.iter()
					.filter(|label| runs_on.contains(label))
					.count();
				(shared, class)
			})
			.filter(|(shared, _)| *shared > 0)
			.max_by_key(|(shared, _)| *shared)
			.map(|(_, class)| class.labels.join(", "))
	}

	pub fn max_vms(&self, repo: &Repo, provider: Provider) -> usize {
		self.grants
			.get(&repo.to_string())
			.and_then(|grant| grant.max_vms.get(&provider))
			.copied()
			.unwrap_or(0)
	}

	/// Every label a job might put in runs-on, for the queue poll filter
	pub fn all_labels(&self) -> Vec<String> {
		let mut all: Vec<String> = self
			.classes
			.iter()
			.flat_map(|class| class.labels.iter().cloned())
			.collect();
		all.sort();
		all.dedup();
		all
	}

	pub fn entry_names(&self) -> Vec<String> {
		self.classes.iter().map(Class::name).collect()
	}

	pub fn longest_lifetime_minutes(&self) -> u64 {
		self.classes
			.iter()
			.map(|class| class.lifetime_minutes)
			.max()
			.unwrap_or(0)
	}

	pub fn poll_interval(&self) -> Duration {
		Duration::from_secs(self.daemon.poll_interval_secs)
	}

	pub fn reconcile_grace(&self) -> Duration {
		Duration::from_secs(self.daemon.reconcile_grace_secs)
	}
}

impl Spec {
	fn whose(&self) -> String {
		let first = self
			.labels
			.sets()
			.first()
			.map(|set| set.join(", "))
			.unwrap_or_default();
		format!("[[machine]] labels = [{first}]")
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const VALID: &str = include_str!("../fixtures/config.toml");
	const EXAMPLE: &str = include_str!("../config.example.toml");

	macro_rules! invalid {
		($($name:ident => $file:literal),* $(,)?) => {
			$(const $name: &str =
				include_str!(concat!("../fixtures/invalid/", $file));)*
		};
	}

	invalid! {
		MACHINE_PREFIX_TOO_LONG => "machine-prefix-too-long.toml",
		MACHINE_PREFIX_NOT_A_HOSTNAME => "machine-prefix-not-a-hostname.toml",
		NO_REPOS => "no-repos.toml",
		REPO_NOT_OWNER_NAME => "repo-not-owner-name.toml",
		NO_MACHINES => "no-machines.toml",
		NO_ALLOWED_EVENTS => "no-allowed-events.toml",
		NO_PLANS => "no-plans.toml",
		NO_LOCATIONS => "no-locations.toml",
		PREFIX_CLASS => "prefix-class.toml",
		ZERO_MAX_VMS => "zero-max-vms.toml",
		JOB_TIMEOUT_PAST_LIFETIME => "job-timeout-past-lifetime.toml",
		UNKNOWN_PROVIDER => "unknown-provider.toml",
		CLASS_NAME_TOO_LONG => "class-name-too-long.toml",
		NO_RUNNER_VERSION => "no-runner-version.toml",
		UNDECLARED_PROVIDER => "undeclared-provider.toml",
		MISSING_IMAGE => "missing-image.toml",
		UNKNOWN_FIELD => "unknown-field.toml",
		SET_NAMES_NO_PROVIDER => "set-names-no-provider.toml",
		SET_NAMES_TWO_PROVIDERS => "set-names-two-providers.toml",
		DUPLICATE_SET => "duplicate-set.toml",
		IMAGE_NOT_FOR_PROVIDER => "image-not-for-provider.toml",
		PLANS_NOT_FOR_PROVIDER => "plans-not-for-provider.toml",
		LABEL_NOT_A_HOSTNAME => "label-not-a-hostname.toml",
		OLD_SCHEMA => "old-schema.toml",
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

	fn valid() -> Config {
		Config::parse(VALID).unwrap()
	}

	fn runs_on(tokens: &[&str]) -> Vec<String> {
		tokens.iter().map(|s| (*s).to_owned()).collect()
	}

	#[test]
	fn parses_a_valid_config() {
		let config = valid();
		let mut repos: Vec<String> =
			config.repos.iter().map(Repo::to_string).collect();
		repos.sort();
		assert_eq!(repos, vec!["acme/gadgets", "acme/widgets"]);
		assert_eq!(config.poll_interval(), Duration::from_secs(15));
	}

	#[test]
	fn answers_only_to_its_exact_label_set() {
		let config = valid();
		let class = config.class_for(&runs_on(&["check", "hetzner"])).unwrap();
		assert_eq!(class.name(), "check-hetzner");
		assert_eq!(class.plans, vec!["cx43", "cx53"]);
		assert_eq!(class.image, "snapshot-1");
		assert!(class.allow_fork_pull_request);
		assert!(
			config.class_for(&runs_on(&["check"])).is_err(),
			"a subset is a different set"
		);
		assert!(
			config
				.class_for(&runs_on(&["check", "hetzner", "more"]))
				.is_err(),
			"a superset is a different set"
		);
	}

	#[test]
	fn a_set_of_one_label_is_a_set_like_any_other() {
		let config = valid();
		let class = config
			.class_for(&runs_on(&["hetzner"]))
			.expect("one label is a set");
		assert_eq!(class.name(), "hetzner");
		assert_eq!(class.provider, Provider::Hetzner);
	}

	#[test]
	fn one_entry_can_answer_to_several_sets() {
		let config = valid();
		let hetzner =
			config.class_for(&runs_on(&["build", "hetzner"])).unwrap();
		let cherry = config.class_for(&runs_on(&["build", "cherry"])).unwrap();
		assert_eq!(
			hetzner.lifetime_minutes, cherry.lifetime_minutes,
			"one entry, so its limits are written once"
		);
		assert_eq!(hetzner.image, "debian-12");
		assert_eq!(cherry.image, "debian_12_64bit");
		assert_eq!(cherry.provider, Provider::Cherry);
	}

	#[test]
	fn the_label_order_does_not_matter() {
		let config = valid();
		assert_eq!(
			config
				.class_for(&runs_on(&["hetzner", "check"]))
				.unwrap()
				.name(),
			"check-hetzner"
		);
	}

	#[test]
	fn a_refusal_names_the_closest_set_configured() {
		let config = valid();
		let error = config
			.class_for(&runs_on(&["check", "cherry"]))
			.unwrap_err();
		assert!(
			error.contains("the closest set configured is ["),
			"got {error}"
		);
	}

	#[test]
	fn a_grant_is_both_the_allowlist_and_the_quota() {
		let config = valid();
		let widgets = Repo {
			owner: "acme".into(),
			name: "widgets".into(),
		};
		let gadgets = Repo {
			owner: "acme".into(),
			name: "gadgets".into(),
		};
		assert_eq!(config.max_vms(&widgets, Provider::Hetzner), 2);
		assert_eq!(
			config.max_vms(&gadgets, Provider::Cherry),
			0,
			"a provider a repo was never granted must read as no machines"
		);
	}

	#[test]
	fn polls_for_every_label_any_set_carries() {
		let config = valid();
		assert_eq!(
			config.all_labels(),
			vec!["build", "check", "cherry", "hetzner"]
		);
	}

	#[test]
	fn the_shipped_example_is_valid() {
		Config::parse(EXAMPLE).unwrap();
	}

	#[test]
	fn defaults_the_machine_prefix() {
		assert_eq!(valid().machine_prefix(), crate::naming::DEFAULT_PREFIX);
	}

	#[test]
	fn rejects_a_machine_prefix_that_overflows_the_hostname_limit() {
		rejects(MACHINE_PREFIX_TOO_LONG, "over the 63 hostname limit");
	}

	#[test]
	fn rejects_a_machine_prefix_that_is_not_a_hostname_label() {
		rejects(MACHINE_PREFIX_NOT_A_HOSTNAME, "must be a hostname label");
	}

	#[test]
	fn rejects_a_config_with_no_repositories() {
		rejects(NO_REPOS, "no [repo] entries");
	}

	#[test]
	fn rejects_a_repository_key_that_is_not_owner_slash_name() {
		rejects(REPO_NOT_OWNER_NAME, "must be named \"owner/name\"");
	}

	#[test]
	fn rejects_a_config_with_no_machines() {
		rejects(NO_MACHINES, "no [[machine]] entries");
	}

	#[test]
	fn rejects_a_machine_with_no_allowed_events() {
		rejects(NO_ALLOWED_EVENTS, "no allowed_events");
	}

	#[test]
	fn rejects_a_job_timeout_that_outlives_the_machine() {
		rejects(JOB_TIMEOUT_PAST_LIFETIME, "job_timeout_minutes");
	}

	#[test]
	fn rejects_a_plans_table_with_no_plans() {
		rejects(NO_PLANS, "has no plans for");
	}

	#[test]
	fn rejects_a_provider_with_no_locations() {
		rejects(NO_LOCATIONS, "has no locations");
	}

	#[test]
	fn rejects_a_machine_name_that_prefixes_another() {
		rejects(PREFIX_CLASS, "is a prefix of");
	}

	#[test]
	fn rejects_a_grant_that_can_hold_no_machines() {
		rejects(ZERO_MAX_VMS, "max_vms.hetzner = 0");
	}

	#[test]
	fn rejects_an_unknown_provider() {
		rejects(UNKNOWN_PROVIDER, "parsing config");
	}

	#[test]
	fn rejects_a_provider_no_table_declares() {
		rejects(UNDECLARED_PROVIDER, "has no [provider.vultr] table");
	}

	#[test]
	fn rejects_an_image_alias_that_does_not_exist() {
		rejects(MISSING_IMAGE, "which no [image] table defines");
	}

	#[test]
	fn rejects_a_field_no_schema_knows() {
		rejects(UNKNOWN_FIELD, "parsing config");
	}

	#[test]
	fn rejects_a_class_whose_machine_name_would_not_fit() {
		rejects(CLASS_NAME_TOO_LONG, "over the 63 hostname limit");
	}

	#[test]
	fn rejects_a_config_with_no_runner_version() {
		rejects(NO_RUNNER_VERSION, "runner_version");
	}

	#[test]
	fn rejects_a_set_that_names_no_provider() {
		rejects(SET_NAMES_NO_PROVIDER, "names no provider");
	}

	#[test]
	fn rejects_a_set_that_names_two_providers() {
		rejects(SET_NAMES_TWO_PROVIDERS, "names 2 providers");
	}

	#[test]
	fn rejects_two_entries_answering_to_the_same_set() {
		rejects(DUPLICATE_SET, "answer to the same label set");
	}

	#[test]
	fn rejects_an_image_with_no_id_for_the_provider_a_set_names() {
		rejects(
			IMAGE_NOT_FOR_PROVIDER,
			"[image.toolchain] does not define for cherry",
		);
	}

	#[test]
	fn rejects_plans_with_no_ladder_for_the_provider_a_set_names() {
		rejects(
			PLANS_NOT_FOR_PROVIDER,
			"[plans.4c8g] does not define for cherry",
		);
	}

	#[test]
	fn rejects_a_label_that_is_not_a_hostname_label() {
		rejects(LABEL_NOT_A_HOSTNAME, "is not a hostname label");
	}

	#[test]
	fn rejects_the_previous_schema_loudly() {
		rejects(OLD_SCHEMA, "parsing config");
	}
}
