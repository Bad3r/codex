#!/usr/bin/env bash
# Exercises cut-release.sh against throwaway local repositories and a stub gh.
set -euo pipefail

script=$(cd "$(dirname "$0")/.." && pwd)/cut-release.sh
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

export GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_GLOBAL=$work/gitconfig
git config --global user.name tester
git config --global user.email tester@example.invalid
git config --global init.defaultBranch main
git config --global advice.detachedHead false

export FAKE=$work/fake FORK_REPO=fork/codex UPSTREAM_REPO=up/codex
export FORK_URL=$work/fork.git UPSTREAM_URL=$work/upstream.git
unset GITHUB_OUTPUT GITHUB_STEP_SUMMARY PUSH_TOKEN
mkdir -p "$FAKE/releases" "$work/bin"
cat >"$work/bin/gh" <<'GH'
#!/usr/bin/env bash
missing() {
  echo "gh: Not Found (HTTP 404)" >&2
  exit 1
}
if [[ $1 == api && -e $FAKE/outage ]]; then
  echo "gh: Gateway Timeout (HTTP 504)" >&2
  exit 1
fi
case "$1 $2" in
"api repos/up/codex/releases/tags/"*)
  if grep -qx "${2##*/}" "$FAKE/draft"; then
    echo true
  elif grep -qx "${2##*/}" "$FAKE/released"; then
    echo false
  else
    missing
  fi
  ;;
"api repos/fork/codex/releases/tags/"*) test -e "$FAKE/releases/${2##*/}" || missing ;;
"release create") touch "$FAKE/releases/$3" ;;
*) echo "stub gh: unexpected $*" >&2 && exit 64 ;;
esac
GH
chmod +x "$work/bin/gh"
PATH=$work/bin:$PATH
runs=0

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

# upstream_release <version>: tags a version-bump commit off upstream main.
upstream_release() {
  git -C "$work/up" checkout --quiet --detach main
  sed -i "s/^version = .*/version = \"$1\"/" "$work/up/codex-rs/Cargo.toml"
  git -C "$work/up" commit --quiet -am "release $1"
  git -C "$work/up" tag -a "rust-v$1" -m "$1"
  git -C "$work/up" push --quiet "$UPSTREAM_URL" "rust-v$1"
  git -C "$work/up" checkout --quiet main
}

# set_line <repo> <file> <line> <text>
set_line() {
  sed -i "$3s/.*/$4/" "$1/codex-rs/src/$2"
}

# prepare <want decision> <want text> [--onto tag]: fresh prepare into $dir.
prepare() {
  local want=$1 text=$2
  shift 2
  dir=$work/run.$((++runs))
  local out status=0
  out=$("$script" prepare "$@" "$dir" 2>&1) || status=$?
  if [[ $want == fail ]]; then
    [[ $status -ne 0 ]] || fail "prepare succeeded, wanted failure: $out"
  else
    [[ $status -eq 0 ]] || fail "prepare failed: $out"
    grep -q "decision: $want " <<<"$out" || fail "wanted decision $want: $out"
  fi
  grep -qF -- "$text" <<<"$out" || fail "wanted text '$text': $out"
  echo "ok: $want: $text"
}

fork_ref() {
  git -C "$work/fork.git" rev-parse --quiet --verify "$1^{commit}" || true
}

mkdir -p "$work/up/codex-rs/src" "$work/up/.github/workflows"
git -C "$work/up" init --quiet
printf '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "0.0.0"\nedition = "2024"\n\n[workspace.dependencies]\nversion = "9"\n' \
  >"$work/up/codex-rs/Cargo.toml"
seq 1 30 >"$work/up/codex-rs/src/a.rs"
seq 1 30 >"$work/up/codex-rs/src/b.rs"
echo 'on: push' >"$work/up/.github/workflows/upstream.yml"
git -C "$work/up" add -A
git -C "$work/up" commit --quiet -m initial
git init --quiet --bare "$work/upstream.git"
git -C "$work/up" push --quiet "$UPSTREAM_URL" main
upstream_release 0.1.0
# Tags that are not codex versions, are drafts, or have no release.
git -C "$work/up" tag rusty-v8-v150.0.0
git -C "$work/up" tag rust-v9.0.0-alpha.1
git -C "$work/up" tag rust-v8.0.0
git -C "$work/up" push --quiet "$UPSTREAM_URL" rusty-v8-v150.0.0 rust-v9.0.0-alpha.1 rust-v8.0.0
git clone --quiet --bare --no-tags "$UPSTREAM_URL" "$work/fork.git"
printf 'rust-v0.1.0\nrusty-v8-v150.0.0\n' >"$FAKE/released"
echo rust-v9.0.0-alpha.1 >"$FAKE/draft"

