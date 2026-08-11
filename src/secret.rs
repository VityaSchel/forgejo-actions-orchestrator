use std::path::PathBuf;

use anyhow::{bail, Context, Result};

const DIRECTORY: &str = "CREDENTIALS_DIRECTORY";

pub fn load(variable: &str) -> Result<String> {
	match credentials_directory() {
		Some(directory) => {
			let path = directory.join(credential_file_name(variable));
			let value = std::fs::read_to_string(&path).with_context(|| {
				format!(
					"reading credential {}: is LoadCredential={} missing from the unit?",
					path.display(),
					credential_file_name(variable)
				)
			})?;
			non_empty(value.trim_end_matches('\n'), &path.display().to_string())
		}
		None => {
			let value = std::env::var(variable)
				.with_context(|| format!("{variable} is not set"))?;
			non_empty(&value, variable)
		}
	}
}

fn credentials_directory() -> Option<PathBuf> {
	std::env::var_os(DIRECTORY)
		.filter(|value| !value.is_empty())
		.map(PathBuf::from)
}

/// `LoadCredential=forgejo-runner-token:...` for `FORGEJO_RUNNER_TOKEN`
fn credential_file_name(variable: &str) -> String {
	variable.to_ascii_lowercase().replace('_', "-")
}

fn non_empty(value: &str, source: &str) -> Result<String> {
	if value.is_empty() {
		bail!("{source} is empty")
	}
	Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn derives_the_credential_file_name_from_the_variable() {
		assert_eq!(
			credential_file_name("FORGEJO_RUNNER_TOKEN"),
			"forgejo-runner-token"
		);
		assert_eq!(credential_file_name("HETZNER_TOKEN"), "hetzner-token");
	}

	#[test]
	fn rejects_an_empty_credential() {
		let error = non_empty("", "hetzner-token").unwrap_err().to_string();
		assert!(error.contains("hetzner-token is empty"), "{error}");
	}

	#[test]
	fn keeps_a_value_that_has_content() {
		assert_eq!(non_empty("abc", "x").unwrap(), "abc");
	}
}
