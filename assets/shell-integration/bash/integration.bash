# Started as `bash --posix` with ENV pointing here. Leave POSIX mode, load
# the user's usual startup files, then install the prompt marks (OSC 133).

set +o posix
unset ENV

if [[ -n "$TERMINAL_BASH_LOGIN" ]]; then
  unset TERMINAL_BASH_LOGIN
  [[ -r /etc/profile ]] && source /etc/profile
  for _terminal_profile in ~/.bash_profile ~/.bash_login ~/.profile; do
    if [[ -r "$_terminal_profile" ]]; then
      source "$_terminal_profile"
      break
    fi
  done
  unset _terminal_profile
else
  [[ -r ~/.bashrc ]] && source ~/.bashrc
fi

if [[ $- == *i* ]]; then
  _terminal_command_running=

  _terminal_prompt_command() {
    local exit_status=$?
    if [[ -n "$_terminal_command_running" ]]; then
      printf '\e]133;D;%s\a' "$exit_status"
      _terminal_command_running=
    fi
    printf '\e]133;A\a'
    if [[ "$PS1" != *'\]133;B'* ]]; then
      PS1="$PS1"'\[\e]133;B\a\]'
    fi
    return $exit_status
  }

  _terminal_preexec() {
    # The DEBUG trap fires for PROMPT_COMMAND too; only mark real commands.
    [[ -n "$COMP_LINE" || "$BASH_COMMAND" == "$PROMPT_COMMAND" || "$BASH_COMMAND" == _terminal_prompt_command* ]] && return
    if [[ -z "$_terminal_command_running" ]]; then
      _terminal_command_running=1
      printf '\e]133;C\a'
    fi
  }

  PROMPT_COMMAND="_terminal_prompt_command${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
  trap '_terminal_preexec' DEBUG
fi
