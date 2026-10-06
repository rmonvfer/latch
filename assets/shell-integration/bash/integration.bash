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

  # Hooks report the shell's state as hex-encoded JSON in a private DCS,
  # ESC P $ d <hex(JSON)> ESC \, stamped with TERMINAL_SESSION_ID.
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

  _terminal_json_array() {
    local item joined=
    for item in "$@"; do
      _terminal_json "$item"
      joined+="${joined:+,}$REPLY"
    done
    REPLY="[$joined]"
  }

  _terminal_hook() {
    [[ -n "$TERMINAL_SESSION_ID" ]] || return
    local message hex
    message="{\"hook\":\"$1\",\"session\":\"$TERMINAL_SESSION_ID\",\"value\":$2}"
    hex=$(printf '%s' "$message" | od -An -v -tx1 | tr -d ' \n')
    printf '\eP$d%s\e\\' "$hex"
  }

  # The terminal sends this key to clear the line before typing a command.
  bind -m emacs '"\e[9876~": kill-whole-line' 2>/dev/null
  bind -m vi-insert '"\e[9876~": kill-whole-line' 2>/dev/null

  _terminal_prompt_command() {
    local exit_status=$?
    if [[ -n "$_terminal_command_running" ]]; then
      printf '\e]133;D;%s\a' "$exit_status"
      _terminal_hook CommandFinished "{\"exit_code\":$exit_status}"
      _terminal_command_running=
    fi
    local cwd venv conda
    _terminal_json "$PWD"; cwd=$REPLY
    _terminal_json "${VIRTUAL_ENV##*/}"; venv=$REPLY
    _terminal_json "$CONDA_DEFAULT_ENV"; conda=$REPLY
    _terminal_hook Precmd "{\"exit_code\":$exit_status,\"cwd\":$cwd,\"virtualenv\":$venv,\"conda_env\":$conda}"
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
      local line
      line=$(HISTTIMEFORMAT= builtin history 1)
      line=${line#*[0-9] }
      line=${line# }
      _terminal_json "${line:-$BASH_COMMAND}"
      _terminal_hook Preexec "{\"command\":$REPLY}"
      printf '\e]133;C\a'
    fi
  }

  _terminal_bootstrapped() {
    local histfile version path alias_names function_names builtin_names
    _terminal_json "${HISTFILE:-}"; histfile=$REPLY
    _terminal_json "$PATH"; path=$REPLY
    _terminal_json "$BASH_VERSION"; version=$REPLY
    _terminal_json_array $(compgen -a); alias_names=$REPLY
    _terminal_json_array $(compgen -A function); function_names=$REPLY
    _terminal_json_array $(compgen -b); builtin_names=$REPLY
    _terminal_hook Bootstrapped "{\"shell\":\"bash\",\"version\":$version,\"histfile\":$histfile,\"path\":$path,\"aliases\":$alias_names,\"functions\":$function_names,\"builtins\":$builtin_names}"
  }
  _terminal_bootstrapped

  PROMPT_COMMAND="_terminal_prompt_command${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
  trap '_terminal_preexec' DEBUG
fi
