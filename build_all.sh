#!/usr/bin/env bash
# Build the Linux release archive: both binaries, stripped, into dist/.
#
# This is the same thing .github/workflows/release.yml does on a runner, kept
# runnable by hand so you can produce an artifact without pushing a tag. On
# Windows, build_all.ps1 calls this script inside WSL.
#
#   ./build_all.sh                 # build and package into dist/
#   ./build_all.sh --bootstrap     # install the toolchain and headers first
#   ./build_all.sh --cli-only      # skip the GUI (and all of its X11/GL deps)
#
# A binary built here needs a glibc at least as new as the one on this machine.
# Build on the oldest distro you intend to support, or let the CI workflow do
# it: that one runs on Ubuntu 22.04, so its output covers Debian 12 and up.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="$root/dist"
target_dir="${CARGO_TARGET_DIR:-}"
bootstrap=0
cli_only=0

while [ $# -gt 0 ]; do
	case "$1" in
		--out) out="$2"; shift 2 ;;
		--target-dir) target_dir="$2"; shift 2 ;;
		--bootstrap) bootstrap=1; shift ;;
		--cli-only) cli_only=1; shift ;;
		-h|--help) sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \?//'; exit 0 ;;
		*) echo "build_all.sh: unknown option '$1'" >&2; exit 2 ;;
	esac
done

say() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }

if [ "$bootstrap" = 1 ]; then
	say "Installing build dependencies (sudo)"
	if ! command -v apt-get >/dev/null 2>&1; then
		echo "--bootstrap only knows apt. See the Linux section of README.md" >&2
		echo "for the Fedora package list, then re-run without --bootstrap." >&2
		exit 1
	fi
	sudo apt-get update
	pkgs="build-essential cmake pkg-config"
	if [ "$cli_only" = 0 ]; then
		pkgs="$pkgs libx11-dev libxcursor-dev libxrandr-dev libxi-dev"
		pkgs="$pkgs libxkbcommon-dev libwayland-dev libgl1-mesa-dev"
	fi
	# shellcheck disable=SC2086
	sudo apt-get install -y --no-install-recommends $pkgs

	if ! command -v cargo >/dev/null 2>&1; then
		say "Installing Rust"
		curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
			| sh -s -- -y --profile minimal
		# shellcheck disable=SC1091
		. "$HOME/.cargo/env"
	fi
fi

if ! command -v cargo >/dev/null 2>&1; then
	# Non-login shells miss this, which is the usual reason a WSL build fails.
	[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env" || true
fi
command -v cargo >/dev/null 2>&1 || {
	echo "cargo not found. Run: ./build_all.sh --bootstrap" >&2
	exit 1
}

version="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
[ -n "$version" ] || { echo "could not read version from Cargo.toml" >&2; exit 1; }
name="modifile-v${version}-x86_64-linux"

export CARGO_TARGET_DIR="${target_dir:-$root/target}"

say "Building modifile v$version (target dir: $CARGO_TARGET_DIR)"
if [ "$cli_only" = 1 ]; then
	cargo build --release --locked -p modifile-cli
else
	cargo build --release --locked
fi

say "Packaging $name.tar.gz"
rm -rf "${out:?}/$name"
mkdir -p "$out/$name"
cp "$CARGO_TARGET_DIR/release/modifile" "$out/$name/"
[ "$cli_only" = 1 ] || cp "$CARGO_TARGET_DIR/release/modifile-gui" "$out/$name/"
cp "$root/README.md" "$out/$name/"
[ -f "$root/LICENSE" ] && cp "$root/LICENSE" "$out/$name/" || true

tar -czf "$out/$name.tar.gz" -C "$out" "$name"
rm -rf "${out:?}/$name"

( cd "$out" && sha256sum "$name.tar.gz" )
say "Done: $out/$name.tar.gz"
