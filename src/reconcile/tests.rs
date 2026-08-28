use std::sync::Mutex;

use anyhow::Result;

use super::*;
use crate::config::{Provider, Repo};
use crate::forgejo::{Job, Registration, Run, Runner};
use time::OffsetDateTime;

use crate::provider::Machine;

const CONFIG: &str = include_str!("../../fixtures/config.toml");

fn repo(name: &str) -> Repo {
	Repo {
		owner: "acme".into(),
		name: name.into(),
	}
}

struct FakeQueue {
	repo: Repo,
	jobs: Result<Vec<Job>, String>,
	elsewhere: Vec<Job>,
	runners: Vec<Runner>,
	deleted: Mutex<Vec<i64>>,
	statuses: Mutex<Vec<(String, String)>>,
	polled: Mutex<Vec<String>>,
	registered: Mutex<Vec<(String, String)>>,
}

impl Default for FakeQueue {
	fn default() -> Self {
		Self {
			repo: repo("widgets"),
			jobs: Ok(Vec::new()),
			elsewhere: Vec::new(),
			runners: Vec::new(),
			deleted: Mutex::new(Vec::new()),
			statuses: Mutex::new(Vec::new()),
			polled: Mutex::new(Vec::new()),
			registered: Mutex::new(Vec::new()),
		}
	}
}

impl Queue for FakeQueue {
	async fn waiting_jobs(
		&self,
		repo: &Repo,
		_: &[String],
	) -> Result<Vec<Job>> {
		self.polled.lock().unwrap().push(repo.to_string());
		if repo != &self.repo {
			return Ok(self.elsewhere.clone());
		}
		self.jobs
			.clone()
			.map_err(|error| anyhow::anyhow!("{error}"))
	}
	async fn run(&self, _: &Repo, _: i64) -> Result<Run> {
		Ok(Run {
			event: "pull_request".into(),
			trigger_event: String::new(),
			is_fork_pull_request: true,
			commit_sha: Some("sha".into()),
			event_payload: Some(
				serde_json::json!({"pull_request": {"head": {"sha": "head"}}})
					.to_string(),
			),
			html_url: None,
		})
	}
	async fn register_runner(
		&self,
		repo: &Repo,
		name: &str,
	) -> Result<Registration> {
		self.registered
			.lock()
			.unwrap()
			.push((repo.to_string(), name.into()));
		Ok(Registration {
			id: 1,
			uuid: "u".into(),
			token: "t".into(),
		})
	}
	async fn runners(&self, repo: &Repo) -> Result<Vec<Runner>> {
		if repo != &self.repo {
			return Ok(Vec::new());
		}
		Ok(self.runners.clone())
	}
	async fn delete_runner(&self, _: &Repo, id: i64) -> Result<()> {
		self.deleted.lock().unwrap().push(id);
		Ok(())
	}
	async fn set_status(
		&self,
		_: &Repo,
		_: &str,
		state: StatusState,
		_: &str,
		description: &str,
		_: Option<&str>,
	) -> Result<()> {
		self.statuses
			.lock()
			.unwrap()
			.push((format!("{state:?}"), description.into()));
		Ok(())
	}
}

struct FakeFleet {
	machines: Vec<Machine>,
	list_fails: bool,
	destroyed: Mutex<Vec<String>>,
	created: Mutex<Vec<String>>,
}

impl FakeFleet {
	fn with(machines: Vec<Machine>) -> Self {
		Self {
			machines,
			list_fails: false,
			destroyed: Mutex::new(Vec::new()),
			created: Mutex::new(Vec::new()),
		}
	}
}

impl Fleet for FakeFleet {
	fn kinds(&self) -> Vec<Provider> {
		vec![Provider::Hetzner]
	}
	async fn list(&self, _: Provider, _: &str) -> Result<Vec<Machine>> {
		if self.list_fails {
			anyhow::bail!("provider unreachable");
		}
		Ok(self.machines.clone())
	}
	async fn create(
		&self,
		_: Provider,
		name: &str,
		_: &str,
		_: &str,
		_: &str,
		_: Option<&str>,
		_: &str,
	) -> Result<Machine> {
		self.created.lock().unwrap().push(name.into());
		Ok(Machine {
			id: "new".into(),
			name: name.into(),
			created_at: Some(OffsetDateTime::now_utc()),
		})
	}
	async fn destroy(&self, _: Provider, id: &str) -> Result<()> {
		self.destroyed.lock().unwrap().push(id.into());
		Ok(())
	}
}

fn job(handle: &str) -> Job {
	labelled_job(handle, "check")
}

fn labelled_job(handle: &str, label: &str) -> Job {
	Job {
		run_id: 1,
		handle: handle.into(),
		status: "waiting".into(),
		runs_on: vec![label.into()],
	}
}

