# Semantic prompt marks (OSC 133) so the terminal knows where prompts,
# input, and command output begin, and how each command exited.
#
#   A  prompt starts      B  input starts
#   C  command runs       D;<status>  command finished

autoload -Uz add-zsh-hook

_terminal_mark_input='%{'$'\e]133;B\a''%}'

_terminal_precmd() {
  local exit_status=$?
  if [[ -n "$_terminal_command_running" ]]; then
    printf '\e]133;D;%s\a' "$exit_status"
    _terminal_command_running=
  fi
  printf '\e]133;A\a'
  # Prompt themes may rebuild PS1 every time, so re-add the input mark.
  if [[ "$PS1" != *"$_terminal_mark_input" ]]; then
    PS1="$PS1$_terminal_mark_input"
  fi
}

_terminal_preexec() {
  _terminal_command_running=1
  printf '\e]133;C\a'
}

# Run after the user's own hooks (installed later by .zshrc) by moving to
# the end of the hook lists on the first prompt.
_terminal_install() {
  add-zsh-hook -d precmd _terminal_install
  add-zsh-hook -d precmd _terminal_precmd
  add-zsh-hook precmd _terminal_precmd
  add-zsh-hook -d preexec _terminal_preexec
  add-zsh-hook preexec _terminal_preexec
  _terminal_precmd
}

add-zsh-hook precmd _terminal_install
