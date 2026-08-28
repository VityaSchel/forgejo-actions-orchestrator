use std::collections::{HashMap, HashSet};
use std::time::Instant;

use tracing::warn;

use crate::alert::{Alerts, Kind};
use crate::config::{Config, Provider, Repo};
use crate::forgejo::{Job, Queue, StatusState};
use crate::naming;
use crate::provider::{Fleet, Machine};

mod provision;
mod sweep;
#[cfg(test)]
mod tests;

const STATUS_CONTEXT: &str = "ci-orchestrator / provision";

pub(crate) type Machines = Vec<(Provider, Machine)>;

pub struct Orchestrator<Q: Queue, F: Fleet> {
	pub(crate) config: Config,
	pub(crate) forgejo: Q,
	pub(crate) clouds: F,
	pub(crate) alerts: Alerts,
	pub(crate) first_seen: HashMap<String, Instant>,
	pub(crate) missed_polls: HashMap<String, u32>,
	pub(crate) missed_machines: HashMap<String, u32>,
}

pub(crate) struct Queued {
	pub(crate) repo: Repo,
	pub(crate) job: Job,
}

/// `complete` is false when any repository's poll failed
pub(crate) struct Poll {
	pub(crate) queued: Vec<Queued>,
	pub(crate) all_queues_read: bool,
}

pub(crate) struct Survey {
	pub(crate) fleet: Machines,
	pub(crate) blind: HashSet<Provider>,
}

impl<Q: Queue, F: Fleet> Orchestrator<Q, F> {
	pub fn new(config: Config, forgejo: Q, clouds: F, alerts: Alerts) -> Self {
		Self {
			config,
			forgejo,
			clouds,
			alerts,
			first_seen: HashMap::new(),
			missed_polls: HashMap::new(),
			missed_machines: HashMap::new(),
		}
	}

	pub async fn tick(&mut self) {
		let names = self.config.entry_names();
		let survey = self.survey().await;

		let poll = self.poll_queues(&self.config.all_labels()).await;

		self.destroy_overdue(&survey, &names).await;
		if poll.all_queues_read {
			self.destroy_departed(&poll.queued, &survey, &names).await;
		}
		self.reap_orphan_runners(&survey).await;
		self.provision_arrived(&poll.queued, &survey, &names).await;
	}

	async fn poll_queues(&mut self, labels: &[String]) -> Poll {
		let repos = self.config.repos.clone();
		let mut poll = Poll {
			queued: Vec::new(),
			all_queues_read: true,
		};
		for repo in repos {
			match self.forgejo.waiting_jobs(&repo, labels).await {
				Ok(jobs) => {
					self.alerts.clear(Kind::PollFailed, &repo.to_string());
					poll.queued.extend(jobs.into_iter().map(|job| Queued {
						repo: repo.clone(),
						job,
					}));
				}
				Err(error) => {
					poll.all_queues_read = false;
					self.alerts
						.raise(
							Kind::PollFailed,
							&repo.to_string(),
							&format!("{error:#}"),
						)
						.await;
				}
			}
		}
		poll
	}

	async fn survey(&mut self) -> Survey {
		let kinds: Vec<Provider> = self.clouds.kinds();
		let mut fleet = Machines::new();
		let mut blind = HashSet::new();
		for kind in kinds {
			let listed = self.clouds.list(kind, naming::PREFIX).await;
			match listed {
				Ok(machines) => {
					self.alerts.clear(Kind::PollFailed, &format!("{kind:?}"));
					fleet.extend(
						machines.into_iter().map(|machine| (kind, machine)),
					);
				}
				Err(error) => {
					blind.insert(kind);
					self.alerts
						.raise(
							Kind::PollFailed,
							&format!("{kind:?}"),
							&format!("{error:#}"),
						)
						.await;
				}
			}
		}
		Survey { fleet, blind }
	}

	async fn report(
		&self,
		repo: &Repo,
		sha: &Option<String>,
		run_url: Option<&str>,
		state: StatusState,
		description: &str,
	) {
		let Some(sha) = sha else { return };
		if let Err(error) = self
			.forgejo
			.set_status(repo, sha, state, STATUS_CONTEXT, description, run_url)
			.await
		{
			warn!(error = %format!("{error:#}"), "posting commit status failed");
		}
	}
}
