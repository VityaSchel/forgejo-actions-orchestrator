use std::fmt;

use crate::config::{Class, Config, Provider, Repo};

#[derive(Debug, Clone)]
pub struct JobRequest {
	pub runs_on: Vec<String>,
	pub event: String,
	pub is_fork_pull_request: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Denial {
	NoLabel,
	NoEvent,
	Unresolvable(String),
	EventNotAllowed {
		class: String,
		event: String,
	},
	ForkPullRequest(String),
	NotGranted {
		repo: String,
		provider: Provider,
	},
	AtCapacity {
		repo: String,
		provider: Provider,
		max: usize,
	},
}

impl fmt::Display for Denial {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::NoLabel => write!(f, "job declares no runs-on label"),
			Self::NoEvent => write!(f, "run reports no triggering event"),
			Self::Unresolvable(why) => write!(f, "{why}"),
			Self::EventNotAllowed { class, event } => {
				write!(f, "class {class} is not available to {event} events")
			}
			Self::ForkPullRequest(class) => {
				write!(
					f,
					"class {class} is not available to fork pull requests"
				)
			}
			Self::NotGranted { repo, provider } => {
				write!(f, "{repo} is not granted any {provider} machine")
			}
			Self::AtCapacity {
				repo,
				provider,
				max,
			} => {
				write!(
					f,
					"{repo} already has {max} {provider} machine(s) running"
				)
			}
		}
	}
}

pub fn resolve<'a>(
	config: &'a Config,
	request: &JobRequest,
) -> Result<&'a Class, Denial> {
	if request.runs_on.is_empty() {
		return Err(Denial::NoLabel);
	}
	config
		.class_for(&request.runs_on)
		.map_err(Denial::Unresolvable)
}

pub fn admit(
	config: &Config,
	class: &Class,
	repo: &Repo,
	request: &JobRequest,
	live: usize,
) -> Result<(), Denial> {
	if request.event.is_empty() {
		return Err(Denial::NoEvent);
	}
	if !class.allowed_events.iter().any(|e| e == &request.event) {
		return Err(Denial::EventNotAllowed {
			class: class.name(),
			event: request.event.clone(),
		});
	}

	if request.is_fork_pull_request && !class.allow_fork_pull_request {
		return Err(Denial::ForkPullRequest(class.name()));
	}

	let max = config.max_vms(repo, class.provider);
	if max == 0 {
		return Err(Denial::NotGranted {
			repo: repo.to_string(),
			provider: class.provider,
		});
	}
	if live >= max {
		return Err(Denial::AtCapacity {
			repo: repo.to_string(),
			provider: class.provider,
			max,
		});
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn config() -> Config {
		Config::parse(include_str!("../fixtures/config.toml")).unwrap()
	}

	fn widgets() -> Repo {
		Repo {
			owner: "acme".into(),
			name: "widgets".into(),
		}
	}

	fn gadgets() -> Repo {
		Repo {
			owner: "acme".into(),
			name: "gadgets".into(),
		}
	}

	fn decide<'a>(
		config: &'a Config,
		repo: &Repo,
		request: &JobRequest,
		live: usize,
	) -> Result<&'a Class, Denial> {
		let class = resolve(config, request)?;
		admit(config, class, repo, request, live)?;
		Ok(class)
	}

	fn request(labels: &[&str], event: &str, fork: bool) -> JobRequest {
		JobRequest {
			runs_on: labels.iter().map(|s| (*s).to_string()).collect(),
			event: event.to_string(),
			is_fork_pull_request: fork,
		}
	}

	#[test]
	fn allows_a_fork_pull_request_on_a_fork_open_set() {
		let config = config();
		let class = decide(
			&config,
			&widgets(),
			&request(&["check", "hetzner"], "pull_request", true),
			0,
		)
		.unwrap();
		assert_eq!(class.name(), "check-hetzner");
	}

	#[test]
	fn refuses_a_set_the_event_may_not_use() {
		let config = config();
		let denial = decide(
			&config,
			&widgets(),
			&request(&["build", "hetzner"], "pull_request", true),
			0,
		)
		.unwrap_err();
		assert_eq!(
			denial,
			Denial::EventNotAllowed {
				class: "build-hetzner".into(),
				event: "pull_request".into()
			}
		);
	}

	#[test]
	fn refuses_a_fork_pull_request_even_on_an_allowed_event() {
		let config = config();
		let denial = decide(
			&config,
			&widgets(),
			&request(&["build", "hetzner"], "push", true),
			0,
		)
		.unwrap_err();
		assert_eq!(denial, Denial::ForkPullRequest("build-hetzner".into()));
	}

	#[test]
	fn allows_a_trusted_push_on_a_fork_closed_set() {
		let config = config();
		let class = decide(
			&config,
			&widgets(),
			&request(&["build", "hetzner"], "push", false),
			0,
		)
		.unwrap();
		assert_eq!(class.name(), "build-hetzner");
	}

	#[test]
	fn refuses_an_unknown_token() {
		let config = config();
		let denial = decide(
			&config,
			&widgets(),
			&request(&["nope", "hetzner"], "push", false),
			0,
		)
		.unwrap_err();
		assert!(matches!(denial, Denial::Unresolvable(_)));
	}

	#[test]
	fn refuses_a_runs_on_no_entry_answers_to() {
		let config = config();
		let denial = decide(
			&config,
			&widgets(),
			&request(&["check"], "pull_request", false),
			0,
		)
		.unwrap_err();
		assert!(matches!(denial, Denial::Unresolvable(_)));
	}

	#[test]
	fn refuses_a_run_that_reports_no_event() {
		let config = config();
		let denial = decide(
			&config,
			&widgets(),
			&request(&["check", "hetzner"], "", false),
			0,
		)
		.unwrap_err();
		assert_eq!(
			denial,
			Denial::NoEvent,
			"an unknown event must refuse, never fall through to an allowed_events mismatch"
		);
	}

	#[test]
	fn refuses_a_job_with_no_label() {
		let config = config();
		let denial =
			decide(&config, &widgets(), &request(&[], "push", false), 0)
				.unwrap_err();
		assert_eq!(denial, Denial::NoLabel);
	}

	#[test]
	fn refuses_a_provider_the_repository_was_never_granted() {
		let config = config();
		let denial = decide(
			&config,
			&gadgets(),
			&request(&["build", "cherry"], "push", false),
			0,
		)
		.unwrap_err();
		assert_eq!(
			denial,
			Denial::NotGranted {
				repo: "acme/gadgets".into(),
				provider: Provider::Cherry
			}
		);
	}

	#[test]
	fn refuses_once_the_repository_fills_its_grant() {
		let config = config();
		let denial = decide(
			&config,
			&gadgets(),
			&request(&["check", "hetzner"], "pull_request", true),
			1,
		)
		.unwrap_err();
		assert_eq!(
			denial,
			Denial::AtCapacity {
				repo: "acme/gadgets".into(),
				provider: Provider::Hetzner,
				max: 1
			}
		);
	}

	#[test]
	fn one_repository_filling_its_grant_leaves_another_room() {
		let config = config();
		decide(
			&config,
			&widgets(),
			&request(&["check", "hetzner"], "pull_request", true),
			1,
		)
		.expect("widgets is granted two, so its second machine is allowed");
	}
}
