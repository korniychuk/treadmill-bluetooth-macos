#!/usr/bin/env bash
# Shared, advisory-only PATH guidance. Never edits a user's shell files.
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
    printf 'For this terminal (zsh/bash): export PATH=%s:"$PATH"\n' "$quoted_directory"
    printf 'For new terminals, add that export line to ~/.zshrc (zsh) or ~/.bash_profile (bash), then reopen the terminal.\n'
  } >&2
}
