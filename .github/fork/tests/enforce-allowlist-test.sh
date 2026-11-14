#!/usr/bin/env bash
# Exercises enforce-allowlist.sh against a stub gh.
set -euo pipefail

script=$(cd "$(dirname "$0")/.." && pwd)/enforce-allowlist.sh
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

export FAKE=$work/fake GITHUB_REPOSITORY=fork/codex
mkdir -p "$work/bin"
# The stub fails a call when $FAKE/fail lists its "<method> <endpoint>".
cat >"$work/bin/gh" <<'GH'
#!/usr/bin/env bash
method=GET
[[ $2 == --method ]] && method=$3 && shift 2
endpoint=${2#repos/fork/codex/}
echo "$method $endpoint" >>"$FAKE/calls"
if grep -qxF "$method $endpoint" "$FAKE/fail"; then
  echo "gh: Server Error (HTTP 500)" >&2
  exit 1
fi
# The fixtures stand for every page of a listing.
if [[ $endpoint == actions/workflows || $endpoint == actions/runs\?* ]] && [[ " $* " != *" --paginate "* ]]; then
  echo "stub gh: $endpoint listed without --paginate" >&2
  exit 64
fi
case "$method $endpoint" in
"GET actions/workflows") cat "$FAKE/workflows" ;;
"GET actions/runs?status=queued&per_page=100") cat "$FAKE/queued" ;;
"GET actions/runs?status=in_progress&per_page=100") cat "$FAKE/in_progress" ;;
"GET contents/"*)
  if ! grep -qxF "${endpoint#contents/}" "$FAKE/files"; then
    echo "gh: Not Found (HTTP 404)" >&2
    exit 1
  fi
  ;;
"PUT actions/workflows/"*/disable | "POST actions/runs/"*/cancel) ;;
*) echo "stub gh: unexpected $method $endpoint" >&2 && exit 64 ;;
esac
GH
chmod +x "$work/bin/gh"
PATH=$work/bin:$PATH

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

# run <want status> <name> [failing call...]: resets the fixture and runs.
# $files lists the workflow files on the default branch.
run() {
  local want=$1 name=$2 status=0
  shift 2
  rm -rf "$FAKE" && mkdir "$FAKE"
  printf '%s\t%s\t%s\n' \
    1 active .github/workflows/sync-upstream.yml \
    2 active .github/workflows/fork-ci.yml \
    3 active .github/workflows/claude-code-review.yml \
    4 active .github/workflows/new-upstream.yml \
    5 disabled_manually .github/workflows/old-upstream.yml >"$FAKE/workflows"
  printf '%s\t%s\n' 40 4 20 2 >"$FAKE/queued"
  printf '%s\t%s\n' 50 5 10 1 >"$FAKE/in_progress"
  echo "${files-.github/workflows/new-upstream.yml}" >"$FAKE/files"
  printf '%s\n' "$@" >"$FAKE/fail"
  out=$("$script" 2>&1) || status=$?
  [[ $status -eq $want ]] || fail "$name: exit $status, wanted $want: $out"
  echo "ok: $name"
}

called() {
  grep -qxF "$1" "$FAKE/calls"
}

run 0 "disables and cancels only what is outside the allowlist"
[[ $(grep '^PUT ' "$FAKE/calls") == "PUT actions/workflows/4/disable" ]] ||
  fail "disabled the wrong workflows: $(grep '^PUT ' "$FAKE/calls")"
[[ $(grep '^POST ' "$FAKE/calls" | sort | paste -sd,) == "POST actions/runs/40/cancel,POST actions/runs/50/cancel" ]] ||
  fail "cancelled the wrong runs: $(grep '^POST ' "$FAKE/calls")"

run 1 "workflow listing fails" "GET actions/workflows"
run 1 "disable fails for a file on the default branch" "PUT actions/workflows/4/disable"
called "POST actions/runs/40/cancel" || fail "a failed disable skipped the cancellation"
run 1 "disable and the file lookup both fail" "PUT actions/workflows/4/disable" "GET contents/.github/workflows/new-upstream.yml"
run 1 "run listing fails" "GET actions/runs?status=in_progress&per_page=100"
run 1 "cancellation fails" "POST actions/runs/40/cancel"
called "POST actions/runs/50/cancel" || fail "a failed cancellation stopped the others"

files="" run 0 "tolerates a registration whose file is gone" "PUT actions/workflows/4/disable"

echo "all enforce-allowlist tests passed"
