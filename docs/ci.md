# CI / Release — agent cheatsheet

Terse map for the AI agent. Two GitHub Actions workflows, both on `macos-latest`.

## Files

- `.github/workflows/ci.yml` — gate on every `push: [main]` and every `pull_request`.
- `.github/workflows/release.yml` — build + publish on `push: tags v*`.
- `scripts/release.sh <ver>` — the **only** sanctioned way to cut a release: bumps
  version, dates the CHANGELOG, commits, tags `v<ver>`, pushes → triggers `release.yml`.

## CI (`ci.yml`) — single `test` job, steps run in order, **fail-fast**

1. Checkout → `dtolnay/rust-toolchain@stable` (with `rustfmt`, `clippy`) → `Swatinem/rust-cache@v2`.
2. `cargo fmt --all --check`   ← **most common breakage**; when it fails, steps 3-5 are `skipped`.
3. `cargo clippy --all-targets -- -D warnings`   ← warnings are errors.
4. `cargo build --verbose`
5. `cargo test --verbose`

`concurrency` has `cancel-in-progress: true` — a newer push to the same ref cancels the
older run (shows up as very short `failure`/cancelled runs; don't confuse with real failures).

## Pre-push mirror — run locally before `git push`, same order CI does

```bash
cargo fmt --all --check && \
cargo clippy --all-targets -- -D warnings && \
cargo build && cargo test
```

If fmt is dirty: `cargo fmt --all` (no `--check`) fixes it. Doing this locally saves a
full CI round-trip, since fmt failing skips everything after it.

## Release (`release.yml`) — on tag `v*`

`cargo build --release` → package a **version-less** tarball
(`treadmill-bluetooth-macos-macos-<arch>.tar.gz`, stable name so the README
`releases/latest/download/...` one-liner resolves; version lives in the tag/notes) with the
binary + `scripts/{install,uninstall,register-notification-identity}` + icon + README + LICENSE →
`softprops/action-gh-release` with `generate_release_notes`. Binary is **unsigned / ad-hoc**
(no Apple notarization) → users must `xattr -d com.apple.quarantine` or build locally.

## Watching a run (ship flow)

```bash
gh run list --commit "$(git rev-parse HEAD)" --json databaseId,status,conclusion,url
gh run watch <id> --exit-status          # under `timeout`, ~10m budget
gh run view <id> --json jobs --jq '.jobs[].steps[] | "\(.conclusion) | \(.name)"'  # which step failed
gh run view <id> --log-failed            # failing step log
```
