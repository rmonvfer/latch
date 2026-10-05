# Shell integration: semantic prompt marks (OSC 133) and lifecycle hooks.
#
# OSC 133 marks where prompts, input, and output begin:
#   A  prompt starts      B  input starts
#   C  command runs       D;<status>  command finished
#
# Hooks report the shell's state as hex-encoded JSON in a private DCS,
#   ESC P $ d <hex(JSON)> ESC \
# so the terminal can show each command as a block. Every hook carries the
# pane's TERMINAL_SESSION_ID; the terminal ignores hooks without it.

autoload -Uz add-zsh-hook

_terminal_mark_input='%{'$'\e]133;B\a''%}'
# The terminal sends this key to clear the line before typing a command.
_terminal_clear_line_key=$'\e[9876~'

# JSON string literal for $1.
_terminal_json() {
  local value=$1
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
  value=${value//$'\n'/\\n}
  value=${value//$'\r'/\\r}
  value=${value//$'\t'/\\t}
  value=${value//[[:cntrl:]]/}
  REPLY="\"$value\""
}

# JSON array of strings from the arguments.
_terminal_json_array() {
  local item items=()
  for item in "$@"; do
    _terminal_json "$item"
    items+=("$REPLY")
  done
  REPLY="[${(j:,:)items}]"
}

# Send hook $1 with JSON object body $2.
_terminal_hook() {
  [[ -n "$TERMINAL_SESSION_ID" ]] || return
  local message hex
  message="{\"hook\":\"$1\",\"session\":\"$TERMINAL_SESSION_ID\",\"value\":$2}"
  hex=$(printf '%s' "$message" | od -An -v -tx1 | tr -d ' \n')
  printf '\eP$d%s\e\\' "$hex"
}

_terminal_bind_clear_line() {
  local keymap
  for keymap in emacs viins vicmd; do
    bindkey -M "$keymap" "$_terminal_clear_line_key" kill-whole-line 2>/dev/null
  done
}

_terminal_precmd() {
  local exit_status=$?
  if [[ -n "$_terminal_command_running" ]]; then
    printf '\e]133;D;%s\a' "$exit_status"
    _terminal_hook CommandFinished "{\"exit_code\":$exit_status}"
    _terminal_command_running=
  fi
  local cwd venv conda
  _terminal_json "$PWD"; cwd=$REPLY
  _terminal_json "${VIRTUAL_ENV:t}"; venv=$REPLY
  _terminal_json "$CONDA_DEFAULT_ENV"; conda=$REPLY
  _terminal_hook Precmd "{\"exit_code\":$exit_status,\"cwd\":$cwd,\"virtualenv\":$venv,\"conda_env\":$conda}"
  # Themes and configs may rebind keys or rebuild PS1 on every prompt.
  _terminal_bind_clear_line
  printf '\e]133;A\a'
  if [[ "$PS1" != *"$_terminal_mark_input" ]]; then
    PS1="$PS1$_terminal_mark_input"
  fi
}

_terminal_preexec() {
  _terminal_command_running=1
  _terminal_json "$1"
  _terminal_hook Preexec "{\"command\":$REPLY}"
  printf '\e]133;C\a'
}

_terminal_bootstrapped() {
  # Locals avoid the names aliases/functions/builtins, which would hide
  # zsh's own tables of those names.
  local histfile alias_names function_names builtin_names version path_value
  _terminal_json "${HISTFILE:-}"; histfile=$REPLY
  _terminal_json "$PATH"; path_value=$REPLY
  _terminal_json "$ZSH_VERSION"; version=$REPLY
  _terminal_json_array ${(k)aliases}; alias_names=$REPLY
  _terminal_json_array ${(k)functions}; function_names=$REPLY
  _terminal_json_array ${(k)builtins}; builtin_names=$REPLY
  _terminal_hook Bootstrapped "{\"shell\":\"zsh\",\"version\":$version,\"histfile\":$histfile,\"path\":$path_value,\"aliases\":$alias_names,\"functions\":$function_names,\"builtins\":$builtin_names}"
}

# Run after the user's own hooks (installed later by .zshrc) by moving to
# the end of the hook lists on the first prompt.
_terminal_install() {
  add-zsh-hook -d precmd _terminal_install
  add-zsh-hook -d precmd _terminal_precmd
  add-zsh-hook precmd _terminal_precmd
  add-zsh-hook -d preexec _terminal_preexec
  add-zsh-hook preexec _terminal_preexec
  _terminal_bootstrapped
  _terminal_precmd
}

add-zsh-hook precmd _terminal_install
