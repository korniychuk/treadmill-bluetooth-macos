# Actionable PATH guidance

The installer creates `~/.bin/tm`, but a default macOS terminal does not search
that directory. A successful installation therefore appears broken when the
operator types `tm status`.

Both installers now print a quoted full-path invocation, a copyable PATH export
for the current shell, and the zsh/bash startup-file location for persistence.
No shell files are modified automatically. Custom LINK_DIR and LINK_NAME values
are respected. The release archive includes the shared helper.

Validation: `bash scripts/test-path-help.sh` verifies shell escaping with spaces
and a literal dollar sign, command discovery after the suggested export, and
silence when the directory is already on PATH. No installation or BLE required.
