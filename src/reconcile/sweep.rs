use std::collections::HashSet;
use std::time::{Duration, Instant};

use time::OffsetDateTime;

use tracing::{info, warn};

use crate::alert::Kind;
use crate::config::Provider;
use crate::forgejo::Queue;
use crate::naming;
use crate::provider::{Fleet, Machine};

use super::{Orchestrator, Queued, Survey};

/// A successful poll returns waiting and running jobs,
/// a second miss only guards against a single anomalous read
const MISSES_BEFORE_DESTROY: u32 = 2;

/// One listing without the machine is never proof it is gone
const MISSES_BEFORE_REAP: u32 = 2;

impl<Q: Queue, F: Fleet> Orchestrator<Q, F> {
	pub(super) async fn destroy_departed(
		&mut self,
		queued: &[Queued],
		survey: &Survey,
		labels: &[String],
	) {
		let live: HashSet<String> = queued
			.iter()
			.map(|entry| naming::truncated_handle(&entry.job.handle))
			.collect();
		for (kind, machine) in &survey.fleet {
			match naming::split(&machine.name, labels) {
				Some((_, handle)) if live.contains(handle) => {
					self.missed_polls.remove(&machine.name);
					continue;
				}
				Some(_) => {}
				None => {
					self.alerts
						.raise(
							Kind::Abandoned,
							&machine.name,
							"carries our prefix but matches no configured label",
						)
						.await;
				}
			}
			let missed = {
				let seen =
					self.missed_polls.entry(machine.name.clone()).or_insert(0);
				*seen += 1;
				*seen
			};
			if missed < MISSES_BEFORE_DESTROY {
				continue;
			}
			self.destroy(*kind, machine).await;
		}
	}

	pub(super) async fn destroy_overdue(
		&mut self,
		survey: &Survey,
		labels: &[String],
	) {
		for (kind, machine) in &survey.fleet {
			let Some(limit) = self.lifetime_of(&machine.name, labels) else {
				continue;
			};
			let age = self.age_of(machine);
			if age <= limit {
				continue;
			}
			warn!(
				machine = %machine.name,
				age_minutes = age.as_secs() / 60,
				"past its lifetime; destroying"
			);
			self.alerts
				.raise(
					Kind::Abandoned,
					&machine.name,
					"outlived its label's lifetime_minutes",
				)
				.await;
			self.destroy(*kind, machine).await;
		}
	}

	pub(super) async fn destroy_unseen(
		&mut self,
		survey: &Survey,
		labels: &[String],
	) {
		let listed: HashSet<&str> = survey
			.fleet
			.iter()
			.map(|(_, machine)| machine.name.as_str())
			.collect();

		let stale: Vec<(Provider, Machine)> = self
			.unseen
			.iter()
			.filter(|(name, _)| !listed.contains(name.as_str()))
			.map(|(_, entry)| entry)
			.filter(|(kind, _)| !survey.blind.contains(kind))
			.cloned()
			.collect();
		for (kind, machine) in stale {
			let Some(limit) = self.lifetime_of(&machine.name, labels) else {
				continue;
			};
			if self.age_of(&machine) <= limit {
				continue;
			}
			warn!(
				machine = %machine.name,
				"never listed by its provider and past its lifetime; destroying"
			);
			self.destroy(kind, &machine).await;
			self.unseen.remove(&machine.name);
		}
	}

	fn lifetime_of(&self, name: &str, labels: &[String]) -> Option<Duration> {
		let (label, _) = naming::split(name, labels)?;
		let label = self.config.label(label)?;
		Some(
			Duration::from_secs(label.lifetime_minutes * 60)
				+ self.config.reconcile_grace(),
		)
	}

	fn age_of(&mut self, machine: &Machine) -> Duration {
		if let Some(created) = machine.created_at {
			let seconds = (OffsetDateTime::now_utc() - created).whole_seconds();
			return Duration::from_secs(seconds.max(0) as u64);
		}
		self.first_seen
			.entry(machine.name.clone())
			.or_insert_with(Instant::now)
			.elapsed()
	}

	async fn destroy(&mut self, kind: Provider, machine: &Machine) {
		let outcome = self.clouds.destroy(kind, &machine.id).await;
		match outcome {
			Ok(()) => {
				info!(machine = %machine.name, "destroyed");
				self.unseen.remove(&machine.name);
				self.first_seen.remove(&machine.name);
				self.missed_polls.remove(&machine.name);
				self.alerts.clear(Kind::SweepFailed, &machine.name);
				self.alerts.clear(Kind::Abandoned, &machine.name);
			}
			Err(error) => {
				self.alerts
					.raise(
						Kind::SweepFailed,
						&machine.name,
						&format!("{error:#}"),
					)
					.await;
			}
		}
	}

	/// A provider's list can lag creation by minutes, so a record counts as
	/// orphaned only after a whole reconcile grace of misses.
	pub(super) async fn reap_orphan_runners(&mut self, survey: &Survey) {
		if !survey.blind.is_empty() {
			return;
		}
		let mut alive: HashSet<String> = survey
			.fleet
			.iter()
			.map(|(_, machine)| machine.name.clone())
			.collect();
		alive.extend(self.unseen.keys().cloned());
		let repos = self.config.repos.clone();
		let mut seen: HashSet<String> = HashSet::new();
		let mut all_repos_listed = true;
		for repo in &repos {
			let runners = match self.forgejo.runners(repo).await {
				Ok(runners) => runners,
				Err(error) => {
					all_repos_listed = false;
					warn!(%repo, error = %format!("{error:#}"), "listing runners failed");
					continue;
				}
			};
			for runner in runners {
				if !runner.name.starts_with(naming::PREFIX) {
					continue;
				}
				seen.insert(runner.name.clone());
				if alive.contains(runner.name.as_str()) {
					self.missed_machines.remove(&runner.name);
					continue;
				}
				let (missed, first_missed) = {
					let seen = self
						.missed_machines
						.entry(runner.name.clone())
						.or_insert((0, Instant::now()));
					seen.0 += 1;
					*seen
				};
				if missed < MISSES_BEFORE_REAP
					|| first_missed.elapsed() < self.config.reconcile_grace()
				{
					continue;
				}
				match self.forgejo.delete_runner(repo, runner.id).await {
					Ok(()) => {
						self.missed_machines.remove(&runner.name);
						info!(runner = %runner.name, %repo, "deleted orphan runner record")
					}
					Err(error) => {
						warn!(runner = %runner.name, %repo, error = %format!("{error:#}"), "deleting orphan runner failed")
					}
				}
			}
		}
		if all_repos_listed {
			self.missed_machines.retain(|name, _| seen.contains(name));
		}
	}
}
