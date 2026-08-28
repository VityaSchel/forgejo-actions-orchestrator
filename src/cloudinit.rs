use serde::Serialize;

use crate::config::{Daemon, Label};
use crate::forgejo::Registration;

const BOOT: &str = include_str!("boot.sh");
const ETC: &str = concat!("/etc/", env!("CARGO_PKG_NAME"));
const RUNNER_PATH: &str = "/usr/local/bin/forgejo-runner";

#[derive(Serialize)]
struct CloudConfig {
	write_files: Vec<WriteFile>,
	runcmd: Vec<Vec<String>>,
	power_state: PowerState,
}

#[derive(Serialize)]
struct WriteFile {
	path: String,
	content: String,
	permissions: &'static str,
	owner: &'static str,
}

#[derive(Serialize)]
struct PowerState {
	mode: &'static str,
	delay: u64,
	condition: bool,
}

impl WriteFile {
	fn secret(name: &str, content: String) -> Self {
		Self {
			path: format!("{ETC}/{name}"),
			content,
			permissions: "0600",
			owner: "root:root",
		}
	}

	fn readable(name: &str, content: String) -> Self {
		Self {
			permissions: "0644",
			..Self::secret(name, content)
		}
	}

	fn script(name: &str, content: String) -> Self {
		Self {
			permissions: "0700",
			..Self::secret(name, content)
		}
	}
}

fn runner_url(version: &str, arch: &str) -> String {
	format!(
		"https://code.forgejo.org/forgejo/runner/releases/download/\
		 v{version}/forgejo-runner-{version}-linux-{arch}"
	)
}

