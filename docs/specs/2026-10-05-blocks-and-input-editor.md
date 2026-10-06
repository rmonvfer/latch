# Command blocks and the input editor

A pane shows its history as a list of blocks, one per command, and takes input in an editor of its own instead of the shell's line editor. The design follows Warp's (studied from its AGPL source and docs; no code reused) adapted to libghostty-vt and GPUI.

## Shell protocol

Shell integration reports the shell's lifecycle as hex-encoded JSON in a private DCS: `ESC P $ d <hex(JSON)> ESC \`. Hex encoding keeps bytes such as ESC or ST inside the payload from ending the sequence. The JSON envelope is `{"hook": "<Name>", "session": "<id>", "value": {...}}`.

| Hook | Value |
|---|---|
| `Bootstrapped` | shell name and version, histfile, PATH, aliases, functions, builtins |
| `Precmd` | exit code of the last command, cwd, virtualenv, conda env |
| `Preexec` | the command line as the shell received it |
| `CommandFinished` | exit code |
| `Completions` | the completed prefix, matching words, and their descriptions |

Each pane generates a random session id and passes it to its shell in `TERMINAL_SESSION_ID`. Hooks carrying any other session id are ignored, so a program printing a forged hook cannot fake command boundaries. Timestamps are taken by the app when hooks arrive. OSC 133 marks remain in place for prompt navigation in shells without hook support.

The scripts also rebind two private keys on every prompt: `ESC [ 9876 ~` runs kill-whole-line, so the app can clear the line editor before injecting a command, and in zsh `ESC [ 9877 ~` reports completions for the text typed before it.

## Blocks

Blocks live in the session runtime, which owns the shell, so they survive closing the window like the rest of the session. A launch asks for them with `command_blocks`; the runtime turns them on when the shell's `Bootstrapped` hook arrives, keeping anything the shell printed while starting as a block without a command.

Byte routing follows the hooks. At `Preexec` the runtime swaps in a fresh terminal for the command's output; at `CommandFinished` it encodes every row of that terminal, scrollback included, as the same self-contained styled VT rows snapshots carry, and swaps in a fresh terminal for the next prompt. The prompt's terminal is never shown: the window's editor stands in for it. Each block keeps its raw output within a 64 MiB budget, so its rows are rebuilt when the width or colors change.

Snapshots carry a summary of the blocks (command, context, exit code, duration, row count, version), the prompt's context, and the latest completions. The client fetches rows a summary announces and the view has not seen, and the view decodes them into paintable rows. A running block's rows that scrolled off its terminal arrive the same way; its live screen is the snapshot itself.

Full-screen programs switch the running terminal to the alternate screen; while that lasts the pane shows the snapshot alone, filling the pane, with keys passed straight through.

The list is virtualized with GPUI's variable-height `list`, scrolling as one document across blocks and following new output. Only rows inside the visible area are painted.

### Block header

The header shows context chips (cwd, git branch, virtualenv/conda), the command with syntax highlighting, and for finished blocks the exit status (red edge on failure) and duration. Blocks can be selected (click, `⌘↑`/`⌘↓`), copied (command, output, both), rerun, and collapsed.

## Input modes

A pane is in one of four modes, decided on every input event:

- **Classic**: the shell has not bootstrapped (unsupported shell, integration off, `ssh`, nested shells). The pane behaves as a plain terminal with today's single grid.
- **Editor**: the shell is idle at a prompt. Keys go to the input editor.
- **Running**: a command has run for more than 50 ms. The editor hides; keys go raw to the PTY through the key encoder, so REPLs, password prompts, and stdin readers work. `Ctrl-C` sends ETX.
- **Full screen**: the running block's terminal is on the alternate screen.

## Input editor

A multi-line GPUI editor pinned below the block list. `Enter` runs, `Shift-Enter` inserts a newline. Running a command writes the clear-line key, the command in bracketed paste, then Enter, only once the shell is ready (after `Precmd` and the prompt's input mark); earlier submissions are queued.

- History: the shell's histfile (zsh extended and plain formats, bash) merged with the app's own per-directory history; `↑`/`↓` walk it, `Ctrl-R` opens a fuzzy search.
- Autosuggestions: the most recent history entry extending the current text, shown as ghost text, accepted with `→`.
- Highlighting: the command word is colored by whether it is an alias, function, builtin, or executable on PATH (from `Bootstrapped` data and a PATH scan); strings, flags, and paths are tinted.
- Completions: native zsh completions. Tab writes the clear-line key, the text before the cursor, and the completion key, whose widget runs zsh's completion system with `compadd` wrapped to record matches (with descriptions) and reports them in a `Completions` hook. One match completes at once; several extend the word by their shared start and open a menu that narrows as you type. bash falls back to command and path completion.

## Existing features in block mode

Search runs over every block's text and highlights matches in the list. Links and selection work within a block's rows. The control API reads output from blocks (`read_output` returns recent blocks' output with their commands). Splits, tabs, sessions, and notifications are unchanged; `CommandFinished` drives the long-command notification.

## Delivery

1. Protocol: hooks in the zsh and bash scripts, DCS parser with session-id check, unit tests against recorded shell output.
2. Block model and routing with the classic fallback; snapshotting finished blocks.
3. Block list rendering and headers.
4. Input editor with modes, command injection, and readiness gating.
5. History, autosuggestions, highlighting, `Ctrl-R`.
6. Native completions.
7. Block actions, search and links across blocks, control API.

Each step lands with tests and an end-to-end check through a PTY harness that drives a real zsh with the user's own configuration.
