#!/usr/bin/env bash
# Shared helpers for the `tm` CLI alias, sourced by the install/uninstall
# scripts. Advisory only: never edits a user's shell files.

# Conventional per-user bin dirs, in preference order when choosing a default.
# ~/.local/bin is the de-facto standard (pipx, uv, many installers) and is the
# most likely to already be on PATH on a fresh Mac.
TM_LINK_DIR_CANDIDATES=("$HOME/.bin" "$HOME/.local/bin")
readonly TM_LINK_DIR_FALLBACK="$HOME/.local/bin"

# Print the directory for the alias when LINK_DIR is not set explicitly:
# a candidate that already holds our symlink (keeps upgrades in place), else
# the first candidate already on PATH, else the fallback.
resolve_link_dir() {
  local link_name="$1" bin_name="$2" dir
  for dir in "${TM_LINK_DIR_CANDIDATES[@]}"; do
    if is_our_link "$dir/$link_name" "$bin_name"; then
      printf '%s\n' "$dir"
      return 0
    fi
  done
  for dir in "${TM_LINK_DIR_CANDIDATES[@]}"; do
    case ":$PATH:" in
      *":$dir:"*) printf '%s\n' "$dir"; return 0 ;;
    esac
  done
  printf '%s\n' "$TM_LINK_DIR_FALLBACK"
}

# True when the path is a symlink to our binary, not someone else's file.
is_our_link() {
  local link="$1" bin_name="$2"
  [[ -L "$link" && "$(readlink "$link")" == *"/${bin_name}" ]]
}

print_path_help() {
  local directory="$1" command_name="$2" quoted_directory quoted_command
  case ":$PATH:" in
    *":$directory:"*) return 0 ;;
  esac
  printf -v quoted_directory '%q' "$directory"
  printf -v quoted_command '%q' "$directory/$command_name"
  {
    printf '\n%s is installed, but its directory is not on PATH.\n' "$command_name"
    printf 'Use it immediately: %s status\n' "$quoted_command"
    # shellcheck disable=SC2016 # literal $PATH is intended: the user pastes it
    printf 'For this terminal (zsh/bash): export PATH=%s:"$PATH"\n' "$quoted_directory"
    printf 'For new terminals, add that export line to ~/.zshrc (zsh) or ~/.bash_profile (bash), then reopen the terminal.\n'
  } >&2
}