fn machine(handle: &str) -> Machine {
	Machine {
		id: format!("id-{handle}"),
		name: naming::machine_name("check", handle),
		created_at: Some(OffsetDateTime::now_utc()),
	}
}

fn aged_machine(handle: &str, minutes: i64) -> Machine {
	Machine {
		created_at: Some(
			OffsetDateTime::now_utc() - time::Duration::minutes(minutes),
		),
		..machine(handle)
	}
}

fn orchestrator(
	queue: FakeQueue,
	fleet: FakeFleet,
) -> Orchestrator<FakeQueue, FakeFleet> {
	Orchestrator::new(
		Config::parse(CONFIG).unwrap(),
		queue,
		fleet,
		Alerts::new(None).unwrap(),
	)
}

#[test]
fn logs_a_stable_refusal_once_and_again_after_it_settles() {
	let mut orc = orchestrator(FakeQueue::default(), FakeFleet::with(vec![]));
	assert!(orc.refusals.is_empty());

	orc.log_refusal("h1", "label build-hetzner already has 1 machine(s)");
	orc.log_refusal("h1", "label build-hetzner already has 1 machine(s)");
	assert_eq!(orc.refusals.len(), 1, "a repeat must not re-log");

	orc.log_refusal("h1", "a different reason");
	assert_eq!(orc.refusals["h1"], "a different reason");

	orc.forget_settled_refusals(&[]);
	assert!(
		orc.refusals.is_empty(),
		"a handle that left the queue must log again if it returns"
	);
}

#[tokio::test]
async fn destroys_a_machine_whose_job_left_the_queue() {
	let queue = FakeQueue {
		jobs: Ok(vec![]),
		..Default::default()
	};
	let fleet = FakeFleet::with(vec![machine("gone")]);
	let mut orc = orchestrator(queue, fleet);
	orc.tick().await;
	assert!(
		orc.clouds.destroyed.lock().unwrap().is_empty(),
		"one missing poll is not yet proof the job is over"
	);
	orc.tick().await;
	assert_eq!(orc.clouds.destroyed.lock().unwrap().as_slice(), ["id-gone"]);
}