pub fn render(
	daemon: &Daemon,
	label: &Label,
	forgejo_url: &str,
	registration: &Registration,
	handle: &str,
) -> String {
	let version = &daemon.runner_version;
	let config = CloudConfig {
		write_files: vec![
			WriteFile::secret("runner-token", registration.token.clone()),
			WriteFile::script("boot.sh", BOOT.to_owned()),
			WriteFile::readable("forgejo-url", forgejo_url.to_owned()),
			WriteFile::readable("runner-uuid", registration.uuid.clone()),
			WriteFile::readable(
				"runner-labels",
				label
					.labels
					.iter()
					.map(|one| format!("{one}:host\n"))
					.collect::<String>(),
			),
			WriteFile::readable("job-handle", handle.to_owned()),
			WriteFile::readable(
				"runner-config.yml",
				format!("runner:\n  timeout: {}m\n", label.job_timeout()),
			),
			WriteFile::readable(
				"runner-url-amd64",
				runner_url(version, "amd64"),
			),
			WriteFile::readable(
				"runner-url-arm64",
				runner_url(version, "arm64"),
			),
			WriteFile::readable(
				"runner-amd64.sha256",
				format!("{}  {RUNNER_PATH}\n", daemon.runner_sha256_amd64),
			),
			WriteFile::readable(
				"runner-arm64.sha256",
				format!("{}  {RUNNER_PATH}\n", daemon.runner_sha256_arm64),
			),
		],
		runcmd: vec![vec!["bash".to_owned(), format!("{ETC}/boot.sh")]],
		power_state: PowerState {
			mode: "poweroff",
			delay: label.lifetime_minutes,
			condition: true,
		},
	};

	format!(
		"#cloud-config\n{}\n",
		serde_json::to_string_pretty(&config)
			.expect("cloud-config is plain data")
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::config::Provider;

	fn daemon() -> Daemon {
		Daemon {
			machine_prefix: crate::naming::DEFAULT_PREFIX.to_owned(),
			poll_interval_secs: 15,
			reconcile_grace_secs: 300,
			runner_version: "12.13.2".into(),
			runner_sha256_amd64: "deadbeef".into(),
			runner_sha256_arm64: "cafebabe".into(),
		}
	}

	fn label() -> Label {
		Label {
			labels: vec!["check".into()],
			provider: Provider::Hetzner,
			plans: vec!["cx43".into()],
			locations: vec!["fsn1".into()],
			image: "snapshot-1".into(),
			ssh_key: None,
			max_vms: 1,
			lifetime_minutes: 90,
			job_timeout_minutes: None,
			allow_fork_pull_request: true,
			allowed_events: vec!["pull_request".into()],
		}
	}

	fn registration() -> Registration {
		Registration {
			id: 7,
			uuid: "abcd-1234".into(),
			token: "s3cret".into(),
		}
	}

	fn rendered(
		registration: &Registration,
		handle: &str,
	) -> serde_json::Value {
		let text = render(
			&daemon(),
			&label(),
			"https://git.example.org",
			registration,
			handle,
		);
		let body = text.strip_prefix("#cloud-config\n").expect("header");
		serde_json::from_str(body).expect("valid cloud-config")
	}

	fn file<'a>(
		config: &'a serde_json::Value,
		name: &str,
	) -> &'a serde_json::Value {
		config["write_files"]
			.as_array()
			.unwrap()
			.iter()
			.find(|f| f["path"] == format!("{ETC}/{name}"))
			.unwrap_or_else(|| panic!("{name} was not written"))
	}

	#[test]
	fn declares_itself_as_cloud_config() {
		let text = render(
			&daemon(),
			&label(),
			"https://git.example.org",
			&registration(),
			"H1",
		);
		assert!(text.starts_with("#cloud-config\n"), "{text}");
	}

	#[test]
	fn ships_the_script_verbatim() {
		let config = rendered(&registration(), "H1");
		assert_eq!(file(&config, "boot.sh")["content"], BOOT);
		assert_eq!(file(&config, "boot.sh")["permissions"], "0700");
	}

	#[test]
	fn writes_every_label_in_the_set_one_per_line() {
		let mut multi = label();
		multi.labels = vec!["build".into(), "hetzner".into()];
		let text = render(
			&daemon(),
			&multi,
			"https://git.example.org",
			&registration(),
			"H1",
		);
		let config: serde_json::Value = serde_json::from_str(
			text.strip_prefix("#cloud-config\n").expect("header"),
		)
		.expect("valid cloud-config");

		assert_eq!(
			file(&config, "runner-labels")["content"],
			"build:host\nhetzner:host\n",
			"a machine registered with a subset of the set never matches the job"
		);
	}

	#[test]
	fn the_boot_script_recovers_every_label_that_was_written() {
		let mut multi = label();
		multi.labels = vec!["build".into(), "hetzner".into()];
		let text = render(
			&daemon(),
			&multi,
			"https://git.example.org",
			&registration(),
			"H1",
		);
		let config: serde_json::Value = serde_json::from_str(
			text.strip_prefix("#cloud-config\n").expect("header"),
		)
		.expect("valid cloud-config");
		let content = file(&config, "runner-labels")["content"]
			.as_str()
			.expect("string content");

		let dir = std::env::temp_dir().join("ci-orchestrator-boot-labels");
		std::fs::create_dir_all(&dir).expect("temp dir");
		std::fs::write(dir.join("runner-labels"), content).expect("write");

		let read_loop = BOOT
			.split_once("labels=()")
			.and_then(|(_, rest)| rest.split_once("done <"))
			.map(|(body, _)| body)
			.expect("the label loop");
		let script = format!(
			"set -euo pipefail\netc='{}'\nlabels=()\n{read_loop}done <\"$etc/runner-labels\"\nprintf '%s\\n' \"${{labels[@]}}\"",
			dir.display()
		);
		let output = std::process::Command::new("bash")
			.arg("-c")
			.arg(&script)
			.output()
			.expect("bash");

		assert_eq!(
			String::from_utf8_lossy(&output.stdout)
				.lines()
				.collect::<Vec<_>>(),
			vec!["--label", "build:host", "--label", "hetzner:host"],
			"stderr: {}",
			String::from_utf8_lossy(&output.stderr)
		);
	}

	#[test]
	fn the_script_installs_what_the_runner_needs_before_it_starts() {
		for tool in ["curl", "git"] {
			assert!(
				BOOT.contains(&format!("command -v {tool}")),
				"the runner clones actions before any step runs, so {tool} cannot come from a workflow step"
			);
		}
		let install = BOOT.find("apt-get install").expect("installs nothing");
		let download = BOOT.find("-o \"$runner\"").expect("no runner download");
		assert!(
			install < download,
			"prerequisites must precede the download"
		);
	}

	#[test]
	fn the_script_interpolates_nothing() {
		assert!(
			!BOOT.contains("{}"),
			"the script must carry no placeholders"
		);
		for value in ["s3cret", "abcd-1234", "git.example.org", "12.13.2"] {
			assert!(!BOOT.contains(value), "{value} is baked into the script");
		}
	}

	#[test]
	fn the_runner_is_given_a_timeout_it_cannot_be_talked_out_of() {
		let config = rendered(&registration(), "H1");
		assert_eq!(
			file(&config, "runner-config.yml")["content"],
			serde_json::json!(format!(
				"runner:\n  timeout: {}m\n",
				label().job_timeout()
			)),
			"a hung job must lose its runner before the machine is destroyed"
		);
		assert!(
			BOOT.contains("--config \"$etc/runner-config.yml\""),
			"the config is written but never passed to the runner"
		);
	}

	#[test]
	fn the_script_reads_from_the_directory_the_files_are_written_to() {
		assert!(
			BOOT.contains(&format!("etc={ETC}\n")),
			"cloud-init writes to {ETC}, so the script must read from it"
		);
	}

	#[test]
	fn every_value_the_script_reads_is_written() {
		let config = rendered(&registration(), "H1");
		for name in [
			"forgejo-url",
			"runner-uuid",
			"runner-labels",
			"job-handle",
			"runner-token",
			"runner-url-amd64",
			"runner-url-arm64",
			"runner-amd64.sha256",
			"runner-arm64.sha256",
			"runner-config.yml",
		] {
			let read = name
				.replace("-amd64", "-$arch")
				.replace("-arm64", "-$arch")
				.replace("amd64.sha256", "$arch.sha256")
				.replace("arm64.sha256", "$arch.sha256");
			assert!(
				BOOT.contains(&read),
				"{name} is written but never read as {read}"
			);
			file(&config, name);
		}
	}

	#[test]
	fn keeps_the_token_private_to_root() {
		let config = rendered(&registration(), "H1");
		let token = file(&config, "runner-token");
		assert_eq!(token["permissions"], "0600");
		assert_eq!(token["owner"], "root:root");
		assert_eq!(token["content"], "s3cret");
	}

	#[test]
	fn pins_the_runner_to_its_own_job() {
		let config = rendered(&registration(), "H1");
		assert_eq!(file(&config, "job-handle")["content"], "H1");
		assert!(BOOT.contains("--handle"), "{BOOT}");
		assert!(BOOT.contains("--wait"), "{BOOT}");
	}

	#[test]
	fn verifies_the_runner_download() {
		let config = rendered(&registration(), "H1");
		assert_eq!(
			file(&config, "runner-amd64.sha256")["content"],
			format!("deadbeef  {RUNNER_PATH}\n")
		);
		assert_eq!(
			file(&config, "runner-arm64.sha256")["content"],
			format!("cafebabe  {RUNNER_PATH}\n")
		);
		assert!(
			file(&config, "runner-url-arm64")["content"]
				.as_str()
				.is_some_and(|url| url.ends_with("-linux-arm64")),
			"the arm64 box must not be sent the amd64 build"
		);
		assert!(BOOT.contains("sha256sum -c"), "{BOOT}");
	}

	#[test]
	fn keeps_the_lifetime_backstop() {
		let config = rendered(&registration(), "H1");
		assert_eq!(config["power_state"]["mode"], "poweroff");
		assert_eq!(config["power_state"]["delay"], 90);
	}

	#[test]
	fn a_hostile_value_stays_data() {
		let mut registration = registration();
		registration.token = "tok'; rm -rf / #".into();
		let config = rendered(&registration, "H1");
		assert_eq!(
			file(&config, "runner-token")["content"],
			"tok'; rm -rf / #"
		);
		assert!(
			!config["runcmd"].to_string().contains("rm -rf"),
			"a value reached a command position"
		);
	}

	#[test]
	fn runs_only_the_script() {
		let config = rendered(&registration(), "H1");
		assert_eq!(
			config["runcmd"],
			serde_json::json!([["bash", format!("{ETC}/boot.sh")]])
		);
	}

	#[test]
	fn fits_inside_the_smallest_provider_limit() {
		let text = render(
			&daemon(),
			&label(),
			"https://git.example.org",
			&registration(),
			"H1",
		);
		assert!(text.len() < 32 * 1024, "{} bytes", text.len());
	}
}
