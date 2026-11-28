#!/usr/bin/env bash
# Gate for the patch stack: formatting, a build of codex-cli, which depends on
# every patched crate, and a complete Cargo.lock. Runs on pull requests into
# `patched` and on a release candidate before it is published.
set -euo pipefail

if [[ -z ${GITHUB_ACTIONS:-} && -z ${IN_NIX_SHELL:-} ]] && command -v nix-shell >/dev/null; then
  exec nix-shell -p pkg-config openssl.dev libcap.dev --run "$(printf '%q ' "$0" "$@")"
fi

cd "$(dirname "$0")/../../codex-rs"

committed=$(mktemp)
cp Cargo.lock "$committed"
finish() {
  cp "$committed" Cargo.lock
  rm -f "$committed"
  if [[ -n ${GITHUB_ACTIONS:-} ]]; then
    echo "check.sh: ${SECONDS}s"
    df -h / .
  fi
}
trap finish EXIT

if [[ -n ${GITHUB_ACTIONS:-} ]]; then
  sudo apt-get update
  sudo DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends \
    build-essential pkg-config libcap-dev
  # Without arguments this installs the toolchain rust-toolchain.toml names.
  rustup toolchain install --no-self-update
  # The gate needs neither incremental state nor debug info, and both cost
  # build time and disk.
  export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
fi

# The command upstream's CI runs; rustfmt.toml carries the same setting.
cargo fmt -- --config imports_granularity=Item --check

# A release tag leaves the workspace crates at 0.0.0 in Cargo.lock, so
# --locked fails on every one. cargo rewrites those to the workspace version
# here; any other rewrite is a dependency change the stack did not commit, and
# packagers vendor from the committed lockfile.
cargo build -p codex-cli
cargo run -p codex-cli --bin codex -- --version
version=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml)
drift=$(diff "$committed" Cargo.lock | grep '^[<>]' |
  grep -vxF -e '< version = "0.0.0"' -e "> version = \"$version\"" || true)
if [[ -n $drift ]]; then
  echo "check.sh: Cargo.lock is missing these changes:" >&2
  echo "$drift" >&2
  exit 1
fi
