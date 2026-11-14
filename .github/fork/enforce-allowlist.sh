#!/usr/bin/env bash
# Disables every workflow file except the fork's own and cancels the pending
# runs of the others. Upstream workflows need runners, caches, and credentials
# the fork lacks, and new ones arrive active with every upstream merge.
set -euo pipefail

repo=${GITHUB_REPOSITORY:-Bad3r/codex}
allow=(sync-upstream.yml fork-ci.yml claude-code-review.yml)

failed=0
error() {
  echo "::error::$*"
  failed=1
}

workflows=$(gh api "repos/$repo/actions/workflows" --paginate \
  --jq '.workflows[] | select(.path | startswith(".github/workflows/")) | "\(.id)\t\(.state)\t\(.path)"')

declare -A foreign=()
while IFS=$'\t' read -r id state path; do
  [[ -n $id ]] || continue
  for name in "${allow[@]}"; do
    [[ ${path##*/} == "$name" ]] && continue 2
  done
  foreign[$id]=$path
  [[ $state == active ]] || continue
  if gh api --method PUT "repos/$repo/actions/workflows/$id/disable" --silent; then
    echo "disabled $path"
  elif reply=$(gh api "repos/$repo/contents/$path" --silent 2>&1); then
    error "could not disable $path"
  elif [[ $reply == *'(HTTP 404)'* ]]; then
    # A registration whose file is gone from the default branch cannot be
    # disabled and cannot be triggered from it.
    echo "skipped $path: not on the default branch"
  else
    error "could not disable $path, and looking it up failed: $reply"
  fi
done <<<"$workflows"

# Disabling a workflow leaves the runs it already started.
for status in queued in_progress; do
  runs=$(gh api "repos/$repo/actions/runs?status=$status&per_page=100" --paginate \
    --jq '.workflow_runs[] | "\(.id)\t\(.workflow_id)"')
  while IFS=$'\t' read -r run workflow; do
    [[ -n $run && -n ${foreign[$workflow]:-} ]] || continue
    if gh api --method POST "repos/$repo/actions/runs/$run/cancel" --silent; then
      echo "cancelled run $run of ${foreign[$workflow]}"
    else
      error "could not cancel run $run of ${foreign[$workflow]}"
    fi
  done <<<"$runs"
done
exit "$failed"
