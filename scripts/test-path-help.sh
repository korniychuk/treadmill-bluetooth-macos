#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/path-help.sh"
test_dir="$(mktemp -d)"
trap 'rm -rf "$test_dir"' EXIT
# Special characters must remain literal in copy/pasted shell instructions.
bin_dir="$test_dir/bin with spaces \$literal"
mkdir -p "$bin_dir"
ln -s /usr/bin/true "$bin_dir/tm"
output="$(print_path_help "$bin_dir" tm 2>&1)"
export_line="${output#*For this terminal (zsh/bash): }"
export_line="${export_line%%$'\n'*}"
(
  eval "$export_line"
  [[ "$(command -v tm)" == "$bin_dir/tm" ]]
  [[ -z "$(print_path_help "$bin_dir" tm 2>&1)" ]]
)
[[ "$output" == *'.zshrc'* && "$output" == *'.bash_profile'* ]]
echo 'PATH guidance: shell escaping and already-on-PATH cases pass'
