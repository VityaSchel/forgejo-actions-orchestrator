#!/bin/bash
set -euo pipefail

readonly target=x86_64-unknown-linux-musl
readonly zig_required=0.16.0
readonly name=forgejo-actions-orchestrator

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

for tool in zig cargo-zigbuild; do
	if ! command -v "$tool" >/dev/null 2>&1; then
		echo "$tool not found; see the README" >&2
		exit 1
	fi
done

zig_found=$(zig version)
if [ "$zig_found" != "$zig_required" ]; then
	echo "zig $zig_required is pinned, found $zig_found" >&2
	echo "another zig links another musl and moves the hash" >&2
	exit 1
fi

sysroot=$(rustc --print sysroot)
cargo_home=${CARGO_HOME:-$HOME/.cargo}

# cargo splits RUSTFLAGS on spaces
for path in "$sysroot" "$cargo_home" "$root"; do
	case $path in
	*" "*)
		echo "cannot remap '$path': a space in the path breaks RUSTFLAGS" >&2
		exit 1
		;;
	esac
done

rustflags="--remap-path-prefix=$sysroot=/rust"
rustflags="$rustflags --remap-path-prefix=$cargo_home=/cargo"
rustflags="$rustflags --remap-path-prefix=$root=/build"

RUSTFLAGS="$rustflags" cargo zigbuild --target "$target" --release --locked

readonly out=target/$target/release/$name

if LC_ALL=C grep -aq -e "$sysroot" -e "$cargo_home" -e "$root" "$out"; then
	echo "a build path survived into $out; the build is not reproducible" >&2
	exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
	sha256sum "$out"
else
	shasum -a 256 "$out"
fi
