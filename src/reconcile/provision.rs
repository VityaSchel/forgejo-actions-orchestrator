use std::collections::{HashMap, HashSet};

use anyhow::Result;
use tracing::{info, warn};

use crate::alert::Kind;
use crate::cloudinit;
use crate::config::Label;
use crate::forgejo::{self, Queue, StatusState};
use crate::naming;
use crate::policy::{self, JobRequest};
use crate::provider::{placements, Fleet, Machine};

use super::{Machines, Orchestrator, Queued, Survey};

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

		let mut pending: HashMap<String, usize> = HashMap::new();

		for entry in queued.iter().filter(|e| e.job.status == "waiting") {
			if !served.insert(naming::truncated_handle(&entry.job.handle)) {
				continue;
			}
			if let Err(error) =
				self.provision(entry, survey, names, &mut pending).await
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
		survey: &Survey,
		names: &[String],
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

		let label = match policy::resolve(&self.config, &request) {
			Ok(label) => label.clone(),
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

		let live = self.live_for_label(&label.name(), &survey.fleet, names)
			+ pending.get(&label.name()).copied().unwrap_or(0);
		if let Err(denial) = policy::admit(&label, &request, live) {
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

		if survey.blind.contains(&label.provider) {
			warn!(handle = %job.handle, provider = ?label.provider, "held back: this provider's machines are not visible");
			return Ok(());
		}

		self.report(
			repo,
			&sha,
			run_url.as_deref(),
			StatusState::Pending,
			&format!("provisioning a {} machine", label.name()),
		)
		.await;

		let name = naming::machine_name(
			self.config.machine_prefix(),
			&label.name(),
			&job.handle,
		);
		let registration = self.forgejo.register_runner(repo, &name).await?;
		let user_data = cloudinit::render(
			&self.config.daemon,
			&label,
			&self.config.forgejo.url,
			&registration,
			&job.handle,
		);

		match self.place(&label, &name, &user_data).await {
			Ok((placement, machine)) => {
				self.unseen
					.insert(machine.name.clone(), (label.provider, machine));
				*pending.entry(label.name()).or_default() += 1;
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

	fn live_for_label(
		&self,
		name: &str,
		fleet: &Machines,
		names: &[String],
	) -> usize {
		fleet
			.iter()
			.filter(|(_, machine)| {
				naming::split(
					self.config.machine_prefix(),
					&machine.name,
					names,
				)
				.is_some_and(|(label, _)| label == name)
			})
			.count()
	}

	async fn place(
		&self,
		label: &Label,
		name: &str,
		user_data: &str,
	) -> Result<(String, Machine)> {
		let mut last = None;
		for placement in placements(label) {
			match self
				.clouds
				.create(
					label.provider,
					name,
					&placement.plan,
					&placement.location,
					&label.image,
					label.ssh_key.as_deref(),
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
			anyhow::anyhow!("{}: nowhere to place it", label.name())
		}))
	}
}
