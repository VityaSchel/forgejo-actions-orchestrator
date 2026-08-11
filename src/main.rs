mod alert;
mod cli;
mod cloudinit;
mod config;
mod forgejo;
mod naming;
mod policy;
mod provider;
mod reconcile;
mod secret;

use anyhow::Result;
use futures::FutureExt;

use crate::config::Repo;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(
			EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
		)
		.init();

	let path = match cli::parse(std::env::args().skip(1))? {
		cli::Invocation::Run(path) => path,
		cli::Invocation::Usage => {
			println!("{}", cli::USAGE);
			return Ok(());
		}
	};
	let config = config::Config::load(&path)?;

	let forgejo = forgejo::Client::new(
		&config.forgejo.url,
		secret::load("FORGEJO_RUNNER_TOKEN")?,
		secret::load("FORGEJO_STATUS_TOKEN")?,
	)?;
	let clouds = provider::Clouds::from_env(&config.labels)?;
	let alerts = alert::Alerts::new(
		config.alert.as_ref().and_then(|a| a.webhook_url.clone()),
	)?;

	let interval = config.poll_interval();
	info!(
		repos = ?config.repos.iter().map(Repo::to_string).collect::<Vec<_>>(),
		labels = ?config.all_labels(),
		interval = ?interval,
		"watching"
	);

	let mut orchestrator =
		reconcile::Orchestrator::new(config, forgejo, clouds, alerts);
	let mut ticker = tokio::time::interval(interval);
	ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

	loop {
		tokio::select! {
			_ = ticker.tick() => {
				let caught = std::panic::AssertUnwindSafe(orchestrator.tick())
					.catch_unwind()
					.await;
				if caught.is_err() {
					error!("tick panicked; the loop continues so machines still get reconciled");
				}
			}
			_ = tokio::signal::ctrl_c() => {
				info!("stopping; machines are left for the next run to reconcile");
				return Ok(());
			}
		}
	}
}
