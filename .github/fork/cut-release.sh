#!/usr/bin/env bash
# Rebuilds the patch stack of the `patched` branch on the newest upstream
# release, prereleases included, and publishes it as
# patched-v<version>-r<N>.
#
#   cut-release.sh prepare [--onto <upstream tag>] <dir>
#   cut-release.sh publish [--gated-tree <tree>] <dir>
#
# prepare writes to no remote. It clones into <dir>, leaves the candidate
# checked out there, and records its decision in <dir>/.git/cut-release.env.
# --onto rehearses the forward-port onto any upstream tag; the result cannot
# be published.
#
# publish moves `patched` to the candidate, pushes the tag in the same atomic
# push, and creates the GitHub release. It does not run the candidate's
# check.sh: the caller does, and --gated-tree refuses a candidate whose tree
# is not the one that was checked. PUSH_TOKEN, when set, authenticates the
# push; otherwise the ambient git credentials do.
set -euo pipefail

FORK_REPO=${FORK_REPO:-${GITHUB_REPOSITORY:-Bad3r/codex}}
UPSTREAM_REPO=${UPSTREAM_REPO:-openai/codex}
FORK_URL=${FORK_URL:-https://github.com/$FORK_REPO.git}
UPSTREAM_URL=${UPSTREAM_URL:-https://github.com/$UPSTREAM_REPO.git}
BRANCH=patched
RELEASE_VERSION='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$'
# Paths that decide whether two trees are the same release.
SOURCE=(. ':(exclude).github')

note() {
  echo "cut-release: $*"
  if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
    echo "- $*" >>"$GITHUB_STEP_SUMMARY"
  fi
}

die() {
  note "failed: $*" >&2
  exit 1
}

# Unsigned, with a committer that exists on a runner without git identity.
git_commit_env() {
  if ! git config user.name >/dev/null; then
    export GIT_COMMITTER_NAME='github-actions[bot]'
    export GIT_COMMITTER_EMAIL='41898282+github-actions[bot]@users.noreply.github.com'
  fi
}

# Sets `reply` to a jq expression over the published release of a tag. Fails
# when the tag has none; any other API error ends the run.
release_query() {
  if reply=$(gh api "repos/$1/releases/tags/$2" --jq "$3" 2>&1); then
    return 0
  elif [[ $reply == *'(HTTP 404)'* ]]; then
    return 1
  fi
  die "looking up the release of $2 in $1: $reply"
}

release_exists() {
  release_query "$FORK_REPO" "$1" .id
}

# Sorts versions in semver order: a prerelease sorts before its release.
sort_versions() {
  sed 's/-/~/' | sort -V "$@" | sed 's/~/-/'
}

# Sets `new` to the highest upstream version, prereleases included, with a
# published release. The versions come from the tags: the releases listing is
# paginated and times out on upstream's pages.
latest_upstream_release() {
  local tags versions version
  tags=$(git ls-remote --tags "$UPSTREAM_URL" 'refs/tags/rust-v*')
  versions=$(awk '{ sub("^refs/tags/rust-v", "", $2); print $2 }' <<<"$tags" |
    grep -E "$RELEASE_VERSION" | sort_versions -r) || die "no release tag in $UPSTREAM_REPO"
  for version in $versions; do
    if release_query "$UPSTREAM_REPO" "rust-v$version" .draft && [[ $reply == false ]]; then
      new=$version
      return
    fi
  done
  die "no published release in $UPSTREAM_REPO"
}

workspace_version() {
  git show "$1:codex-rs/Cargo.toml" | awk '
    /^\[workspace\.package\]/ { inside = 1; next }
    /^\[/ { inside = 0 }
    inside && /^version[ \t]*=/ { gsub(/[" \t]/, ""); sub(/^version=/, ""); print; exit }
  '
}

# Prints "<N> <sha>" for the highest patched-v<version>-r<N> in ls-remote
# output on stdin, or nothing.
highest_revision() {
  awk -v prefix="refs/tags/patched-v$1-r" '
    index($2, prefix) == 1 {
      n = substr($2, length(prefix) + 1)
      if (n ~ /^[0-9]+$/ && n + 0 > best) { best = n + 0; sha = $1 }
    }
    END { if (best) print best, sha }
  '
}

decide() {
  local decision=$1 reason=$2 tree=""
  note "decision: $decision ($reason)"
  if [[ -n ${candidate:-} ]]; then
    tree=$(git rev-parse "$candidate^{tree}")
  fi
  if [[ -n ${state_file:-} ]]; then
    {
      echo "decision=$decision"
      echo "tag=${tag:-}"
      echo "candidate=${candidate:-}"
      echo "tree=$tree"
      echo "lease=${tip:-}"
      echo "version=${new:-}"
    } >"$state_file"
  fi
  if [[ -n ${GITHUB_OUTPUT:-} ]]; then
    {
      echo "decision=$decision"
      echo "tag=${tag:-}"
      echo "tree=$tree"
    } >>"$GITHUB_OUTPUT"
  fi
}

cmd_prepare() {
  local onto=""
  if [[ ${1:-} == --onto ]]; then
    onto=${2:?--onto needs an upstream tag}
    shift 2
  fi
  local dir=${1:?usage: cut-release.sh prepare [--onto <upstream tag>] <dir>}
  [[ ! -e $dir ]] || die "$dir already exists"

  local refs tip new last last_n=0 last_sha=""
  refs=$(git ls-remote "$FORK_URL" "refs/heads/$BRANCH" 'refs/tags/patched-v*')
  tip=$(awk -v ref="refs/heads/$BRANCH" '$2 == ref { print $1 }' <<<"$refs")
  if [[ -z $tip ]]; then
    decide none "no $BRANCH branch"
    return
  fi

  if [[ -n $onto ]]; then
    new=${onto#rust-v}
  else
    latest_upstream_release
  fi
  last=$(highest_revision "$new" <<<"$refs")
  if [[ -n $last ]]; then
    read -r last_n last_sha <<<"$last"
  fi
  if [[ $last_sha == "$tip" ]] && release_exists "patched-v$new-r$last_n"; then
    decide none "$BRANCH is released as patched-v$new-r$last_n"
    return
  fi

  git init --quiet "$dir"
  cd "$dir"
  state_file=$PWD/.git/cut-release.env
  git remote add origin "$FORK_URL"
  git remote add upstream "$UPSTREAM_URL"
  git fetch --quiet --no-tags origin \
    "refs/heads/$BRANCH:refs/remotes/origin/$BRANCH" 'refs/tags/patched-v*:refs/tags/patched-v*'

  local cur base onto_commit
  cur=$(workspace_version "origin/$BRANCH")
  [[ -n $cur ]] || die "no workspace version in codex-rs/Cargo.toml on $BRANCH"
  git fetch --quiet --no-tags upstream \
    "refs/tags/rust-v$cur:refs/upstream/rust-v$cur" "refs/tags/rust-v$new:refs/upstream/rust-v$new"
  base=$(git rev-parse "refs/upstream/rust-v$cur^{commit}")
  onto_commit=$(git rev-parse "refs/upstream/rust-v$new^{commit}")
  git merge-base --is-ancestor "$base" "origin/$BRANCH" ||
    die "$BRANCH is not based on rust-v$cur"
  [[ -z $(git rev-list --merges "$base..origin/$BRANCH") ]] ||
    die "the stack on $BRANCH contains merge commits; rebase-merge pull requests into $BRANCH"

  if [[ $new == "$cur" ]]; then
    git checkout --quiet --detach "origin/$BRANCH"
  elif [[ $(printf '%s\n' "$cur" "$new" | sort_versions | tail -n 1) != "$new" ]]; then
    die "upstream version $new is older than $cur on $BRANCH"
  else
    git checkout --quiet --detach "$onto_commit"
    git_commit_env
    local commit subject unmerged log
    log=$(mktemp)
    for commit in $(git rev-list --reverse "$base..origin/$BRANCH"); do
      subject=$(git log -1 --format='%h %s' "$commit")
      if git -c commit.gpgsign=false cherry-pick "$commit" >"$log" 2>&1; then
        continue
      fi
      unmerged=$(git diff --name-only --diff-filter=U | paste -sd ' ')
      if [[ -n $unmerged ]]; then
        die "\`$subject\` conflicts with rust-v$new in: $unmerged"
      fi
      if git rev-parse --quiet --verify CHERRY_PICK_HEAD >/dev/null && git diff --cached --quiet HEAD; then
        die "\`$subject\` is empty on rust-v$new: upstream contains it, drop it from the stack"
      fi
      cat "$log" >&2
      die "cherry-pick of \`$subject\` onto rust-v$new failed"
    done
    rm -f "$log"
  fi
  candidate=$(git rev-parse HEAD)

  if git diff --quiet "$onto_commit" "$candidate" -- "${SOURCE[@]}"; then
    decide none "the stack changes no source file"
  elif [[ -n $onto ]]; then
    decide rehearsal "stack applies to rust-v$new as $candidate"
  elif [[ -n $last_sha ]] && git diff --quiet "$last_sha" "$candidate" -- "${SOURCE[@]}"; then
    if release_exists "patched-v$new-r$last_n"; then
      decide none "no source change since patched-v$new-r$last_n"
    else
      tag=patched-v$new-r$last_n
      candidate=$last_sha
      decide release-only "$tag is pushed but has no release"
    fi
  else
    tag=patched-v$new-r$((last_n + 1))
    decide release "$tag from rust-v$new at $candidate"
  fi
}

cmd_publish() {
  local gated=0 gated_tree=""
  if [[ ${1:-} == --gated-tree ]]; then
    gated=1
    gated_tree=${2:?--gated-tree needs a tree}
    shift 2
  fi
  local dir=${1:?usage: cut-release.sh publish [--gated-tree <tree>] <dir>}
  cd "$dir"
  local decision="" tag="" candidate="" tree="" lease="" version=""
  # shellcheck source=/dev/null
  source .git/cut-release.env
  [[ $decision == release || $decision == release-only ]] ||
    die "nothing to publish (decision: $decision)"
  [[ $version =~ $RELEASE_VERSION ]] || die "$version is not a release version"
  if ((gated)) && [[ $tree != "$gated_tree" ]]; then
    die "the candidate tree $tree is not the gated tree $gated_tree"
  fi

  if [[ $decision == release ]]; then
    git tag "$tag" "$candidate"
    local auth=()
    if [[ -n ${PUSH_TOKEN:-} ]]; then
      # The helper reads the token from the environment, keeping it out of argv.
      # shellcheck disable=SC2016
      auth=(-c credential.helper= -c 'credential.helper=!f() { test "$1" = get && printf "username=x-access-token\npassword=%s\n" "$PUSH_TOKEN"; }; f')
    fi
    local refs=("$candidate:refs/heads/$BRANCH" "refs/tags/$tag")
    # A forward-port takes the old tip off the branch. A tag keeps the tip
    # when no release tag reaches it.
    if ! git merge-base --is-ancestor "$lease" "$candidate" &&
      [[ -z $(git tag --list 'patched-v*' --contains "$lease") ]]; then
      refs+=("$lease:refs/tags/superseded-${lease:0:12}")
    fi
    git "${auth[@]}" push --atomic --force-with-lease="refs/heads/$BRANCH:$lease" origin "${refs[@]}"
    note "pushed ${refs[*]#*:} and moved $BRANCH from $lease to $candidate"
  fi

  local notes
  notes=$(mktemp)
  {
    echo "Upstream release: https://github.com/$UPSTREAM_REPO/releases/tag/rust-v$version"
    echo
    echo "Patch stack:"
    echo
    git log --reverse --format='- %h %s' "refs/upstream/rust-v$version^{commit}..$candidate"
  } >"$notes"
  gh release create "$tag" --repo "$FORK_REPO" --verify-tag --latest \
    --title "$version patched r${tag##*-r}" --notes-file "$notes"
  rm -f "$notes"
  note "released $tag"
}

case ${1:-} in
prepare | publish)
  cmd=$1
  shift
  "cmd_$cmd" "$@"
  ;;
*)
  sed -n '2,17s/^# \{0,1\}//p' "$0" >&2
  exit 2
  ;;
esac
