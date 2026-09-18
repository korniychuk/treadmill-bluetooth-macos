# 067 — Actionable PATH guidance

Status: **done** (external PR #2 by @huertin03, finished in review).

The installer created `~/.bin/tm`, but a default macOS terminal does not search
that directory. A successful installation therefore appeared broken when the
operator typed `tm status`.

- Both installers print a quoted full-path invocation, a copyable PATH export
  for the current shell, and the zsh/bash startup-file location. No shell files
  are modified automatically.
- With `LINK_DIR` unset, the alias directory is resolved by
  `scripts/cli-link.sh::resolve_link_dir`: a candidate that already holds our
  symlink (upgrades stay in place) → the first of `~/.bin`, `~/.local/bin`
  already on PATH → fallback `~/.local/bin` (the de-facto per-user bin dir, the
  most likely to be on PATH already). `uninstall-daemon.sh` removes our symlink
  from every candidate. Explicit `LINK_DIR` / `LINK_NAME` are respected.
- The release archive includes the shared helper.

Validation: `bash scripts/test-cli-link.sh` (also in CI) — shell escaping with
spaces and a literal dollar sign, command discovery after the suggested export,
silence when already on PATH, link-dir resolution order, foreign-symlink guard.
No installation or BLE required.
