#!/usr/bin/env bash
#
# Cut a release in one command: bump the version, date the CHANGELOG, commit,
# tag, push, and watch the Release workflow (see docs/tasks/024).
#
# The GitHub Release is produced by .github/workflows/release.yml, which triggers
# ONLY on a pushed `v*` tag — pushing commits to main never releases. This script
# is the missing "cut the tag" step so it is not forgotten.
#
#   bash scripts/release.sh 0.2.0      # cuts v0.2.0
#
# It refuses to run on a dirty tree, off `main`, on an existing tag, or when the
# CHANGELOG's [Unreleased] section is empty (nothing to release). The CHANGELOG
# prose itself is written by hand under `## [Unreleased]` as work lands; this
# script only moves it under a dated version header.
set -euo pipefail

readonly CHANGELOG="CHANGELOG.md"
readonly MAIN_BRANCH="main"

die() {
  echo "release: $*" >&2
  exit 1
}

# --- arguments -------------------------------------------------------------
[[ $# -eq 1 ]] || die "usage: bash scripts/release.sh <version>  (e.g. 0.2.0)"
version="${1#v}" # tolerate a leading v
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must be X.Y.Z, got: $1"
readonly version
readonly tag="v${version}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# --- preconditions ---------------------------------------------------------
branch="$(git rev-parse --abbrev-ref HEAD)"
[[ "$branch" == "$MAIN_BRANCH" ]] || die "must release from '$MAIN_BRANCH', on '$branch'"

if ! git diff --quiet || ! git diff --cached --quiet; then
  die "working tree is dirty — commit or stash first (only version+changelog should change here)"
fi

git rev-parse -q --verify "refs/tags/${tag}" >/dev/null \
  && die "tag ${tag} already exists"

# The [Unreleased] section must have real content between its header and the
# next `## [` version header — otherwise there is nothing to release.
unreleased="$(awk '
  /^## \[Unreleased\]/ { grab = 1; next }
  grab && /^## \[/     { exit }
  grab                 { print }
' "$CHANGELOG" | tr -d '[:space:]')"
[[ -n "$unreleased" ]] \
  || die "CHANGELOG [Unreleased] is empty — document the changes there first"

echo "release: cutting ${tag} from ${branch}"

# --- bump version ----------------------------------------------------------
# First `version = "X.Y.Z"` in Cargo.toml is the [package] version (deps use the
# `name = "..."` form). Slurp (-0) so the s/// hits only that first occurrence.
perl -0pi -e "s/version = \"[0-9]+\.[0-9]+\.[0-9]+\"/version = \"${version}\"/" Cargo.toml
grep -q "^version = \"${version}\"" Cargo.toml || die "Cargo.toml version bump failed"

# Sync Cargo.lock (the crate's own version entry) via a quick incremental build,
# which doubles as a sanity compile of the release commit.
cargo build --quiet || die "cargo build failed after version bump — aborting"

# --- date the changelog ----------------------------------------------------
# Insert a dated version header right under [Unreleased]; the content that was
# under Unreleased now sits under it, and Unreleased is empty again for next time.
today="$(date +%F)"
perl -0pi -e "s/## \[Unreleased\]\n/## [Unreleased]\n\n## [${version}] — ${today}\n/" "$CHANGELOG"
grep -q "^## \[${version}\] — ${today}\$" "$CHANGELOG" || die "CHANGELOG date insertion failed"

# --- commit, tag, push -----------------------------------------------------
git add Cargo.toml Cargo.lock "$CHANGELOG"
git commit -m "chore(release): ${tag}" -- Cargo.toml Cargo.lock "$CHANGELOG"
git tag -a "$tag" -m "$tag"
git push origin "$branch"
git push origin "$tag"
echo "release: pushed ${tag}"

# --- watch the Release workflow --------------------------------------------
if command -v gh >/dev/null 2>&1; then
  echo "release: waiting for the Release workflow to pick up ${tag}…"
  # Match the run for THIS tag by its ref (headBranch), not just any push-event
  # release run — otherwise the previous release's run is grabbed before the new
  # one registers (found by dogfooding v0.2.0). The run takes a few seconds to
  # appear after the tag push, so poll.
  run_id=""
  for _ in {1..10}; do
    run_id="$(gh run list --workflow=release.yml -L 20 --json databaseId,headBranch \
      --jq "[.[] | select(.headBranch == \"${tag}\")][0].databaseId" 2>/dev/null || true)"
    [[ -n "$run_id" && "$run_id" != "null" ]] && break
    run_id=""
    sleep 6
  done
  if [[ -n "$run_id" ]]; then
    gh run watch "$run_id" --exit-status || die "Release workflow failed — see the run above"
    echo "release: workflow green"
    gh release view "$tag" --json url,assets \
      --jq '"release: \(.url)\nassets: \([.assets[].name] | join(", "))"' || true
  else
    echo "release: could not locate the Release run — check: gh run list --workflow=release.yml"
  fi
else
  echo "release: gh not found — watch manually: https://github.com/\$(gh repo view)/actions"
fi

echo "release: ${tag} done"
