//! How an agent shows on its tab, as in Warp: its logo on a circle in its
//! brand color, with a badge in the corner for where its turn stands once
//! its hooks report one.

use gpui::{AnyElement, Hsla, Pixels, div, prelude::*, rgb};

use crate::{
    agent_events::AgentTurn,
    agents::Agent,
    components::icon,
    terminal_view::AgentState,
    theme::{self, Theme},
};

/// Share of the icon's box the brand circle, its logo, and the status
/// badge take.
const CIRCLE_SCALE: f32 = 0.76;
const LOGO_SCALE: f32 = 0.43;
const BADGE_SCALE: f32 = 0.57;
const BADGE_GLYPH_SCALE: f32 = 0.34;

pub fn turn_label(turn: AgentTurn) -> &'static str {
    match turn {
        AgentTurn::InProgress => "working",
        AgentTurn::Done => "done",
        AgentTurn::Failed => "failed",
        AgentTurn::Blocked => "needs you",
    }
}

pub fn turn_color(turn: AgentTurn, theme: &Theme) -> Hsla {
    let ansi = |index: usize| theme::to_hsla(theme.terminal.ansi[index]);
    match turn {
        AgentTurn::InProgress => ansi(5),
        AgentTurn::Done => ansi(2),
        AgentTurn::Failed => ansi(1),
        AgentTurn::Blocked => ansi(3),
    }
}

fn turn_icon(turn: AgentTurn) -> &'static str {
    match turn {
        AgentTurn::InProgress => "loader-circle",
        AgentTurn::Done => "check",
        AgentTurn::Failed => "triangle-alert",
        AgentTurn::Blocked => "stop-filled",
    }
}

/// An agent's circle color, its logo (under `icons/`), and whether the
/// logo is drawn dark to stand out on a light circle.
fn brand(agent: Agent) -> (Hsla, Option<&'static str>, bool) {
    let color = |hex: u32| Hsla::from(rgb(hex));
    match agent {
        Agent::ClaudeCode => (color(0xD97757), Some("brands/claude"), false),
        Agent::Codex => (color(0x000000), Some("brands/openai"), false),
        Agent::Gemini => (color(0x4285F4), Some("brands/gemini"), false),
        Agent::OpenCode => (color(0x808080), Some("brands/opencode"), false),
        Agent::Cursor => (color(0x26251E), Some("brands/cursor"), false),
        Agent::Amp => (color(0xF34E3F), None, false),
        Agent::Goose => (color(0x101010), None, false),
        Agent::Aider => (color(0x646464), None, false),
    }
}

/// The agent's icon in a `size` box. The status badge is ringed in
/// `cutout`, the color behind the icon, so it reads as cut out of the
/// circle.
pub fn agent_icon(state: &AgentState, size: Pixels, cutout: Hsla, theme: &Theme) -> AnyElement {
    let (circle, logo, dark_logo) = brand(state.agent);
    let logo_color = if dark_logo {
        Hsla::black()
    } else {
        Hsla::white()
    };
    let badge = state.turn.map(|turn| {
        div()
            .absolute()
            .right(-size * 0.08)
            .bottom(-size * 0.08)
            .flex()
            .items_center()
            .justify_center()
            .size(size * BADGE_SCALE)
            .rounded_full()
            .bg(cutout)
            .child(icon(
                turn_icon(turn),
                size * BADGE_GLYPH_SCALE,
                turn_color(turn, theme),
            ))
    });
    div()
        .relative()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(size)
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(size * CIRCLE_SCALE)
                .rounded_full()
                .bg(circle)
                .child(icon(logo.unwrap_or("bot"), size * LOGO_SCALE, logo_color)),
        )
        .children(badge)
        .into_any_element()
}
