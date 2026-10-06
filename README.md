# terminal

A macOS terminal built on [GPUI](https://github.com/zed-industries/zed) (Zed's UI framework) and [libghostty-vt](https://github.com/uzaaft/libghostty-rs) (Ghostty's terminal emulation, as a library). Ghostty parses the bytes and GPUI draws the grid. Everything around those two parts is written here: tabs, a block view of commands, an input editor, and sessions that keep running when the window closes.

It's early and macOS only.

## Building

You need Rust (pinned in `rust-toolchain.toml`), Zig 0.15, and Apple's Metal toolchain. Zig has to be exactly 0.15: the Ghostty source that libghostty-vt pins checks the major and minor version, and Homebrew's default `zig` is newer.

```sh
brew install zig@0.15
xcodebuild -downloadComponent MetalToolchain
PATH="$(brew --prefix zig@0.15)/bin:$PATH" cargo build
./target/debug/terminal
```

`.cargo/config.toml` builds Ghostty's Zig core with `ReleaseFast` even in dev builds. In Debug mode, parsing terminal output is dozens of times slower.

Notifications only show up when the app runs from a bundle. `script/bundle-mac` builds `target/release/Terminal.app`, or a debug bundle with `--debug`.

## Sessions

Shells don't belong to the window. The first launch starts a session runtime, a background copy of the same binary run with `--session-runtime`. The runtime owns every PTY and its Ghostty terminal. The app connects over a Unix socket and draws whatever the runtime sends. If you quit the app, the shells keep running, and opening it again reattaches them.

If the runtime itself goes away (a reboot, a crash, a new build), each pane starts a fresh shell in its last working directory. Claude Code and Codex sessions also come back if they were set up to report their session ids:

```sh
terminal integrate claude
terminal integrate codex
```

Layout and session state are written to `~/.config/terminal/session.json`, with backups and rolling snapshots next to it. The safeguards and the agent resume approach follow [herdr](https://github.com/herdrdev/herdr) (Apache-2.0).

## Command blocks

In zsh with shell integration on, each command and its output become a block, and you type into an editor pinned to the bottom of the pane, as in Warp. The editor highlights syntax, suggests from your history as you type, and opens zsh's own completions on Tab. Ctrl-R searches history.

A block shows the directory, git branch, and duration above the command, and is tinted red if the command failed. Click to select a block, ⌘-click to add another, ⇧-click to select a range. ⌘C copies the selected blocks. The ⋯ menu on each block has the rest: copy the command or output, filter the output by text or regex (with context lines), find within the block, bookmark, rerun.

Full-screen programs such as vim, htop, or an agent that takes the alternate screen get the whole pane as a normal terminal. Bash gets shell integration (working directory, prompt marks) but not blocks yet.

## Driving it from outside

The `terminal` binary is also a CLI for the running app. It talks to a control socket in the config directory:

```sh
terminal new --name logs -- tail -f /var/log/system.log
terminal send --tab 3 --enter "make test"
terminal read --tab 3 -n 20
```

`terminal mcp` serves the same API over MCP on stdio, so an agent can open tabs, type into panes, and read their output. Settings has a switch to turn the control socket off.

## Settings and themes

Settings live in `~/.config/terminal/settings.json` and are edited from the app's settings page (⌘,). The bundled themes are One, Ayu, and Gruvbox, each with light and dark variants. Their licenses are in `assets/themes/LICENSES`.

## Profiling

Run with `TERMINAL_TRACE=1` and both the app and the runtime write Chrome-format traces to `~/.config/terminal/traces/`. Open them in [ui.perfetto.dev](https://ui.perfetto.dev).

Benchmarks are kept as ignored tests:

```sh
cargo test --release benchmark -- --ignored --nocapture
```

## License

Apache-2.0. See [LICENSE](LICENSE).