prepare none "no patched branch"

dev=$work/dev
git clone --quiet "$FORK_URL" "$dev"
git -C "$dev" fetch --quiet --no-tags "$UPSTREAM_URL" 'refs/tags/rust-v*:refs/upstream/rust-v*'
git -C "$dev" checkout --quiet -b patched refs/upstream/rust-v0.1.0
mkdir -p "$dev/.github/fork"
echo 'true' >"$dev/.github/fork/check.sh"
git -C "$dev" add -A
git -C "$dev" commit --quiet -m "fork: tooling"
git -C "$dev" push --quiet origin patched
prepare none "changes no source file"

set_line "$dev" a.rs 15 patch-one
git -C "$dev" commit --quiet -am "patch one"
git -C "$dev" push --quiet origin patched
prepare release "patched-v0.1.0-r1 from rust-v0.1.0"
"$script" publish "$dir" >/dev/null
[[ $(fork_ref patched-v0.1.0-r1) == "$(fork_ref patched)" ]] || fail "r1 is not the patched tip"
[[ -e $FAKE/releases/patched-v0.1.0-r1 ]] || fail "r1 has no release"
prepare none "is released as patched-v0.1.0-r1"

# An API failure is not read as a missing release.
touch "$FAKE/outage"
prepare fail "looking up the release of rust-v9.0.0-alpha.1"
rm "$FAKE/outage"

echo 'true # changed' >"$dev/.github/fork/check.sh"
git -C "$dev" commit --quiet -am "fork: tooling change"
git -C "$dev" push --quiet origin patched
prepare none "no source change since patched-v0.1.0-r1"

set_line "$dev" b.rs 15 patch-two
git -C "$dev" commit --quiet -am "patch two"
git -C "$dev" push --quiet origin patched
prepare release "patched-v0.1.0-r2 from"
"$script" publish "$dir" >/dev/null

# A release that failed after its tag was pushed is created by a later run,
# also when a tooling commit landed in between.
rm "$FAKE/releases/patched-v0.1.0-r2"
prepare release-only "patched-v0.1.0-r2 is pushed but has no release"
echo 'true # again' >"$dev/.github/fork/check.sh"
git -C "$dev" commit --quiet -am "fork: tooling again"
git -C "$dev" push --quiet origin patched
prepare release-only "patched-v0.1.0-r2 is pushed but has no release"
"$script" publish "$dir" >/dev/null
[[ -e $FAKE/releases/patched-v0.1.0-r2 ]] || fail "r2 release was not recreated"
[[ $(git -C "$work/fork.git" log --format=%s -1 patched) == "fork: tooling again" ]] || fail "a release-only publish moved the branch"
prepare none "no source change since patched-v0.1.0-r2"

# A new upstream release that does not touch the patched lines.
set_line "$work/up" a.rs 2 upstream-two
git -C "$work/up" commit --quiet -am "upstream change"
git -C "$work/up" push --quiet "$UPSTREAM_URL" main
upstream_release 0.2.0
echo rust-v0.2.0 >>"$FAKE/released"
prepare release "patched-v0.2.0-r1 from rust-v0.2.0"

# The forced move is refused when the branch changed after prepare.
set_line "$dev" b.rs 20 patch-three
git -C "$dev" commit --quiet -am "patch three"
git -C "$dev" push --quiet origin patched
if "$script" publish "$dir" >/dev/null 2>&1; then
  fail "publish moved a branch that changed after prepare"
fi
[[ -z $(git -C "$work/fork.git" tag --list 'patched-v0.2.0-*' 'superseded-*') ]] || fail "a rejected publish left a tag"
[[ $(git -C "$work/fork.git" log --format=%s -1 patched) == "patch three" ]] || fail "a rejected publish moved the branch"

# The workflow checks one prepare and publishes a second one from a fresh
# runner, which must rebuild the tree that was checked.
old_tip=$(fork_ref patched)
prepare release "patched-v0.2.0-r1 from rust-v0.2.0"
gated_tree=$(sed -n 's/^tree=//p' "$dir/.git/cut-release.env")
[[ $gated_tree == "$(git -C "$dir" rev-parse 'HEAD^{tree}')" ]] || fail "prepare did not record the candidate tree"
prepare release "patched-v0.2.0-r1 from rust-v0.2.0"
if "$script" publish --gated-tree "$(git -C "$dir" rev-parse 'HEAD~1^{tree}')" "$dir" >/dev/null 2>&1; then
  fail "publish accepted a tree that was not checked"
