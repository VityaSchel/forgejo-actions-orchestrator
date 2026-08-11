use std::path::PathBuf;

use anyhow::{bail, Result};

pub const USAGE: &str =
	concat!("usage: ", env!("CARGO_PKG_NAME"), " --config <path>");

const LONG: &str = "--config";
const SHORT: &str = "-c";

pub enum Invocation {
	Run(PathBuf),
	Usage,
}

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Invocation> {
	let mut args = args.into_iter();
	let mut path: Option<PathBuf> = None;

	while let Some(arg) = args.next() {
		match arg.as_str() {
			"-h" | "--help" => return Ok(Invocation::Usage),
			LONG | SHORT => match args.next() {
				Some(value) if !value.is_empty() => path = Some(value.into()),
				_ => bail!("{arg} needs a path\n{USAGE}"),
			},
			_ => match arg.strip_prefix("--config=") {
				Some("") => bail!("--config needs a path\n{USAGE}"),
				Some(value) => path = Some(value.into()),
				None => bail!("unexpected argument {arg}\n{USAGE}"),
			},
		}
	}

	match path {
		Some(path) => Ok(Invocation::Run(path)),
		None => bail!("--config is required\n{USAGE}"),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parsed(args: &[&str]) -> Result<Invocation> {
		parse(args.iter().map(|a| a.to_string()))
	}

	fn path(args: &[&str]) -> PathBuf {
		match parsed(args).unwrap() {
			Invocation::Run(path) => path,
			Invocation::Usage => panic!("expected a config path"),
		}
	}

	fn error(args: &[&str]) -> String {
		parsed(args).err().expect("should have failed").to_string()
	}

	#[test]
	fn accepts_the_long_flag() {
		assert_eq!(
			path(&["--config", "/etc/orc.toml"]),
			PathBuf::from("/etc/orc.toml")
		);
	}

	#[test]
	fn accepts_the_short_flag() {
		assert_eq!(
			path(&["-c", "/etc/orc.toml"]),
			PathBuf::from("/etc/orc.toml")
		);
	}

	#[test]
	fn accepts_the_joined_form() {
		assert_eq!(
			path(&["--config=/etc/orc.toml"]),
			PathBuf::from("/etc/orc.toml")
		);
	}

	#[test]
	fn requires_the_flag() {
		assert!(error(&[]).contains("--config is required"));
	}

	#[test]
	fn rejects_a_flag_with_no_value() {
		assert!(error(&["--config"]).contains("needs a path"));
		assert!(error(&["-c"]).contains("needs a path"));
		assert!(error(&["--config="]).contains("needs a path"));
	}

	#[test]
	fn rejects_a_positional_argument() {
		let error = error(&["/etc/orc.toml"]);
		assert!(error.contains("unexpected argument"), "{error}");
		assert!(error.contains(USAGE), "{error}");
	}

	#[test]
	fn rejects_an_unknown_flag() {
		assert!(error(&["--verbose"]).contains("unexpected argument"));
	}

	#[test]
	fn asks_for_usage() {
		assert!(matches!(parsed(&["--help"]).unwrap(), Invocation::Usage));
		assert!(matches!(parsed(&["-h"]).unwrap(), Invocation::Usage));
	}

	#[test]
	fn the_last_flag_wins() {
		assert_eq!(
			path(&["-c", "a.toml", "--config", "b.toml"]),
			PathBuf::from("b.toml")
		);
	}
}
