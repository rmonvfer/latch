//! An MCP server (over stdio) that gives agents the control API: they can
//! list tabs, open terminals and agents, type into panes, and read output.
//!
//! Run it as `terminal mcp`; it forwards every tool call to the running
//! app over the control socket.

use anyhow::Result;
use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::control::{self, NewTab, PaneTarget, Request, SplitDirection};

const INSTRUCTIONS: &str = "Controls the terminal app the user is running. Tabs and panes \
have numeric ids from list_tabs. Pane-targeted tools take either a pane id or a tab id \
(meaning that tab's focused pane); with neither they act on the focused pane of the active \
tab. send_input types text exactly as given: end it with a newline to run a command.";

#[derive(Deserialize, JsonSchema)]
struct Target {
    /// Tab id; acts on that tab's focused pane.
    tab: Option<u64>,
    /// Pane id.
    pane: Option<u64>,
}

impl From<Target> for PaneTarget {
    fn from(target: Target) -> Self {
        PaneTarget {
            tab: target.tab,
            pane: target.pane,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct OpenTabArgs {
    /// Directory to start in.
    cwd: Option<String>,
    /// Command to run in the new tab's shell.
    command: Option<String>,
    /// Agent profile to launch instead of a command, e.g. "Claude Code".
    agent: Option<String>,
    /// Name shown for the tab.
    name: Option<String>,
    /// Group to put the tab in, created if needed.
    group: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct TabArgs {
    /// Tab id from list_tabs.
    tab: u64,
}

#[derive(Deserialize, JsonSchema)]
struct RenameArgs {
    /// Tab id from list_tabs.
    tab: u64,
    /// New name; omit to show the program's own title.
    name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct SendArgs {
    #[serde(flatten)]
    target: Target,
    /// Text to type; include "\n" to press Enter.
    text: String,
}

#[derive(Deserialize, JsonSchema)]
struct ReadArgs {
    #[serde(flatten)]
    target: Target,
    /// How many recent non-empty lines to return (default 50).
    lines: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Direction {
    Right,
    Down,
}

#[derive(Deserialize, JsonSchema)]
struct SplitArgs {
    #[serde(flatten)]
    target: Target,
    direction: Direction,
    /// Command to run in the new pane.
    command: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct NotifyArgs {
    #[serde(flatten)]
    target: Target,
    title: String,
    body: Option<String>,
}

#[derive(Clone)]
struct TerminalServer {
    tool_router: ToolRouter<Self>,
}

/// Forward a request to the app off the async runtime, reporting failures
/// to the agent as tool errors.
async fn forward(request: Request) -> Result<CallToolResult, ErrorData> {
    let outcome = tokio::task::spawn_blocking(move || control::call(request))
        .await
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
    Ok(match outcome {
        Ok(Value::Object(map)) if map.is_empty() => {
            CallToolResult::success(vec![ContentBlock::text("ok")])
        }
        Ok(value) => {
            let text = match value.get("text").and_then(Value::as_str) {
                Some(text) => text.to_string(),
                None => serde_json::to_string_pretty(&value).unwrap_or_default(),
            };
            CallToolResult::success(vec![ContentBlock::text(text)])
        }
        Err(error) => CallToolResult::error(vec![ContentBlock::text(format!("{error:#}"))]),
    })
}

#[tool_router]
impl TerminalServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List open tabs with their ids, titles, groups, directories, git branches, coding-agent status, and panes."
    )]
    async fn list_tabs(&self) -> Result<CallToolResult, ErrorData> {
        forward(Request::ListTabs).await
    }

    #[tool(
        description = "Open a terminal tab, optionally running a command or launching an agent profile. Returns the new tab and pane ids."
    )]
    async fn open_tab(
        &self,
        Parameters(args): Parameters<OpenTabArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        forward(Request::NewTab(NewTab {
            cwd: args.cwd.map(Into::into),
            command: args.command,
            agent: args.agent,
            name: args.name,
            group: args.group,
        }))
        .await
    }

    #[tool(description = "Type text into a pane, as if the user typed it.")]
    async fn send_input(
        &self,
        Parameters(args): Parameters<SendArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        forward(Request::SendInput {
            target: args.target.into(),
            text: args.text,
        })
        .await
    }

    #[tool(description = "Read a pane's most recent output (screen and scrollback).")]
    async fn read_output(
        &self,
        Parameters(args): Parameters<ReadArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        forward(Request::ReadOutput {
            target: args.target.into(),
            lines: args.lines.unwrap_or(50),
        })
        .await
    }

    #[tool(
        description = "Split a pane to the right or downward, optionally running a command in the new pane."
    )]
    async fn split_pane(
        &self,
        Parameters(args): Parameters<SplitArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        forward(Request::Split {
            target: args.target.into(),
            direction: match args.direction {
                Direction::Right => SplitDirection::Right,
                Direction::Down => SplitDirection::Down,
            },
            command: args.command,
        })
        .await
    }

    #[tool(description = "Bring a tab to the front.")]
    async fn focus_tab(
        &self,
        Parameters(args): Parameters<TabArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        forward(Request::FocusTab { tab: args.tab }).await
    }

    #[tool(description = "Close a tab, ending the programs running in it.")]
    async fn close_tab(
        &self,
        Parameters(args): Parameters<TabArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        forward(Request::CloseTab { tab: args.tab }).await
    }

    #[tool(description = "Name a tab, or clear its name to show the program's title.")]
    async fn rename_tab(
        &self,
        Parameters(args): Parameters<RenameArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        forward(Request::RenameTab {
            tab: args.tab,
            name: args.name,
        })
        .await
    }

    #[tool(description = "Show the user a desktop notification attributed to a tab.")]
    async fn notify(
        &self,
        Parameters(args): Parameters<NotifyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        forward(Request::Notify {
            target: args.target.into(),
            title: args.title,
            body: args.body.unwrap_or_default(),
        })
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TerminalServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Serve MCP on stdin/stdout until the client disconnects.
pub fn serve() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let service = TerminalServer::new().serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_every_tool() {
        let names: Vec<String> = TerminalServer::new()
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        for expected in [
            "list_tabs",
            "open_tab",
            "send_input",
            "read_output",
            "split_pane",
            "focus_tab",
            "close_tab",
            "rename_tab",
            "notify",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing {expected}"
            );
        }
    }
}