fi
[[ -z $(fork_ref patched-v0.2.0-r1) && $(fork_ref patched) == "$old_tip" ]] || fail "a refused publish wrote to the fork"
"$script" publish --gated-tree "$gated_tree" "$dir" >/dev/null
new_tip=$(fork_ref patched)
[[ $new_tip == "$(fork_ref patched-v0.2.0-r1)" ]] || fail "patched was not moved to r1"
# The replaced tip had no release, so a tag has to keep it.
[[ -n $(fork_ref patched-v0.1.0-r2) && $(fork_ref "superseded-${old_tip:0:12}") == "$old_tip" ]] ||
  fail "the replaced tip is not reachable from a tag"
[[ $(git -C "$work/fork.git" show patched:codex-rs/src/a.rs | sed -n '2p;15p' | paste -sd,) == upstream-two,patch-one ]] ||
  fail "forward-port lost a change"
[[ $(git -C "$work/fork.git" log --format=%s -6 patched | paste -sd,) == "patch three,fork: tooling again,patch two,fork: tooling change,patch one,fork: tooling" ]] ||
  fail "forward-port changed the stack"
prepare none "is released as patched-v0.2.0-r1"

# Rehearsal onto a prerelease, and a release that already contains a patch.
git -C "$work/up" checkout --quiet --detach rust-v0.2.0
set_line "$work/up" a.rs 15 patch-one
git -C "$work/up" commit --quiet -am "upstream adopts patch one"
sed -i 's/^version = "0.2.0"/version = "0.2.1"/' "$work/up/codex-rs/Cargo.toml"
git -C "$work/up" commit --quiet -am "release 0.2.1"
git -C "$work/up" tag -a rust-v0.2.1 -m 0.2.1
git -C "$work/up" push --quiet "$UPSTREAM_URL" rust-v0.2.1
git -C "$work/up" checkout --quiet main
prepare fail "patch one\` is empty on rust-v0.2.1" --onto rust-v0.2.1
upstream_release 0.3.0-alpha.1
prepare rehearsal "stack applies to rust-v0.3.0-alpha.1" --onto rust-v0.3.0-alpha.1
if "$script" publish "$dir" >/dev/null 2>&1; then
  fail "a rehearsal was published"
fi

# A prerelease with a release is published like a stable release.
echo rust-v0.3.0-alpha.1 >>"$FAKE/released"
prepare release "patched-v0.3.0-alpha.1-r1 from rust-v0.3.0-alpha.1"
"$script" publish "$dir" >/dev/null
[[ $(fork_ref patched-v0.3.0-alpha.1-r1) == "$(fork_ref patched)" ]] || fail "alpha r1 is not the patched tip"
[[ -e $FAKE/releases/patched-v0.3.0-alpha.1-r1 ]] || fail "alpha r1 has no release"
prepare none "is released as patched-v0.3.0-alpha.1-r1"

# A release sorts after its prereleases; this one changes a patched line.
set_line "$work/up" a.rs 15 upstream-fifteen
git -C "$work/up" commit --quiet -am "upstream rewrites line 15"
upstream_release 0.3.0
echo rust-v0.3.0 >>"$FAKE/released"
prepare fail "patch one\` conflicts with rust-v0.3.0 in: codex-rs/src/a.rs"

# 0.3.0 is a draft and only 0.1.0 has a release.
echo rust-v0.3.0 >"$FAKE/draft"
echo rust-v0.1.0 >"$FAKE/released"
prepare fail "upstream version 0.1.0 is older than 0.3.0-alpha.1"

printf 'rust-v0.2.0\n' >"$FAKE/released"
git -C "$dev" fetch --quiet origin
git -C "$dev" reset --quiet --hard origin/patched
git -C "$dev" checkout --quiet -b side HEAD~1
set_line "$dev" a.rs 28 side
git -C "$dev" commit --quiet -am "side"
git -C "$dev" checkout --quiet patched
git -C "$dev" merge --quiet --no-ff -m "merge side" side
git -C "$dev" push --quiet origin patched
prepare fail "contains merge commits"

echo "all cut-release tests passed"
