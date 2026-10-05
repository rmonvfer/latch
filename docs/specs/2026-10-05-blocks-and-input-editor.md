# Command blocks and the input editor

A pane shows its history as a list of blocks, one per command, and takes input in an editor of its own instead of the shell's line editor. The design follows Warp's (studied from its AGPL source and docs; no code reused) adapted to libghostty-vt and GPUI.

## Shell protocol

Shell integration reports the shell's lifecycle as hex-encoded JSON in a private DCS: `ESC P $ d <hex(JSON)> ESC \`. Hex encoding keeps bytes such as ESC or ST inside the payload from ending the sequence. The JSON envelope is `{"hook": "<Name>", "session": "<id>", "value": {...}}`.

| Hook | Value |
|---|---|
| `Bootstrapped` | shell name and version, histfile, aliases, functions, builtins |
| `Precmd` | exit code of the last command, cwd, git branch, virtualenv, conda env |
| `Preexec` | the command line as the shell received it |
| `CommandFinished` | exit code |

Each pane generates a random session id and passes it to its shell in `TERMINAL_SESSION_ID`. Hooks carrying any other session id are ignored, so a program printing a forged hook cannot fake command boundaries. Timestamps are taken by the app when hooks arrive. OSC 133 marks remain in place for prompt navigation in shells without hook support.

The scripts also rebind a kill-whole-line key (`^P`) on every prompt so the app can clear the line editor before injecting a command, and keep bracketed paste enabled.

## Blocks

A `BlockList` replaces the single terminal as the content of a pane. Each `Block` holds:

- metadata: command, cwd, git branch, environment, exit code, start and end times, state (`Prompting`, `Running`, `Finished`, `Background`);
- an output `Terminal` (libghostty-vt) that receives exactly that command's output, with its own scrollback.

Byte routing follows the hooks. Bytes before `Preexec` (the shell's prompt and its echo of the injected command) go to a discarded prompt terminal; the command text shown in the header comes from `Preexec`. Bytes after `Preexec` go to the running block's terminal. `CommandFinished` closes the block. Output arriving with no command running (background jobs) opens a `Background` block.

Full-screen programs switch the running block's terminal to the alternate screen; libghostty tracks this per terminal, and while it is active the pane renders that terminal alone, filling the pane, with keys passed straight through.

Finished blocks are converted once into an immutable styled-row snapshot (text runs with colors and attributes) and their terminal is dropped, which keeps memory bounded and makes painting a cheap row list. The running block paints from its live terminal. The list is virtualized with GPUI's variable-height `list`, scrolling as one document across blocks; block count and total rows are capped by the scrollback setting.

### Block header

The header shows context chips (cwd, git branch, virtualenv/conda), the command with syntax highlighting, and for finished blocks the exit status (red edge on failure) and duration. Blocks can be selected (click, `⌘↑`/`⌘↓`), copied (command, output, both), rerun, and collapsed.

## Input modes

A pane is in one of four modes, decided on every input event:

- **Classic**: the shell has not bootstrapped (unsupported shell, integration off, `ssh`, nested shells). The pane behaves as a plain terminal with today's single grid.
- **Editor**: the shell is idle at a prompt. Keys go to the input editor.
- **Running**: a command has run for more than 50 ms. The editor hides; keys go raw to the PTY through the key encoder, so REPLs, password prompts, and stdin readers work. `Ctrl-C` sends ETX.
- **Full screen**: the running block's terminal is on the alternate screen.

## Input editor

A multi-line GPUI editor pinned below the block list. `Enter` runs, `Shift-Enter` inserts a newline. Running a command writes `^P` (clear line), the command in bracketed paste, then a newline, only once the shell is ready (after `Precmd` and the prompt's input mark); earlier submissions are queued.

- History: the shell's histfile (zsh extended and plain formats, bash) merged with the app's own per-directory history; `↑`/`↓` walk it, `Ctrl-R` opens a fuzzy search.
- Autosuggestions: the most recent history entry extending the current text, shown as ghost text, accepted with `→`.
- Highlighting: the command word is colored by whether it is an alias, function, builtin, or executable on PATH (from `Bootstrapped` data and a PATH scan); strings, flags, and paths are tinted.
- Completions: native zsh completions. A completion request writes a hidden widget invocation that runs zsh's completion system with `compadd` redirected to emit matches (with descriptions) over a private OSC, framed by start and end marks. The editor shows them in a menu. bash falls back to command and path completion.

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
