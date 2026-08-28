use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::json;
use tracing::{error, warn};

const REPEAT_AFTER: Duration = Duration::from_secs(900);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
	SweepFailed,
	CreateFailed,
	PollFailed,
	Abandoned,
}

impl Kind {
	fn as_str(self) -> &'static str {
		match self {
			Self::SweepFailed => "sweep_failed",
			Self::CreateFailed => "create_failed",
			Self::PollFailed => "poll_failed",
			Self::Abandoned => "abandoned_machine",
		}
	}
}

pub struct Alerts {
	http: reqwest::Client,
	webhook: Option<String>,
	streaks: HashMap<(Kind, String), (u32, Instant)>,
}

impl Alerts {
	pub fn new(webhook: Option<String>) -> anyhow::Result<Self> {
		Ok(Self {
			http: reqwest::Client::builder()
				.timeout(Duration::from_secs(15))
				.build()?,
			webhook,
			streaks: HashMap::new(),
		})
	}

	pub async fn raise(&mut self, kind: Kind, subject: &str, detail: &str) {
		let key = (kind, subject.to_owned());
		let now = Instant::now();
		let (count, first) = self
			.streaks
			.entry(key.clone())
			.and_modify(|(count, _)| *count += 1)
			.or_insert((1, now));
		let count = *count;
		let elapsed = first.elapsed();

		if count == 1 {
			warn!(kind = kind.as_str(), subject, detail, "alert");
		} else {
			error!(
				kind = kind.as_str(),
				subject,
				detail,
				count,
				minutes = elapsed.as_secs() / 60,
				"alert repeating"
			);
		}

		if count > 1 && elapsed < REPEAT_AFTER {
			return;
		}
		if count > 1 {
			self.streaks.insert(key, (count, now));
		}
		self.post(kind, subject, detail, count).await;
	}

	pub fn clear(&mut self, kind: Kind, subject: &str) {
		self.streaks.remove(&(kind, subject.to_owned()));
	}

	async fn post(&self, kind: Kind, subject: &str, detail: &str, count: u32) {
		let Some(url) = &self.webhook else { return };
		let body = json!({
			"kind": kind.as_str(),
			"subject": subject,
			"detail": detail,
			"consecutive": count,
		});
		if let Err(error) = self.http.post(url).json(&body).send().await {
			let error = anyhow::Error::new(error);
			warn!(error = %format!("{error:#}"), "posting alert webhook failed");
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn first_failure_warns_and_repeats_escalate() {
		let mut alerts = Alerts::new(None).unwrap();
		alerts.raise(Kind::SweepFailed, "box-1", "boom").await;
		assert_eq!(alerts.streaks[&(Kind::SweepFailed, "box-1".into())].0, 1);
		alerts.raise(Kind::SweepFailed, "box-1", "boom").await;
		assert_eq!(alerts.streaks[&(Kind::SweepFailed, "box-1".into())].0, 2);
	}

	#[tokio::test]
	async fn success_clears_the_streak() {
		let mut alerts = Alerts::new(None).unwrap();
		alerts.raise(Kind::SweepFailed, "box-1", "boom").await;
		alerts.clear(Kind::SweepFailed, "box-1");
		assert!(alerts.streaks.is_empty());
	}

	#[tokio::test]
	async fn streaks_are_tracked_per_subject() {
		let mut alerts = Alerts::new(None).unwrap();
		alerts.raise(Kind::SweepFailed, "box-1", "boom").await;
		alerts.raise(Kind::SweepFailed, "box-2", "boom").await;
		assert_eq!(alerts.streaks.len(), 2);
	}
}