#[tokio::test]
async fn keeps_a_machine_whose_job_is_still_queued() {
	let queue = FakeQueue {
		jobs: Ok(vec![job("live")]),
		..Default::default()
	};
	let fleet = FakeFleet::with(vec![machine("live")]);
	let mut orc = orchestrator(queue, fleet);
	orc.tick().await;
	assert!(orc.clouds.destroyed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_failed_queue_poll_never_destroys_anything() {
	let queue = FakeQueue {
		jobs: Err("forgejo down".into()),
		..Default::default()
	};
	let fleet = FakeFleet::with(vec![machine("gone")]);
	let mut orc = orchestrator(queue, fleet);
	orc.tick().await;
	assert!(
		orc.clouds.destroyed.lock().unwrap().is_empty(),
		"a machine was destroyed on the strength of a failed poll"
	);
}

#[tokio::test]
async fn an_unreachable_provider_holds_back_provisioning() {
	let queue = FakeQueue {
		jobs: Ok(vec![job("new")]),
		..Default::default()
	};
	let mut fleet = FakeFleet::with(vec![]);
	fleet.list_fails = true;
	let mut orc = orchestrator(queue, fleet);
	orc.tick().await;
	assert!(
		orc.clouds.created.lock().unwrap().is_empty(),
		"provisioned while blind to that provider's machines"
	);
}

#[tokio::test]
async fn destroys_a_machine_no_label_explains() {
	let queue = FakeQueue {
		jobs: Ok(vec![]),
		..Default::default()
	};
	let orphan = Machine {
		id: "id-orphan".into(),
		name: format!("{}-renamed-label-abc", naming::PREFIX),
		created_at: Some(OffsetDateTime::now_utc()),
	};
	let fleet = FakeFleet::with(vec![orphan]);
	let mut orc = orchestrator(queue, fleet);
	orc.tick().await;
	orc.tick().await;
	assert_eq!(
			orc.clouds.destroyed.lock().unwrap().as_slice(),
			["id-orphan"],
			"a machine carrying our prefix but no known label must not be left billing"
		);
}

#[tokio::test]
async fn provisions_for_a_queued_job() {
	let queue = FakeQueue {
		jobs: Ok(vec![job("new")]),
		..Default::default()
	};
	let fleet = FakeFleet::with(vec![]);
	let mut orc = orchestrator(queue, fleet);
	orc.tick().await;
	assert_eq!(
		orc.clouds.created.lock().unwrap().as_slice(),
		[naming::machine_name("check", "new")]
	);
}

#[tokio::test]
async fn does_not_provision_twice_for_the_same_job() {
	let queue = FakeQueue {
		jobs: Ok(vec![job("served")]),
		..Default::default()
	};
	let fleet = FakeFleet::with(vec![machine("served")]);
	let mut orc = orchestrator(queue, fleet);
	orc.tick().await;
	assert!(orc.clouds.created.lock().unwrap().is_empty());
}

#[tokio::test]
async fn respects_the_per_label_machine_cap() {
	let queue = FakeQueue {
		jobs: Ok(vec![job("a"), job("b")]),
		..Default::default()
	};
	let fleet = FakeFleet::with(vec![machine("a")]);
	let mut orc = orchestrator(queue, fleet);
	orc.tick().await;
	assert!(
		orc.clouds.created.lock().unwrap().is_empty(),
		"max_vms = 1 but a second machine was created"
	);
}

#[tokio::test]
async fn a_restart_does_not_immediately_destroy_live_machines() {
	let queue = FakeQueue {
		jobs: Ok(vec![job("live")]),
		..Default::default()
	};
	let fleet = FakeFleet::with(vec![machine("live")]);
	let mut orc = orchestrator(queue, fleet);
	orc.tick().await;
	assert!(orc.clouds.destroyed.lock().unwrap().is_empty());
	assert!(
		orc.missed_polls.is_empty(),
		"a live handle must clear any recorded miss"
	);
}

#[tokio::test]
async fn deletes_a_runner_record_with_no_machine() {
	let queue = FakeQueue {
		jobs: Ok(vec![]),
		runners: vec![Runner {
			id: 42,
			name: naming::machine_name("check", "vanished"),
			ephemeral: true,
		}],
		..Default::default()
	};
	let fleet = FakeFleet::with(vec![]);
	let mut orc = orchestrator(queue, fleet);
	orc.tick().await;
	assert!(
		orc.forgejo.deleted.lock().unwrap().is_empty(),
		"one listing without the machine is not yet proof it is gone"
	);
	orc.tick().await;
	assert_eq!(orc.forgejo.deleted.lock().unwrap().as_slice(), [42]);
}

#[tokio::test]
async fn keeps_a_runner_whose_machine_the_provider_has_not_listed_yet() {
	let name = naming::machine_name("check", "fresh");
	let queue = FakeQueue {
		jobs: Ok(vec![]),
		runners: vec![Runner {
			id: 42,
			name: name.clone(),
			ephemeral: true,
		}],
		..Default::default()
	};
	let mut orc = orchestrator(queue, FakeFleet::with(vec![]));

	orc.tick().await;
	orc.clouds.machines = vec![machine("fresh")];
	orc.tick().await;

	assert!(
		orc.forgejo.deleted.lock().unwrap().is_empty(),
		"a machine that surfaced one tick late must keep its registration"
	);
	assert!(
		!orc.missed_machines.contains_key(&name),
		"seeing the machine must clear the streak"
	);
}

#[tokio::test]
async fn keeps_a_runner_whose_provider_lists_the_machine_minutes_late() {
	let name = naming::machine_name("check", "slowlist");
	let queue = FakeQueue {
		jobs: Ok(vec![]),
		runners: vec![Runner {
			id: 42,
			name: name.clone(),
			ephemeral: true,
		}],
		..Default::default()
	};
	let mut orc = orchestrator(queue, FakeFleet::with(vec![]));
	orc.config.daemon.reconcile_grace_secs = 300;

	for _ in 0..8 {
		orc.tick().await;
	}
	assert!(
		orc.forgejo.deleted.lock().unwrap().is_empty(),
		"a miss streak alone must not revoke a registration inside the grace"
	);
	assert_eq!(orc.missed_machines[&name].0, 8, "the streak still counts");
}

#[tokio::test]
async fn polls_only_allowlisted_repositories() {
	let mut orchestrator =
		orchestrator(FakeQueue::default(), FakeFleet::with(Vec::new()));
	orchestrator.tick().await;

	assert_eq!(
		*orchestrator.forgejo.polled.lock().unwrap(),
		vec!["acme/widgets", "acme/gadgets"]
	);
}

#[tokio::test]
async fn provisions_for_every_allowlisted_repository() {
	let queue = FakeQueue {
		jobs: Ok(vec![labelled_job("A", "roomy")]),
		elsewhere: vec![labelled_job("B", "roomy")],
		..Default::default()
	};
	let mut orchestrator = orchestrator(queue, FakeFleet::with(Vec::new()));
	orchestrator.tick().await;

	let created = orchestrator.clouds.created.lock().unwrap().clone();
	assert_eq!(
		created,
		vec![
			naming::machine_name("roomy", "A"),
			naming::machine_name("roomy", "B")
		]
	);
}

#[tokio::test]
async fn registers_the_runner_in_the_repository_that_asked_for_it() {
	let queue = FakeQueue {
		jobs: Ok(Vec::new()),
		elsewhere: vec![job("B")],
		..Default::default()
	};
	let mut orchestrator = orchestrator(queue, FakeFleet::with(Vec::new()));
	orchestrator.tick().await;

	assert_eq!(
		*orchestrator.forgejo.registered.lock().unwrap(),
		vec![(
			"acme/gadgets".to_string(),
			naming::machine_name("check", "B")
		)]
	);
}

#[tokio::test]
async fn one_repository_failing_to_poll_holds_back_every_sweep() {
	let queue = FakeQueue {
		jobs: Err("forgejo down".into()),
		elsewhere: vec![job("B")],
		..Default::default()
	};
	let mut orchestrator =
		orchestrator(queue, FakeFleet::with(vec![machine("gone")]));
	orchestrator.tick().await;

	assert!(
		orchestrator.clouds.destroyed.lock().unwrap().is_empty(),
		"a machine may belong to the queue we could not read"
	);
}

#[tokio::test]
async fn provisions_once_for_a_handle_listed_twice() {
	let queue = FakeQueue {
		jobs: Ok(vec![job("A"), job("A")]),
		..Default::default()
	};
	let mut orchestrator = orchestrator(queue, FakeFleet::with(Vec::new()));
	orchestrator.tick().await;

	assert_eq!(
		*orchestrator.clouds.created.lock().unwrap(),
		vec![naming::machine_name("check", "A")]
	);
}

#[tokio::test]
async fn the_machine_cap_counts_machines_made_earlier_in_the_same_tick() {
	let queue = FakeQueue {
		jobs: Ok(vec![job("a"), job("b")]),
		..Default::default()
	};
	let mut orchestrator = orchestrator(queue, FakeFleet::with(Vec::new()));
	orchestrator.tick().await;

	assert_eq!(
		orchestrator.clouds.created.lock().unwrap().len(),
		1,
		"max_vms = 1, but the tick-start snapshot showed an empty fleet for both jobs"
	);
	let statuses = orchestrator.forgejo.statuses.lock().unwrap().clone();
	assert!(
		statuses
			.iter()
			.any(|(_, d)| d.contains("already has 1 machine(s) running")),
		"the second job should be told why it was refused: {statuses:?}"
	);
}

#[tokio::test]
async fn destroys_a_machine_past_its_lifetime_even_while_its_job_is_queued() {
	let queue = FakeQueue {
		jobs: Ok(vec![job("old")]),
		..Default::default()
	};
	let fleet = FakeFleet::with(vec![aged_machine("old", 200)]);
	let mut orchestrator = orchestrator(queue, fleet);
	orchestrator.tick().await;

	assert_eq!(
		*orchestrator.clouds.destroyed.lock().unwrap(),
		vec!["id-old"],
		"a powered-off machine still bills, so age must win over queue presence"
	);
}

#[tokio::test]
async fn an_unreadable_queue_still_reaps_an_overdue_machine() {
	let queue = FakeQueue {
		jobs: Err("forgejo down".into()),
		..Default::default()
	};
	let fleet = FakeFleet::with(vec![aged_machine("old", 200)]);
	let mut orchestrator = orchestrator(queue, fleet);
	orchestrator.tick().await;

	assert_eq!(
		*orchestrator.clouds.destroyed.lock().unwrap(),
		vec!["id-old"]
	);
}

#[tokio::test]
async fn leaves_a_machine_that_is_still_within_its_lifetime() {
	let queue = FakeQueue {
		jobs: Ok(vec![job("young")]),
		..Default::default()
	};
	let fleet = FakeFleet::with(vec![aged_machine("young", 5)]);
	let mut orchestrator = orchestrator(queue, fleet);
	orchestrator.tick().await;

	assert!(orchestrator.clouds.destroyed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_running_job_keeps_its_machine() {
	let mut running = job("busy");
	running.status = "running".into();
	let queue = FakeQueue {
		jobs: Ok(vec![running]),
		..Default::default()
	};
	let fleet = FakeFleet::with(vec![machine("busy")]);
	let mut orc = orchestrator(queue, fleet);

	for _ in 0..5 {
		orc.tick().await;
	}

	assert!(
		orc.clouds.destroyed.lock().unwrap().is_empty(),
		"Forgejo returns waiting AND running jobs, so liveness must ignore status"
	);
	assert!(
		orc.clouds.created.lock().unwrap().is_empty(),
		"a running job must not be provisioned a second machine"
	);
}
