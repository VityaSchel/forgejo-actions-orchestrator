use std::fmt;

use crate::config::{Config, Label};

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
	UnknownLabels(Vec<String>),
	EventNotAllowed { label: String, event: String },
	ForkPullRequest(String),
	AtCapacity { label: String, max: usize },
}

impl fmt::Display for Denial {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::NoLabel => write!(f, "job declares no runs-on label"),
			Self::NoEvent => {
				write!(f, "run reports no triggering event")
			}
			Self::UnknownLabels(labels) => {
				write!(f, "no configuration for labels {}", labels.join(", "))
			}
			Self::EventNotAllowed { label, event } => {
				write!(f, "label {label} is not available to {event} events")
			}
			Self::ForkPullRequest(label) => {
				write!(
					f,
					"label {label} is not available to fork pull requests"
				)
			}
			Self::AtCapacity { label, max } => {
				write!(f, "label {label} already has {max} machine(s) running")
			}
		}
	}
}

pub fn resolve<'a>(
	config: &'a Config,
	request: &JobRequest,
) -> Result<&'a Label, Denial> {
	if request.runs_on.is_empty() {
		return Err(Denial::NoLabel);
	}
	config
		.label_for(&request.runs_on)
		.ok_or_else(|| Denial::UnknownLabels(request.runs_on.clone()))
}

pub fn admit(
	label: &Label,
	request: &JobRequest,
	live: usize,
) -> Result<(), Denial> {
	if request.event.is_empty() {
		return Err(Denial::NoEvent);
	}
	if !label.allowed_events.iter().any(|e| e == &request.event) {
		return Err(Denial::EventNotAllowed {
			label: label.name(),
			event: request.event.clone(),
		});
	}

	if request.is_fork_pull_request && !label.allow_fork_pull_request {
		return Err(Denial::ForkPullRequest(label.name()));
	}

	if live >= label.max_vms {
		return Err(Denial::AtCapacity {
			label: label.name(),
			max: label.max_vms,
		});
	}

	Ok(())
}

#[cfg(test)]
mod tests {

	fn decide<'a>(
		config: &'a Config,
		request: &JobRequest,
		live: usize,
	) -> Result<&'a Label, Denial> {
		let label = resolve(config, request)?;
		admit(label, request, live)?;
		Ok(label)
	}
	use super::*;

	fn config() -> Config {
		Config::parse(include_str!("../fixtures/config.toml")).unwrap()
	}

	fn request(labels: &[&str], event: &str, fork: bool) -> JobRequest {
		JobRequest {
			runs_on: labels.iter().map(|s| s.to_string()).collect(),
			event: event.to_string(),
			is_fork_pull_request: fork,
		}
	}

	#[test]
	fn allows_a_fork_pull_request_on_the_cheap_label() {
		let config = config();
		let label =
			decide(&config, &request(&["check"], "pull_request", true), 0)
				.unwrap();
		assert_eq!(label.name(), "check");
	}

	#[test]
	fn refuses_an_expensive_label_to_a_fork_pull_request() {
		let config = config();
		let denial = decide(
			&config,
			&request(&["builder", "cherry"], "pull_request", true),
			0,
		)
		.unwrap_err();
		assert_eq!(
			denial,
			Denial::EventNotAllowed {
				label: "builder-cherry".into(),
				event: "pull_request".into()
			}
		);
	}

	#[test]
	fn refuses_a_fork_pull_request_even_on_an_allowed_event() {
		let config = config();
		let denial =
			decide(&config, &request(&["builder", "cherry"], "push", true), 0)
				.unwrap_err();
		assert_eq!(denial, Denial::ForkPullRequest("builder-cherry".into()));
	}

	#[test]
	fn allows_the_expensive_label_to_a_trusted_push() {
		let config = config();
		let label =
			decide(&config, &request(&["builder", "cherry"], "push", false), 0)
				.unwrap();
		assert_eq!(label.name(), "builder-cherry");
	}

	#[test]
	fn refuses_an_unknown_label() {
		let config = config();
		let denial =
			decide(&config, &request(&["nope"], "push", false), 0).unwrap_err();
		assert_eq!(denial, Denial::UnknownLabels(vec!["nope".into()]));
	}

	#[test]
	fn refuses_an_unconfigured_label_set() {
		let config = config();
		let denial = decide(
			&config,
			&request(&["check", "cherry"], "pull_request", false),
			0,
		)
		.unwrap_err();
		assert!(matches!(denial, Denial::UnknownLabels(_)));
	}

	#[test]
	fn refuses_a_run_that_reports_no_event() {
		let config = config();
		let denial =
			decide(&config, &request(&["check"], "", false), 0).unwrap_err();
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
			decide(&config, &request(&[], "push", false), 0).unwrap_err();
		assert_eq!(denial, Denial::NoLabel);
	}

	#[test]
	fn refuses_once_the_label_is_at_capacity() {
		let config = config();
		let denial =
			decide(&config, &request(&["check"], "pull_request", true), 1)
				.unwrap_err();
		assert_eq!(
			denial,
			Denial::AtCapacity {
				label: "check".into(),
				max: 1
			}
		);
	}
}
