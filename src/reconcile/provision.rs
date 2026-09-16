use std::collections::{HashMap, HashSet};

use anyhow::Result;
use tracing::{info, warn};

use crate::alert::Kind;
use crate::cloudinit;
use crate::config::{Class, Provider, Repo};
use crate::forgejo::{self, Queue, StatusState};
use crate::naming;
use crate::policy::{self, JobRequest};
use crate::provider::{placements, Fleet, Machine};

use super::{Orchestrator, Queued, Survey};

impl<Q: Queue, F: Fleet> Orchestrator<Q, F> {
	pub(super) async fn provision_arrived(
		&mut self,
		queued: &[Queued],
		survey: &Survey,
		names: &[String],
	) {
		let prefix = self.config.machine_prefix().to_owned();
		let mut served: HashSet<String> = survey
			.fleet
			.iter()
			.filter_map(|(_, machine)| {
				naming::split(&prefix, &machine.name, names)
					.map(|(_, h)| h.to_owned())
			})
			.collect();
		served.extend(self.unseen.keys().filter_map(|name| {
			naming::split(&prefix, name, names)
				.map(|(_, handle)| handle.to_owned())
		}));
		let in_flight: Vec<String> = survey
			.fleet
			.iter()
			.map(|(_, machine)| machine.name.clone())
			.chain(self.unseen.keys().cloned())
			.collect();

		let mut pending: HashMap<String, usize> = HashMap::new();

		for entry in queued.iter().filter(|e| e.job.status == "waiting") {
			let handle = naming::truncated_handle(&entry.job.handle);
			if !served.insert(handle.clone()) {
				continue;
			}
			let tail = format!("-{handle}");
			if in_flight.iter().any(|name| name.ends_with(&tail)) {
				continue;
			}
			if let Err(error) =
				self.provision(entry, queued, survey, &mut pending).await
			{
				self.alerts
					.raise(
						Kind::CreateFailed,
						&entry.job.handle,
						&format!("{error:#}"),
					)
					.await;
			}
		}
	}

	async fn provision(
		&mut self,
		entry: &Queued,
		queued: &[Queued],
		survey: &Survey,
		pending: &mut HashMap<String, usize>,
	) -> Result<()> {
		let Queued { repo, job } = entry;
		let run = self.forgejo.run(repo, job.run_id).await?;
		let sha = forgejo::status_target_sha(&run);
		let run_url = run.html_url.clone();
		let request = JobRequest {
			runs_on: job.runs_on.clone(),
			event: run.triggering_event().to_owned(),
			is_fork_pull_request: run.is_fork_pull_request,
		};

		let class = match policy::resolve(&self.config, &request) {
			Ok(class) => class.clone(),
			Err(denial) => {
				self.log_refusal(&job.handle, &denial.to_string());
				self.report(
					repo,
					&sha,
					run_url.as_deref(),
					StatusState::Error,
					&denial.to_string(),
				)
				.await;
				return Ok(());
			}
		};

		let quota = format!("{repo}/{}", class.provider);
		let live = self.live_for(repo, class.provider, queued, survey)
			+ pending.get(&quota).copied().unwrap_or(0);
		if let Err(denial) =
			policy::admit(&self.config, &class, repo, &request, live)
		{
			{
				self.log_refusal(&job.handle, &denial.to_string());
				self.report(
					repo,
					&sha,
					run_url.as_deref(),
					StatusState::Error,
					&denial.to_string(),
				)
				.await;
				return Ok(());
			}
		}

		if survey.blind.contains(&class.provider) {
			warn!(handle = %job.handle, provider = ?class.provider, "held back: this provider's machines are not visible");
			return Ok(());
		}

		self.report(
			repo,
			&sha,
			run_url.as_deref(),
			StatusState::Pending,
			&format!("provisioning a {} machine", class.name()),
		)
		.await;

		let name = naming::machine_name(
			self.config.machine_prefix(),
			&class.name(),
			&job.handle,
		);
		let registration = self.forgejo.register_runner(repo, &name).await?;
		let user_data = cloudinit::render(
			&self.config.daemon,
			&class,
			&self.config.forgejo.url,
			&registration,
			&job.handle,
		);

		match self.place(&class, &name, &user_data).await {
			Ok((placement, machine)) => {
				self.unseen
					.insert(machine.name.clone(), (class.provider, machine));
				*pending.entry(quota).or_default() += 1;
				info!(machine = %name, %placement, "created");
				self.alerts.clear(Kind::CreateFailed, &job.handle);
				self.report(
					repo,
					&sha,
					run_url.as_deref(),
					StatusState::Success,
					&format!("running on {placement}"),
				)
				.await;
				Ok(())
			}
			Err(error) => {
				if let Err(cleanup) =
					self.forgejo.delete_runner(repo, registration.id).await
				{
					warn!(cleanup = %format!("{cleanup:#}"), "could not delete the runner of a failed placement");
				}
				self.report(
					repo,
					&sha,
					run_url.as_deref(),
					StatusState::Error,
					&error.to_string(),
				)
				.await;
				Err(error)
			}
		}
	}

	pub(super) fn log_refusal(&mut self, handle: &str, denial: &str) {
		if self.refusals.get(handle).is_some_and(|last| last == denial) {
			return;
		}
		info!(%handle, %denial, "refused");
		self.refusals.insert(handle.to_owned(), denial.to_owned());
	}

	pub(super) fn forget_settled_refusals(&mut self, queued: &[Queued]) {
		let live: HashSet<&str> = queued
			.iter()
			.map(|entry| entry.job.handle.as_str())
			.collect();
		self.refusals
			.retain(|handle, _| live.contains(handle.as_str()));
	}

	/// By handle, never by class name: a rename must not empty the quota
	/// while the machines it renamed are still billing
	fn live_for(
		&self,
		repo: &Repo,
		provider: Provider,
		queued: &[Queued],
		survey: &Survey,
	) -> usize {
		let head = format!("{}-", self.config.machine_prefix());
		let mine: Vec<String> = queued
			.iter()
			.filter(|entry| &entry.repo == repo)
			.map(|entry| {
				format!("-{}", naming::truncated_handle(&entry.job.handle))
			})
			.collect();
		let mut counted: HashSet<&str> = HashSet::new();
		survey
			.fleet
			.iter()
			.map(|(kind, machine)| (*kind, machine.name.as_str()))
			.chain(
				self.unseen
					.values()
					.map(|(kind, machine)| (*kind, machine.name.as_str())),
			)
			.filter(|(_, name)| counted.insert(name))
			.filter(|(kind, name)| {
				*kind == provider
					&& name.starts_with(&head)
					&& mine.iter().any(|tail| name.ends_with(tail.as_str()))
			})
			.count()
	}

	async fn place(
		&self,
		class: &Class,
		name: &str,
		user_data: &str,
	) -> Result<(String, Machine)> {
		let mut last = None;
		for placement in placements(class) {
			match self
				.clouds
				.create(
					class.provider,
					name,
					&placement.plan,
					&placement.location,
					&class.image,
					class.ssh_key.as_deref(),
					user_data,
				)
				.await
			{
				Ok(machine) => {
					return Ok((
						format!("{} in {}", placement.plan, placement.location),
						machine,
					))
				}
				Err(error) => {
					warn!(machine = %name, plan = %placement.plan, location = %placement.location, error = %format!("{error:#}"), "placement rejected");
					last = Some(error);
				}
			}
		}
		Err(last.unwrap_or_else(|| {
			anyhow::anyhow!("{}: nowhere to place it", class.name())
		}))
	}
}
