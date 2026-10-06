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
# The terminal sends this key, after typing the text before the cursor, to
# ask for completions.
_terminal_complete_key=$'\e[9877~'
# Most completions reported at once.
_terminal_completion_limit=500

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
    bindkey -M "$keymap" "$_terminal_complete_key" _terminal_complete 2>/dev/null
  done
}

# Stands in for compadd while completions are captured: lets compadd do
# the matching into arrays instead of the command line, then records each
# match as the text that would be inserted, with its description.
_terminal_compadd() {
  # Calls that already collect matches into arrays pass through.
  if [[ ${@[1,(i)(-|--)]} == *-(O|A|D)\ * ]]; then
    builtin compadd "$@"
    return
  fi
  # Options, which may be clustered (-Qf) and take their argument attached
  # or as the next word.
  local -A opts
  local arg cluster letter
  integer i=1 j
  while (( i <= $# )); do
    arg=${@[i]}
    [[ $arg == -* && $arg != - && $arg != -- ]] || break
    cluster=${arg#-}
    for (( j = 1; j <= $#cluster; j++ )); do
      letter=${cluster[j]}
      if [[ $letter == [PSpsiIWJVXxrRMFOADdE] ]]; then
        if (( j < $#cluster )); then
          opts[$letter]=${cluster[j+1,-1]}
        else
          (( i++ ))
          opts[$letter]=${@[i]}
        fi
        break
      fi
      opts[$letter]=1
    done
    (( i++ ))
  done
  local -a hits descriptions
  if [[ -n ${opts[d]-} ]]; then
    if [[ ${opts[d]} == \(* ]]; then
      # An inline array: split it into words without evaluating it, since
      # its text can come from file names or other untrusted data.
      descriptions=("${(@Q)${(z)${${opts[d]#\(}%\)}}}")
    else
      descriptions=("${(@P)opts[d]}")
    fi
  fi
  builtin compadd -A hits -D descriptions "$@"
  (( $#hits )) || return 1
  # Adding the matches for real tells zsh completion succeeded, so it does
  # not retry with further matchers; nothing is inserted or listed.
  builtin compadd "$@"
  local hit word description suffix index
  for index in {1..$#hits}; do
    (( $#_terminal_completion_words < _terminal_completion_limit )) || return 0
    hit=$hits[$index]
    (( ${+opts[Q]} )) || hit=${(q)hit}
    suffix=
    if [[ ${opts[S]-} == / ]] || { (( ${+opts[f]} )) && [[ -d ${opts[W]-}${hits[$index]} ]] }; then
      suffix=/
    fi
    word="${opts[P]-}${opts[p]-}$hit${opts[s]-}$suffix"
    description=${descriptions[$index]-}
    if [[ $description == *' -- '* ]]; then
      description=${description#* -- }
    else
      description=
    fi
    _terminal_completion_words+=("$word")
    _terminal_completion_descriptions+=("$description")
  done
}

# Completion widget body: runs zsh's completion system with compadd
# captured, inserting and listing nothing.
_terminal_capture_completions() {
  _terminal_completion_prefix=$PREFIX
  compadd() { _terminal_compadd "$@" }
  {
    _main_complete
  } always {
    unfunction compadd
  }
  compstate[insert]=
  compstate[list]=
}

# Report completions for the text typed before the key, then clear the
# line, which the terminal's own editor still holds.
_terminal_complete() {
  _terminal_completion_words=()
  _terminal_completion_descriptions=()
  _terminal_completion_prefix=
  if (( $+functions[_main_complete] )); then
    zle _terminal_capture
  fi
  local prefix words descriptions
  _terminal_json "$_terminal_completion_prefix"; prefix=$REPLY
  _terminal_json_array "${_terminal_completion_words[@]}"; words=$REPLY
  _terminal_json_array "${_terminal_completion_descriptions[@]}"; descriptions=$REPLY
  _terminal_hook Completions "{\"prefix\":$prefix,\"words\":$words,\"descriptions\":$descriptions}"
  BUFFER=
}
zle -C _terminal_capture complete-word _terminal_capture_completions
zle -N _terminal_complete

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
