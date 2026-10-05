//! How agent status looks: a status icon and a short label.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, Hsla, Pixels, Transformation, percentage, prelude::*,
};

use crate::{
    agents::AgentStatus,
    components::icon,
    terminal_view::AgentState,
    theme::{self, Theme},
};

pub fn status_label(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Working => "working",
        AgentStatus::NeedsInput => "needs input",
        AgentStatus::Idle => "idle",
    }
}

pub fn status_color(status: AgentStatus, theme: &Theme) -> Hsla {
    match status {
        AgentStatus::Working => theme.text_accent,
        AgentStatus::NeedsInput => theme::to_hsla(theme.terminal.ansi[3]),
        AgentStatus::Idle => theme.text_muted,
    }
}

/// The status icon: a spinner while working, an alert when the agent
/// wants the user, a robot otherwise.
pub fn status_icon(state: AgentState, size: Pixels, theme: &Theme) -> AnyElement {
    let color = status_color(state.status, theme);
    match state.status {
        AgentStatus::Working => icon("loader-circle", size, color)
            .with_animation(
                "agent-working",
                Animation::new(Duration::from_millis(900)).repeat(),
                |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))),
            )
            .into_any_element(),
        AgentStatus::NeedsInput => icon("circle-alert", size, color).into_any_element(),
        AgentStatus::Idle => icon("bot", size, color).into_any_element(),
    }
}
