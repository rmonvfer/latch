//! The `terminal` command line: control a running app from a shell.
//!
//! Commands talk to the app over the control socket. Inside a terminal
//! tab they default to that tab's pane, through `TERMINAL_PANE_ID`.

use std::{
    io::{Read, Write},
    path::PathBuf,
};

use anyhow::{Context as _, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::Value;

use crate::{
    agent_events,
    agent_resume::{self, ResumableAgent},
    control::{self, NewTab, PaneTarget, Request, SplitDirection},
    mcp,
    terminal_view::PANE_ID_VARIABLE,
};

#[derive(Parser)]
#[command(name = env!("CARGO_PKG_NAME"), version, about = "Control the terminal app")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List tabs and their panes.
    List {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Open a tab, optionally running a command or an agent profile.
    New {
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
        /// Put the tab in this group, creating it if needed.
        #[arg(long)]
        group: Option<String>,
        /// Agent profile to launch, e.g. "Claude Code".
        #[arg(long, conflicts_with = "command")]
        agent: Option<String>,
        /// Command to run in the tab's shell.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Bring a tab to the front.
    Focus { tab: u64 },
    /// Close a tab.
    Close { tab: u64 },
    /// Name a tab; with no name, show the program's own title again.
    Rename { tab: u64, name: Option<String> },
    /// Type text into a pane.
    Send {
        #[command(flatten)]
        target: TargetArgs,
        /// Press Enter after the text.
        #[arg(long, short = 'e')]
        enter: bool,
        text: String,
    },
    /// Print a pane's recent output.
    Read {
        #[command(flatten)]
        target: TargetArgs,
        #[arg(long, short = 'n', default_value_t = 50)]
        lines: usize,
    },
    /// Split a pane, optionally running a command in the new one.
    Split {
        #[command(flatten)]
        target: TargetArgs,
        #[arg(value_enum, default_value_t = Direction::Right)]
        direction: Direction,
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Show a desktop notification from a pane's tab.
    Notify {
        #[command(flatten)]
        target: TargetArgs,
        title: String,
        body: Option<String>,
    },
    /// Serve the control API to an agent over MCP (stdio).
    Mcp,
    /// Set up a coding agent (claude or codex) to report its sessions, so
    /// panes resume them after the session runtime restarts.
    Integrate { agent: String },
    /// Run by an agent's SessionStart hook, with the hook's JSON on stdin.
    #[command(hide = true)]
    AgentHook { agent: String },
}

#[derive(Args)]
struct TargetArgs {
    /// Tab id (its focused pane).
    #[arg(long, conflicts_with = "pane")]
    tab: Option<u64>,
    /// Pane id; defaults to the pane this command runs in.
    #[arg(long)]
    pane: Option<u64>,
}

impl TargetArgs {
    fn resolve(self) -> PaneTarget {
        let pane = self.pane.or_else(|| {
            self.tab
                .is_none()
                .then(|| std::env::var(PANE_ID_VARIABLE).ok()?.parse().ok())
                .flatten()
        });
        PaneTarget {
            tab: self.tab,
            pane,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Direction {
    Right,
    Down,
}

impl From<Direction> for SplitDirection {
    fn from(direction: Direction) -> Self {
        match direction {
            Direction::Right => SplitDirection::Right,
            Direction::Down => SplitDirection::Down,
        }
    }
}

/// Run as a command-line client when the process was started with
/// arguments. Returns the exit code, or `None` to start the app.
pub fn run_from_args() -> Option<i32> {
    if std::env::args_os().len() <= 1 {
        return None;
    }
    let cli = Cli::parse();
    Some(match run(cli.command) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{}: {error:#}", env!("CARGO_PKG_NAME"));
            1
        }
    })
}

fn joined(words: Vec<String>) -> Option<String> {
    (!words.is_empty()).then(|| words.join(" "))
}

/// Report what a hook announces, a session or a turn's progress, to the
/// pane it runs in. Hooks must never get in an agent's way, so every
/// failure is silent.
fn report_agent_hook(agent: &str) {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    // Outside this app's panes there is nothing to report to.
    if std::env::var_os(control::TOKEN_VARIABLE).is_none() {
        return;
    }
    let (Some(resumable), Ok(payload)) = (
        ResumableAgent::parse(agent),
        serde_json::from_str::<Value>(&input),
    ) else {
        return;
    };
    let request = if let Some(session) = agent_resume::session_from_hook(resumable, &payload) {
        Request::ReportAgentSession {
            agent: agent.to_string(),
            session_id: session.session_id,
        }
    } else if let Some(event) = agent_events::event_from_hook(&payload) {
        Request::ReportAgentEvent {
            agent: agent.to_string(),
            event,
        }
    } else {
        return;
    };
    let _ = control::ControlClient::connect().and_then(|mut client| client.request(request));
}

fn run(command: Command) -> Result<()> {
    match command {
        Command::List { json } => {
            let result = control::call(Request::ListTabs)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                print_tabs(&result);
            }
        }
        Command::New {
            cwd,
            name,
            group,
            agent,
            command,
        } => {
            let cwd = cwd.or_else(|| std::env::current_dir().ok());
            let result = control::call(Request::NewTab(NewTab {
                cwd,
                command: joined(command),
                agent,
                name,
                group,
            }))?;
            print_ids(&result);
        }
        Command::Focus { tab } => {
            control::call(Request::FocusTab { tab })?;
        }
        Command::Close { tab } => {
            control::call(Request::CloseTab { tab })?;
        }
        Command::Rename { tab, name } => {
            control::call(Request::RenameTab { tab, name })?;
        }
        Command::Send {
            target,
            enter,
            mut text,
        } => {
            if enter {
                text.push('\r');
            }
            control::call(Request::SendInput {
                target: target.resolve(),
                text,
            })?;
        }
        Command::Read { target, lines } => {
            let result = control::call(Request::ReadOutput {
                target: target.resolve(),
                lines,
            })?;
            let text = result
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut stdout = std::io::stdout();
            writeln!(stdout, "{text}")?;
        }
        Command::Split {
            target,
            direction,
            command,
        } => {
            let result = control::call(Request::Split {
                target: target.resolve(),
                direction: direction.into(),
                command: joined(command),
            })?;
            print_ids(&result);
        }
        Command::Notify {
            target,
            title,
            body,
        } => {
            control::call(Request::Notify {
                target: target.resolve(),
                title,
                body: body.unwrap_or_default(),
            })?;
        }
        Command::Mcp => mcp::serve().context("the MCP server stopped")?,
        Command::Integrate { agent } => {
            let agent = ResumableAgent::parse(&agent)
                .with_context(|| format!("no integration for {agent:?}; use claude or codex"))?;
            for path in agent_resume::install(agent)? {
                println!("updated {}", path.display());
            }
            println!(
                "{} now reports its sessions and turns: tabs show what it works on, and panes running it resume after a restart.",
                agent.display_name()
            );
        }
        Command::AgentHook { agent } => report_agent_hook(&agent),
    }
    Ok(())
}

fn print_ids(result: &Value) {
    let field = |key: &str| result.get(key).and_then(Value::as_u64);
    match (field("tab"), field("pane")) {
        (Some(tab), Some(pane)) => println!("tab {tab} pane {pane}"),
        (Some(tab), None) => println!("tab {tab}"),
        _ => {}
    }
}

fn print_tabs(result: &Value) {
    let Some(tabs) = result.get("tabs").and_then(Value::as_array) else {
        return;
    };
    for tab in tabs {
        let text = |key: &str| tab.get(key).and_then(Value::as_str).unwrap_or_default();
        let marker = if tab.get("active").and_then(Value::as_bool) == Some(true) {
            "*"
        } else {
            " "
        };
        let mut line = format!(
            "{marker} {:>4}  {}",
            tab.get("id").and_then(Value::as_u64).unwrap_or_default(),
            text("title")
        );
        if !text("group").is_empty() {
            line.push_str(&format!("  [{}]", text("group")));
        }
        if let Some(agent) = tab.get("agent").filter(|agent| !agent.is_null()) {
            let field = |key: &str| agent.get(key).and_then(Value::as_str).unwrap_or_default();
            line.push_str(&format!("  ({} — {})", field("name"), field("status")));
        }
        if !text("directory").is_empty() {
            line.push_str(&format!("  {}", text("directory")));
        }
        println!("{line}");
        let panes = tab
            .get("panes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if panes.len() > 1 {
            for pane in panes {
                let field = |key: &str| pane.get(key).and_then(Value::as_str).unwrap_or_default();
                println!(
                    "        pane {}  {}",
                    pane.get("id").and_then(Value::as_u64).unwrap_or_default(),
                    field("title")
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_trailing_commands() {
        let cli = Cli::try_parse_from([
            "terminal", "new", "--name", "api", "--", "npm", "run", "dev",
        ])
        .unwrap();
        let Command::New { name, command, .. } = cli.command else {
            panic!("expected new");
        };
        assert_eq!(name.as_deref(), Some("api"));
        assert_eq!(joined(command).as_deref(), Some("npm run dev"));
    }
}
