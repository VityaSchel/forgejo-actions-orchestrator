use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;

use crate::config::Repo;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Client {
	http: reqwest::Client,
	url: String,
	runner_token: String,
	status_token: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Job {
	pub run_id: i64,
	pub handle: String,
	pub status: String,
	#[serde(default)]
	pub runs_on: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Run {
	#[serde(default)]
	pub event: String,
	#[serde(default)]
	pub trigger_event: String,
	#[serde(default)]
	pub is_fork_pull_request: bool,
	#[serde(default)]
	pub commit_sha: Option<String>,
	#[serde(default)]
	pub event_payload: Option<String>,
	#[serde(default)]
	pub html_url: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Registration {
	pub id: i64,
	pub uuid: String,
	pub token: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Runner {
	pub id: i64,
	pub name: String,
	#[serde(default)]
	pub ephemeral: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum StatusState {
	Pending,
	Error,
	Success,
}

impl StatusState {
	fn as_str(self) -> &'static str {
		match self {
			Self::Pending => "pending",
			Self::Error => "error",
			Self::Success => "success",
		}
	}
}

#[allow(async_fn_in_trait)]
pub trait Queue {
	async fn waiting_jobs(
		&self,
		repo: &Repo,
		labels: &[String],
	) -> Result<Vec<Job>>;
	async fn run(&self, repo: &Repo, run_id: i64) -> Result<Run>;
	async fn register_runner(
		&self,
		repo: &Repo,
		name: &str,
	) -> Result<Registration>;
	async fn runners(&self, repo: &Repo) -> Result<Vec<Runner>>;
	async fn delete_runner(&self, repo: &Repo, id: i64) -> Result<()>;
	async fn set_status(
		&self,
		repo: &Repo,
		sha: &str,
		state: StatusState,
		context: &str,
		description: &str,
		target_url: Option<&str>,
	) -> Result<()>;
}

impl Queue for Client {
	async fn waiting_jobs(
		&self,
		repo: &Repo,
		labels: &[String],
	) -> Result<Vec<Job>> {
		Client::waiting_jobs(self, repo, labels).await
	}
	async fn run(&self, repo: &Repo, run_id: i64) -> Result<Run> {
		Client::run(self, repo, run_id).await
	}
	async fn register_runner(
		&self,
		repo: &Repo,
		name: &str,
	) -> Result<Registration> {
		Client::register_runner(self, repo, name).await
	}
	async fn runners(&self, repo: &Repo) -> Result<Vec<Runner>> {
		Client::runners(self, repo).await
	}
	async fn delete_runner(&self, repo: &Repo, id: i64) -> Result<()> {
		Client::delete_runner(self, repo, id).await
	}
	async fn set_status(
		&self,
		repo: &Repo,
		sha: &str,
		state: StatusState,
		context: &str,
		description: &str,
		target_url: Option<&str>,
	) -> Result<()> {
		Client::set_status(
			self,
			repo,
			sha,
			state,
			context,
			description,
			target_url,
		)
		.await
	}
}

impl Run {
	pub fn triggering_event(&self) -> &str {
		match self.trigger_event.as_str() {
			"workflow_dispatch" | "schedule" | "package" => &self.trigger_event,
			_ if self.event.is_empty() => &self.trigger_event,
			_ => &self.event,
		}
	}
}

fn require_ephemeral(runner: &Runner, name: &str) -> Result<()> {
	if runner.ephemeral {
		return Ok(());
	}
	let id = runner.id;
	bail!(
        "{name}: runner {id} is not ephemeral. Untrusted code runs as root on this machine and can \
         read its runner credential; only the ephemeral flag bars that credential from claiming \
         another job. Refusing to boot."
    );
}

impl Client {
	pub fn new(
		url: &str,
		runner_token: String,
		status_token: String,
	) -> Result<Self> {
		Ok(Self {
			http: reqwest::Client::builder()
				.user_agent(concat!(
					env!("CARGO_PKG_NAME"),
					"/",
					env!("CARGO_PKG_VERSION")
				))
				.timeout(REQUEST_TIMEOUT)
				.build()?,
			url: url.trim_end_matches('/').to_owned(),
			runner_token,
			status_token,
		})
	}

	fn api(&self, repo: &Repo) -> String {
		format!("{}/api/v1/repos/{}/{}", self.url, repo.owner, repo.name)
	}

	pub async fn waiting_jobs(
		&self,
		repo: &Repo,
		labels: &[String],
	) -> Result<Vec<Job>> {
		let url = format!("{}/actions/runners/jobs", self.api(repo));
		let response = self
			.http
			.get(&url)
			.query(&[("labels", labels.join(","))])
			.header("Authorization", self.runner_auth())
			.send()
			.await
			.context("polling jobs")?;
		let jobs: Option<Vec<Job>> =
			self.decode(response, "polling jobs").await?;
		Ok(jobs.unwrap_or_default())
	}

	pub async fn run(&self, repo: &Repo, run_id: i64) -> Result<Run> {
		let url = format!("{}/actions/runs/{run_id}", self.api(repo));
		let response = self
			.http
			.get(&url)
			.header("Authorization", self.runner_auth())
			.send()
			.await
			.context("fetching run")?;
		self.decode(response, "fetching run").await
	}

	pub async fn register_runner(
		&self,
		repo: &Repo,
		name: &str,
	) -> Result<Registration> {
		let url = format!("{}/actions/runners", self.api(repo));
		let response = self
			.http
			.post(&url)
			.header("Authorization", self.runner_auth())
			.json(&json!({ "name": name, "ephemeral": true }))
			.send()
			.await
			.context("registering runner")?;
		let registration: Registration =
			self.decode(response, "registering runner").await?;
		self.assert_ephemeral(repo, registration.id, name).await?;
		Ok(registration)
	}

	async fn assert_ephemeral(
		&self,
		repo: &Repo,
		id: i64,
		name: &str,
	) -> Result<()> {
		let url = format!("{}/actions/runners/{id}", self.api(repo));
		let response = self
			.http
			.get(&url)
			.header("Authorization", self.runner_auth())
			.send()
			.await
			.context("reading runner back")?;
		let runner: Runner =
			self.decode(response, "reading runner back").await?;
		require_ephemeral(&runner, name)
	}

	pub async fn runners(&self, repo: &Repo) -> Result<Vec<Runner>> {
		let url = format!("{}/actions/runners", self.api(repo));
		let response = self
			.http
			.get(&url)
			.query(&[("limit", "100")])
			.header("Authorization", self.runner_auth())
			.send()
			.await
			.context("listing runners")?;
		let runners: Option<Vec<Runner>> =
			self.decode(response, "listing runners").await?;
		Ok(runners.unwrap_or_default())
	}

	pub async fn delete_runner(&self, repo: &Repo, id: i64) -> Result<()> {
		let url = format!("{}/actions/runners/{id}", self.api(repo));
		let response = self
			.http
			.delete(&url)
			.header("Authorization", self.runner_auth())
			.send()
			.await
			.context("deleting runner")?;
		if response.status() == reqwest::StatusCode::NOT_FOUND {
			return Ok(());
		}
		self.expect_success(response, "deleting runner").await
	}

	pub async fn set_status(
		&self,
		repo: &Repo,
		sha: &str,
		state: StatusState,
		context: &str,
		description: &str,
		target_url: Option<&str>,
	) -> Result<()> {
		let url = format!("{}/statuses/{sha}", self.api(repo));
		let response = self
			.http
			.post(&url)
			.header("Authorization", format!("token {}", self.status_token))
			.json(&json!({
				"state": state.as_str(),
				"context": context,
				"description": truncate(description, 100),
				"target_url": target_url.unwrap_or_default(),
			}))
			.send()
			.await
			.context("posting commit status")?;
		self.expect_success(response, "posting commit status").await
	}

	fn runner_auth(&self) -> String {
		format!("token {}", self.runner_token)
	}

	async fn decode<T: serde::de::DeserializeOwned>(
		&self,
		response: reqwest::Response,
		what: &str,
	) -> Result<T> {
		let status = response.status();
		let body = response.text().await.unwrap_or_default();
		if !status.is_success() {
			bail!("{what}: HTTP {status}: {}", truncate(&body, 200));
		}
		serde_json::from_str(&body).with_context(|| {
			format!("{what}: decoding {}", truncate(&body, 200))
		})
	}

	async fn expect_success(
		&self,
		response: reqwest::Response,
		what: &str,
	) -> Result<()> {
		let status = response.status();
		if status.is_success() {
			return Ok(());
		}
		let body = response.text().await.unwrap_or_default();
		bail!("{what}: HTTP {status}: {}", truncate(&body, 200))
	}
}

pub fn status_target_sha(run: &Run) -> Option<String> {
	let payload: serde_json::Value =
		serde_json::from_str(run.event_payload.as_deref()?).ok()?;
	let from_payload = match run.triggering_event() {
		"pull_request" | "pull_request_target" | "pull_request_sync" => {
			payload["pull_request"]["head"]["sha"].as_str()
		}
		"push" => payload["head_commit"]["id"].as_str(),
		_ => None,
	};
	from_payload
		.map(str::to_owned)
		.or_else(|| run.commit_sha.clone())
}

fn truncate(value: &str, limit: usize) -> String {
	if value.chars().count() <= limit {
		return value.to_owned();
	}
	value.chars().take(limit).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
	use std::io::{BufRead, BufReader, Write};
	use std::net::{TcpListener, TcpStream};

	use super::*;

	fn run(event: &str, payload: serde_json::Value) -> Run {
		Run {
			event: event.into(),
			trigger_event: String::new(),
			is_fork_pull_request: false,
			commit_sha: Some("run-commit".into()),
			event_payload: Some(payload.to_string()),
			html_url: None,
		}
	}

	const REGISTRATION_THEN_READ_BACK: usize = 2;
	const REGISTRATION_RESPONSE: &str =
		r#"{"id":7,"uuid":"u-1","token":"tok"}"#;

	#[derive(Clone, Copy)]
	enum EphemeralFlag {
		AsRegistered,
		IgnoredByTheServer,
	}

	enum RunnerRequest {
		Registration { asked_for_ephemeral: bool },
		ReadBack,
	}

	struct RunnerRegistry {
		ephemeral_flag: EphemeralFlag,
		runner_is_ephemeral: bool,
	}

	impl RunnerRegistry {
		fn new(ephemeral_flag: EphemeralFlag) -> Self {
			Self {
				ephemeral_flag,
				runner_is_ephemeral: false,
			}
		}

		fn respond_to(&mut self, request: RunnerRequest) -> String {
			match request {
				RunnerRequest::Registration {
					asked_for_ephemeral,
				} => {
					self.runner_is_ephemeral = match self.ephemeral_flag {
						EphemeralFlag::AsRegistered => asked_for_ephemeral,
						EphemeralFlag::IgnoredByTheServer => false,
					};
					REGISTRATION_RESPONSE.to_owned()
				}
				RunnerRequest::ReadBack => {
					read_back_response(self.runner_is_ephemeral)
				}
			}
		}
	}

	fn fake_forgejo(ephemeral_flag: EphemeralFlag) -> String {
		let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
		let address = listener.local_addr().expect("addr");
		std::thread::spawn(move || {
			let mut registry = RunnerRegistry::new(ephemeral_flag);
			let connections =
				listener.incoming().take(REGISTRATION_THEN_READ_BACK);
			for connection in connections {
				let Ok(mut stream) = connection else { continue };
				let mut reader = BufReader::new(
					stream.try_clone().expect("clone the accepted socket"),
				);
				let request = read_runner_request(&mut reader);
				respond_with_json(&mut stream, &registry.respond_to(request));
			}
		});
		format!("http://{address}")
	}

	fn read_runner_request(reader: &mut impl BufRead) -> RunnerRequest {
		let mut request_line = String::new();
		reader.read_line(&mut request_line).expect("request line");
		let headers = read_headers(reader);
		if !request_line.starts_with("POST ") {
			return RunnerRequest::ReadBack;
		}
		let registration = read_json_body(reader, content_length(&headers));
		RunnerRequest::Registration {
			asked_for_ephemeral: registration["ephemeral"] == json!(true),
		}
	}

	fn read_headers(reader: &mut impl BufRead) -> Vec<String> {
		let mut headers = Vec::new();
		loop {
			let mut header = String::new();
			reader.read_line(&mut header).expect("header");
			if header.trim().is_empty() {
				return headers;
			}
			headers.push(header.to_ascii_lowercase());
		}
	}

	fn content_length(headers: &[String]) -> usize {
		headers
			.iter()
			.find_map(|header| header.strip_prefix("content-length:"))
			.map_or(0, |value| value.trim().parse().expect("content-length"))
	}

	fn read_json_body(
		reader: &mut impl BufRead,
		content_length: usize,
	) -> serde_json::Value {
		let mut payload = vec![0u8; content_length];
		reader.read_exact(&mut payload).expect("request body");
		serde_json::from_slice(&payload).expect("request json")
	}

	fn read_back_response(ephemeral: bool) -> String {
		format!(r#"{{"id":7,"name":"check-1","ephemeral":{ephemeral}}}"#)
	}

	fn respond_with_json(stream: &mut TcpStream, body: &str) {
		let _ = write!(
			stream,
			"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
			 content-length: {}\r\nconnection: close\r\n\r\n{body}",
			body.len()
		);
	}

	fn repo() -> Repo {
		Repo {
			owner: "acme".into(),
			name: "widgets".into(),
		}
	}

	#[tokio::test]
	async fn registration_reads_the_runner_back_and_accepts_an_ephemeral_one() {
		let url = fake_forgejo(EphemeralFlag::AsRegistered);
		let client =
			Client::new(&url, "runner-tok".into(), "status-tok".into())
				.unwrap();

		let registration =
			client.register_runner(&repo(), "check-1").await.unwrap();

		assert_eq!(registration.id, 7);
	}

	#[tokio::test]
	async fn registration_fails_when_the_read_back_says_it_is_not_ephemeral() {
		let url = fake_forgejo(EphemeralFlag::IgnoredByTheServer);
		let client =
			Client::new(&url, "runner-tok".into(), "status-tok".into())
				.unwrap();

		let error = client
			.register_runner(&repo(), "check-1")
			.await
			.expect_err(
				"a non-ephemeral runner must abort before any VM exists",
			)
			.to_string();

		assert!(error.contains("not ephemeral"), "got {error}");
	}

	#[test]
	fn a_runner_the_server_reports_as_ephemeral_is_accepted() {
		let runner = Runner {
			id: 7,
			name: "check-1".into(),
			ephemeral: true,
		};

		assert!(require_ephemeral(&runner, "check-1").is_ok());
	}

	#[test]
	fn a_runner_that_is_not_ephemeral_refuses_to_boot() {
		let runner = Runner {
			id: 7,
			name: "check-1".into(),
			ephemeral: false,
		};

		let error = require_ephemeral(&runner, "check-1")
			.unwrap_err()
			.to_string();
		assert!(error.contains("check-1"), "got {error}");
		assert!(error.contains("7"), "got {error}");
	}

	#[test]
	fn a_response_that_omits_the_flag_is_refused_rather_than_assumed() {
		let runner: Runner =
			serde_json::from_str(r#"{"id":7,"name":"check-1"}"#).unwrap();

		assert!(!runner.ephemeral);
		assert!(require_ephemeral(&runner, "check-1").is_err());
	}

	#[test]
	fn takes_the_pull_request_head_not_the_run_commit() {
		let run = run(
			"pull_request",
			json!({ "pull_request": { "head": { "sha": "head-sha" } } }),
		);
		assert_eq!(status_target_sha(&run).as_deref(), Some("head-sha"));
	}

	#[test]
	fn takes_the_head_commit_for_a_push() {
		let run = run("push", json!({ "head_commit": { "id": "push-sha" } }));
		assert_eq!(status_target_sha(&run).as_deref(), Some("push-sha"));
	}

	#[test]
	fn falls_back_to_the_run_commit_for_other_events() {
		let run = run("workflow_dispatch", json!({}));
		assert_eq!(status_target_sha(&run).as_deref(), Some("run-commit"));
	}

	#[test]
	fn has_no_sha_when_there_is_nothing_to_use() {
		let mut run = run("workflow_dispatch", json!({}));
		run.commit_sha = None;
		assert_eq!(status_target_sha(&run), None);
	}

	const PUSH: &str = include_str!("../fixtures/forgejo/run-push.json");
	const DISPATCH: &str =
		include_str!("../fixtures/forgejo/run-workflow-dispatch.json");

	#[test]
	fn a_push_run_reports_its_event() {
		let run: Run = serde_json::from_str(PUSH).unwrap();
		assert_eq!(run.triggering_event(), "push");
		assert!(!run.is_fork_pull_request);
		assert!(run.commit_sha.is_some());
		assert!(run.html_url.is_some());
	}

	#[test]
	fn a_manual_dispatch_names_itself_only_in_trigger_event() {
		let run: Run = serde_json::from_str(DISPATCH).unwrap();
		assert_eq!(run.event, "", "the fixture records the real empty field");
		assert_eq!(
			run.triggering_event(),
			"workflow_dispatch",
			"reading only `event` refuses every manually dispatched run"
		);
	}

	const SCHEDULE: &str =
		include_str!("../fixtures/forgejo/run-schedule.json");

	#[test]
	fn a_scheduled_run_is_not_mistaken_for_a_push() {
		let run: Run = serde_json::from_str(SCHEDULE).unwrap();
		assert_eq!(
			run.event, "push",
			"the fixture records what Forgejo really sends"
		);
		assert_eq!(
			run.triggering_event(),
			"schedule",
			"a cron run must not spend a push allowance"
		);
	}

	#[test]
	fn a_pull_request_synchronize_stays_a_pull_request() {
		let run = Run {
			event: "pull_request".into(),
			trigger_event: "pull_request_sync".into(),
			is_fork_pull_request: false,
			commit_sha: None,
			event_payload: None,
			html_url: None,
		};
		assert_eq!(
			run.triggering_event(),
			"pull_request",
			"Forgejo normalizes PR sub-events; allowed_events lists the normalized name"
		);
	}

	#[test]
	fn parses_a_jobs_payload() {
		let jobs: Vec<Job> = serde_json::from_str(
			r#"[{"run_id":2,"handle":"H","status":"waiting","runs_on":["check"]}]"#,
		)
		.unwrap();
		assert_eq!(jobs[0].handle, "H");
		assert_eq!(jobs[0].runs_on, vec!["check"]);
	}

	#[test]
	fn an_empty_job_queue_arrives_as_null() {
		let jobs: Option<Vec<Job>> = serde_json::from_str("null").unwrap();
		assert!(jobs.unwrap_or_default().is_empty());
		assert!(
			serde_json::from_str::<Vec<Job>>("null").is_err(),
			"a bare Vec would reject null and stall every poll"
		);
	}

	#[test]
	fn truncates_long_descriptions() {
		assert_eq!(truncate("abcdef", 3), "abc…");
		assert_eq!(truncate("ab", 3), "ab");
	}
}
