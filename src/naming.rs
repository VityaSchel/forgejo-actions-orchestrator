pub const PREFIX: &str = "ci-orc";

const HANDLE_LEN: usize = 24;

pub fn machine_name(label: &str, handle: &str) -> String {
	format!("{PREFIX}-{label}-{}", truncated_handle(handle))
}

pub fn truncated_handle(handle: &str) -> String {
	handle
		.chars()
		.filter_map(|c| match c {
			'a'..='z' | '0'..='9' | '-' => Some(c),
			'A'..='Z' => Some(c.to_ascii_lowercase()),
			_ => None,
		})
		.take(HANDLE_LEN)
		.collect::<String>()
		.trim_end_matches('-')
		.to_owned()
}

pub fn split<'a, S: AsRef<str>>(
	name: &'a str,
	labels: &[S],
) -> Option<(&'a str, &'a str)> {
	let mut longest_first: Vec<&str> =
		labels.iter().map(AsRef::as_ref).collect();
	longest_first.sort_by_key(|label| std::cmp::Reverse(label.len()));
	longest_first.iter().find_map(|label| {
		let head = format!("{PREFIX}-{label}-");
		name.strip_prefix(&head).map(|handle| {
			let start = PREFIX.len() + 1;
			(&name[start..start + label.len()], handle)
		})
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn never_ends_a_name_with_a_hyphen() {
		let uuid = "9baa37c2-8d04-4025-86d7-fa1b2c3d4e5f";
		assert_eq!(truncated_handle(uuid), "9baa37c2-8d04-4025-86d7");
		let name = machine_name("hetzner-verify", uuid);
		assert!(
			!name.ends_with('-'),
			"RFC 1123 forbids a trailing hyphen: {name}"
		);
		assert_eq!(
			split(&name, &["hetzner-verify"]),
			Some(("hetzner-verify", "9baa37c2-8d04-4025-86d7"))
		);
	}

	#[test]
	fn round_trips_a_simple_label() {
		let name = machine_name("check", "H1");
		assert_eq!(split(&name, &["check"]), Some(("check", "h1")));
	}

	#[test]
	fn round_trips_a_label_containing_hyphens() {
		let name = machine_name("release-builder-1-cherry", "abc");
		assert_eq!(
			split(&name, &["release-builder-1-cherry"]),
			Some(("release-builder-1-cherry", "abc"))
		);
	}

	#[test]
	fn prefers_the_longest_matching_label() {
		let name = machine_name("check-big", "H");
		assert_eq!(
			split(&name, &["check", "check-big"]),
			Some(("check-big", "h"))
		);
	}

	#[test]
	fn ignores_names_that_are_not_ours() {
		assert_eq!(split("someone-elses-server", &["check"]), None);
		assert_eq!(split("ci-orc-other-H", &["check"]), None);
	}

	#[test]
	fn strips_characters_a_hostname_cannot_carry() {
		assert_eq!(truncated_handle("A_b/c:d"), "abcd");
	}

	#[test]
	fn truncates_a_long_handle_deterministically() {
		let handle = "a".repeat(100);
		assert_eq!(truncated_handle(&handle).len(), HANDLE_LEN);
		assert_eq!(truncated_handle(&handle), truncated_handle(&handle));
	}
}
