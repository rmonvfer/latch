# Loaded because the terminal points ZDOTDIR here. Restore the user's
# ZDOTDIR so zsh reads their own startup files next, load their .zshenv,
# then install the terminal's prompt hooks for interactive shells.
if [[ -n "${TERMINAL_ORIG_ZDOTDIR+set}" ]]; then
  export ZDOTDIR="$TERMINAL_ORIG_ZDOTDIR"
  unset TERMINAL_ORIG_ZDOTDIR
else
  unset ZDOTDIR
fi

if [[ -f "${ZDOTDIR:-$HOME}/.zshenv" ]]; then
  source "${ZDOTDIR:-$HOME}/.zshenv"
fi

if [[ -o interactive && -n "$TERMINAL_SHELL_INTEGRATION_DIR" ]]; then
  source "$TERMINAL_SHELL_INTEGRATION_DIR/zsh/integration.zsh"
fi
