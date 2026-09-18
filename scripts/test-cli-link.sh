#!/usr/bin/env bash
# Regression test for scripts/cli-link.sh. No installation, no BLE.
set -euo pipefail
readonly BIN_NAME="treadmill-bluetooth-macos"
script_dir="$(cd "$(dirname "$0")" && pwd)"
test_dir="$(mktemp -d)"
trap 'rm -rf "$test_dir"' EXIT

fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }

# Candidates are derived from $HOME at source time — point it at a sandbox.
export HOME="$test_dir/home"
mkdir -p "$HOME"
# shellcheck source=scripts/cli-link.sh
source "$script_dir/cli-link.sh"

# PATH guidance: special characters must remain literal in copy/pasted lines.
bin_dir="$test_dir/bin with spaces \$literal"
mkdir -p "$bin_dir"
ln -s /usr/bin/true "$bin_dir/tm"
output="$(print_path_help "$bin_dir" tm 2>&1)"
export_line="${output#*For this terminal (zsh/bash): }"
export_line="${export_line%%$'\n'*}"
(
  eval "$export_line"
  [[ "$(command -v tm)" == "$bin_dir/tm" ]] || fail 'suggested export does not expose tm'
  [[ -z "$(print_path_help "$bin_dir" tm 2>&1)" ]] || fail 'help printed although dir is on PATH'
)
[[ "$output" == *'.zshrc'* && "$output" == *'.bash_profile'* ]] || fail 'startup-file hint missing'

# Link dir: nothing on PATH → fallback ~/.local/bin.
[[ "$(PATH=/usr/bin resolve_link_dir tm "$BIN_NAME")" == "$HOME/.local/bin" ]] \
  || fail 'fallback is not ~/.local/bin'
# First candidate already on PATH wins.
[[ "$(PATH="/usr/bin:$HOME/.local/bin" resolve_link_dir tm "$BIN_NAME")" == "$HOME/.local/bin" ]] \
  || fail 'HOME/.local/bin on PATH not chosen'
[[ "$(PATH="$HOME/.local/bin:$HOME/.bin" resolve_link_dir tm "$BIN_NAME")" == "$HOME/.bin" ]] \
  || fail 'HOME/.bin must win when both are on PATH'
# An existing link of ours keeps its place on upgrade, even off PATH.
mkdir -p "$HOME/.bin"
ln -s "/opt/x/$BIN_NAME" "$HOME/.bin/tm"
[[ "$(PATH="/usr/bin:$HOME/.local/bin" resolve_link_dir tm "$BIN_NAME")" == "$HOME/.bin" ]] \
  || fail 'existing link location not preserved'
# Someone else's `tm` is not ours.
rm "$HOME/.bin/tm"
ln -s /usr/bin/true "$HOME/.bin/tm"
is_our_link "$HOME/.bin/tm" "$BIN_NAME" && fail 'foreign symlink treated as ours'

echo 'cli-link: PATH guidance and link-dir resolution pass'
