use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use gpui::{
    Action, AnyElement, App, AsyncApp, ClickEvent, Context, CursorStyle, Entity, FocusHandle,
    Focusable, Modifiers, MouseButton, MouseMoveEvent, Pixels, Point, PromptLevel, ScrollHandle,
    SharedString, Subscription, Task, WeakEntity, Window, actions, div, prelude::*, px,
};

use crate::{
    agents::{AgentProfile, AgentStatus},
    components::{self, icon, icon_button, keybinding},
    confirm::confirm_close,
    git::{self, DiffStats},
    notifications,
    pane_group::{Detached, DraggedPane, Edge, PaneGroup, PaneGroupEvent},
    pane_tree::PaneNode,
    process_info::shorten_home,
    runtime,
    runtime_protocol::SessionInfo,
    session::{self, EntryState, GroupState, SessionState, TabKind, TabState},
    settings::SettingsStore,
    settings_page::SettingsPage,
    sidebar::{SidebarDrop, sidebar_child_index},
    status_bar::{StatusItem, format_duration},
    tabs::{Entry, GroupId, Row, TabColor, TabDestination, TabIcon, TabId, TabLayout, TabStyle},
    terminal_view::{AgentState, Attention, TabMetadata, TerminalView},
    text_input::{TextInput, TextInputEvent},
    theme::{self, ActiveTheme, ActiveThemeExt, Theme},
};

actions!(
    workspace,
    [
        NewTab,
        CloseTab,
        StopTab,
        CloseWindow,
        NextAttention,
        NextTab,
        PreviousTab,
        ToggleSidebar,
        OpenSettings,
        RenameTab,
        Quit
    ]
);

fn clamp_sidebar_width(width: Pixels) -> Pixels {
    width.clamp(theme::SIDEBAR_MIN_WIDTH, theme::SIDEBAR_MAX_WIDTH)
}

fn restored_tab(index: Option<usize>, slots: &[Option<TabId>]) -> Option<TabId> {
    index.and_then(|index| slots.get(index)).copied().flatten()
}

fn next_tab_after_close(ordered: &[TabId], closing: TabId) -> Option<TabId> {
    let position = ordered.iter().position(|id| *id == closing)?;
    ordered
        .get(position + 1)
        .or_else(|| position.checked_sub(1).and_then(|index| ordered.get(index)))
        .copied()
}

fn sessions_using_worktree(
    sessions: &[SessionInfo],
    owned: &HashSet<u64>,
    path: &Path,
) -> Vec<u64> {
    sessions
        .iter()
        .filter(|session| {
            !session.exited
                && (owned.contains(&session.id)
                    || session
                        .cwd
                        .as_ref()
                        .is_some_and(|cwd| cwd.starts_with(path)))
        })
        .map(|session| session.id)
        .collect()
}

/// Activate the tab at the given index; `usize::MAX` selects the last tab.
#[derive(Clone, Debug, PartialEq, Action)]
#[action(namespace = workspace, no_json)]
pub struct ActivateTab(pub usize);

/// Open a tab running the agent profile at the given index in settings.
#[derive(Clone, Debug, PartialEq, Action)]
#[action(namespace = workspace, no_json)]
pub struct NewAgent(pub usize);

const SESSION_SAVE_DELAY: Duration = Duration::from_millis(500);
/// Commands running at least this long notify when they finish.
const LONG_COMMAND: Duration = Duration::from_secs(10);

enum TabContent {
    Terminal(Entity<PaneGroup>),
    Settings(Entity<SettingsPage>),
}

impl TabContent {
    fn kind(&self) -> TabKind {
        match self {
            TabContent::Terminal(_) => TabKind::Terminal,
            TabContent::Settings(_) => TabKind::Settings,
        }
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            TabContent::Terminal(panes) => panes.focus_handle(cx),
            TabContent::Settings(page) => page.focus_handle(cx),
        }
    }
}

struct OpenTab {
    content: TabContent,
    hidden: bool,
    _subscription: Option<Subscription>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Tab(TabId),
    Group(GroupId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuKind {
    Tab(TabId),
    /// The tabs selected together with Cmd- or Shift-click.
    SelectedTabs,
    Group(GroupId),
    ViewOptions,
    NewTab,
}

pub(crate) struct ContextMenu {
    pub kind: MenuKind,
    pub position: Point<Pixels>,
}

pub(crate) struct Renaming {
    pub target: Target,
    pub input: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

/// What the sidebar and titlebar show for a tab.
pub(crate) struct TabDisplay {
    pub title: SharedString,
    pub icon: &'static str,
    pub color: Option<TabColor>,
    pub pinned: bool,
    pub hidden: bool,
    pub exited: bool,
    pub connected: bool,
    pub unavailable: bool,
    pub directory: Option<SharedString>,
    pub branch: Option<SharedString>,
    /// The last command in the focused pane exited with an error.
    pub failed: bool,
    /// Something happened in this tab while it was in the background.
    pub attention: bool,
    pub agent: Option<AgentState>,
    pub diff: Option<DiffStats>,
}

/// The window contents: a titlebar, a collapsible sidebar of tabs and tab
/// groups, the active tab, and a status bar. The layout is saved as it
/// changes and restored on the next launch.
pub struct Workspace {
    pub(crate) layout: TabLayout,
    open_tabs: HashMap<TabId, OpenTab>,
    pub(crate) active: Option<TabId>,
    pub(crate) sidebar_open: bool,
    pub(crate) sidebar_width: Pixels,
    pub(crate) sidebar_resizing: bool,
    /// Tabs selected together with Cmd- or Shift-click, in sidebar order,
    /// and the tab a Shift-click selects from.
    pub(crate) selected_tabs: Vec<TabId>,
    selection_anchor: Option<TabId>,
    /// The session runtime running is from an older build and cannot be
    /// attached to; a banner offers to restart sessions.
    outdated_runtime: bool,
    /// Sessions are restarting on the current runtime.
    restarting_runtime: bool,
    titlebar_dragging: bool,
    pub(crate) tab_scroll: ScrollHandle,
    pub(crate) context_menu: Option<ContextMenu>,
    /// Where a drag over the sidebar's tab rows would land.
    pub(crate) sidebar_drop: Option<SidebarDrop>,
    pub(crate) renaming: Option<Renaming>,
    pub(crate) tab_search: Entity<TextInput>,
    focus_handle: FocusHandle,
    save_task: Option<Task<()>>,
    /// Background tabs that rang the bell, notified, or finished a command.
    attention: HashSet<TabId>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tab_search = cx.new(|cx| TextInput::new("Search tabs…", cx));
        let subscriptions = vec![
            cx.observe_global::<ActiveTheme>(|_, cx| cx.notify()),
            cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
            cx.subscribe_in(
                &tab_search,
                window,
                |this, input, event, window, cx| match event {
                    TextInputEvent::Changed => cx.notify(),
                    // Enter jumps to the first match; Escape clears the search.
                    TextInputEvent::Confirmed => {
                        if let Some(first) =
                            this.visible_rows(cx).into_iter().find_map(|row| match row {
                                Row::Tab { id, .. } => Some(id),
                                Row::Group(_) => None,
                            })
                        {
                            input.update(cx, |input, cx| input.set_text("", cx));
                            this.activate(first, window, cx);
                        }
                    }
                    TextInputEvent::Cancelled => {
                        input.update(cx, |input, cx| input.set_text("", cx));
                        this.focus_content(window, cx);
                    }
                },
            ),
            cx.on_app_quit(|workspace, cx| {
                workspace.save_session(cx);
                async {}
            }),
        ];
        let mut workspace = Self {
            layout: TabLayout::default(),
            open_tabs: HashMap::new(),
            active: None,
            sidebar_open: true,
            sidebar_width: theme::SIDEBAR_WIDTH,
            sidebar_resizing: false,
            selected_tabs: Vec::new(),
            selection_anchor: None,
            outdated_runtime: false,
            restarting_runtime: false,
            titlebar_dragging: false,
            tab_scroll: ScrollHandle::new(),
            context_menu: None,
            sidebar_drop: None,
            renaming: None,
            tab_search,
            focus_handle: cx.focus_handle(),
            save_task: None,
            attention: HashSet::new(),
            _subscriptions: subscriptions,
        };
        if let Some(state) = session::load() {
            workspace.restore(state, window, cx);
        }
        workspace.save_session(cx);
        workspace.reconcile_sessions(window, cx);
        workspace
    }

    /// Replace an outdated session runtime with the current one. Its
    /// sessions end; panes start again where they were and resume their
    /// agents.
    fn restart_runtime(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.restarting_runtime {
            return;
        }
        self.restarting_runtime = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { runtime::replace_outdated() })
                .await;
            let _ = this.update_in(cx, |workspace, window, cx| {
                workspace.restarting_runtime = false;
                match result {
                    Ok(()) => {
                        workspace.outdated_runtime = false;
                        workspace.reconcile_sessions(window, cx);
                    }
                    Err(error) => log::error!("failed to restart sessions: {error:#}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_runtime_banner(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.outdated_runtime {
            return None;
        }
        let label = if self.restarting_runtime {
            "Restarting…"
        } else {
            "Restart Sessions"
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.elevated_surface)
                .text_sm()
                .child(icon("circle-alert", px(14.), theme.text_accent))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme.text_muted)
                        .child(
                            "Your sessions run on an older version of this app. Restart them to use this one: programs in them stop, and panes reopen where they were with their agents resumed.",
                        ),
                )
                .child(
                    components::button("restart-runtime", Some("refresh-cw"), label, theme)
                        .on_click(cx.listener(|workspace, _, window, cx| {
                            workspace.restart_runtime(window, cx)
                        })),
                )
                .into_any_element(),
        )
    }

    fn reconcile_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let sessions = cx
                .background_executor()
                .spawn(async move { runtime::list_sessions() })
                .await;
            let _ =
                this.update_in(cx, |workspace, window, cx| {
                    match sessions {
                        Ok(sessions) => {
                            let views: Vec<Entity<TerminalView>> = workspace
                                .open_tabs
                                .values()
                                .filter_map(|tab| match &tab.content {
                                    TabContent::Terminal(panes) => Some(panes.read(cx).views()),
                                    TabContent::Settings(_) => None,
                                })
                                .flatten()
                                .collect();
                            let known: HashSet<u64> = views
                                .iter()
                                .map(|view| view.read(cx).session_id())
                                .collect();
                            // Saved panes whose sessions ended with the runtime
                            // (a restart, a reboot) start again where they were.
                            let live: HashSet<u64> =
                                sessions.iter().map(|session| session.id).collect();
                            for view in views {
                                if !live.contains(&view.read(cx).session_id()) {
                                    view.update(cx, |view, cx| view.respawn(None, cx));
                                }
                            }
                            for session in sessions {
                                if known.contains(&session.id) {
                                    continue;
                                }
                                match TerminalView::attach(session.id, cx) {
                                    Ok(view) => {
                                        let show = workspace.open_tabs.is_empty();
                                        let panes = PaneGroup::from_view(view, window, cx);
                                        let id = workspace.add_terminal_tab(
                                            panes,
                                            None,
                                            TabStyle::default(),
                                            window,
                                            cx,
                                        );
                                        if let Some(tab) = workspace.open_tabs.get_mut(&id) {
                                            tab.hidden = !show;
                                        }
                                        if show {
                                            workspace.activate(id, window, cx);
                                        }
                                    }
                                    Err(error) => log::warn!(
                                        "failed to attach session {}: {error:#}",
                                        session.id,
                                    ),
                                }
                            }
                        }
                        Err(error) => {
                            if runtime::is_outdated(&error) {
                                workspace.outdated_runtime = true;
                            }
                            log::warn!("failed to list sessions: {error:#}");
                        }
                    }
                    if workspace.open_tabs.is_empty() {
                        workspace.open_terminal(None, None, None, TabStyle::default(), window, cx);
                    }
                    workspace.focus_content(window, cx);
                    workspace.layout_changed(cx);
                    workspace.save_session(cx);
                });
        })
        .detach();
    }

    fn restore(&mut self, state: SessionState, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = state.sidebar_open;
        self.sidebar_width = clamp_sidebar_width(px(state.sidebar_width));
        let mut restored = Vec::new();
        for entry in state.entries {
            match entry {
                EntryState::Tab(tab) => restored.push(self.restore_tab(tab, None, window, cx)),
                EntryState::Group(group) => {
                    let id = self.layout.add_group(group.name);
                    if let Some(created) = self.layout.group_mut(id) {
                        created.color = group.color;
                        created.collapsed = group.collapsed;
                    }
                    for tab in group.tabs {
                        restored.push(self.restore_tab(tab, Some(id), window, cx));
                    }
                }
            }
        }
        // Drop groups whose tabs all failed to open.
        let empty_groups: Vec<GroupId> = self
            .layout
            .groups()
            .iter()
            .filter(|group| group.tabs().is_empty())
            .map(|group| group.id)
            .collect();
        for group in empty_groups {
            self.layout.ungroup(group);
        }
        let active = restored_tab(state.active, &restored);
        self.active = None;
        if let Some(active) =
            active.filter(|id| self.open_tabs.get(id).is_some_and(|tab| !tab.hidden))
        {
            self.activate(active, window, cx);
        }
    }

    fn restore_tab(
        &mut self,
        tab: TabState,
        group: Option<GroupId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<TabId> {
        match tab.kind {
            TabKind::Terminal => {
                let panes = match &tab.panes {
                    Some(state) => PaneGroup::restore(state, window, cx),
                    None => PaneGroup::build(None, None, window, cx),
                };
                match panes {
                    Ok(panes) => {
                        let id = self.add_terminal_tab(panes, group, tab.style, window, cx);
                        if let Some(open) = self.open_tabs.get_mut(&id) {
                            open.hidden = tab.hidden;
                        }
                        Some(id)
                    }
                    Err(error) => {
                        log::error!("failed to restore tab: {error:#}");
                        None
                    }
                }
            }
            TabKind::Settings => Some(
                self.settings_tab()
                    .unwrap_or_else(|| self.add_settings_tab(tab.style, group, cx)),
            ),
        }
    }

    pub(crate) fn open_terminal(
        &mut self,
        cwd: Option<&Path>,
        startup: Option<&str>,
        group: Option<GroupId>,
        style: TabStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<TabId> {
        match PaneGroup::build(cwd, startup, window, cx) {
            Ok(panes) => {
                let id = self.add_terminal_tab(panes, group, style, window, cx);
                self.activate(id, window, cx);
                self.save_session(cx);
                Some(id)
            }
            Err(error) => {
                log::error!("failed to open terminal: {error:#}");
                None
            }
        }
    }

    fn add_terminal_tab(
        &mut self,
        panes: Entity<PaneGroup>,
        group: Option<GroupId>,
        style: TabStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TabId {
        let id = self.layout.add_tab(style, group);
        let subscription =
            cx.subscribe_in(
                &panes,
                window,
                move |this, _, event, window, cx| match event {
                    PaneGroupEvent::MetadataChanged => {
                        cx.notify();
                        this.schedule_save(cx);
                    }
                    PaneGroupEvent::Hidden(view) => {
                        let group = this.layout.group_of(id);
                        let panes = PaneGroup::from_view(view.clone(), window, cx);
                        let style = TabStyle {
                            worktree: this.layout.style(id).worktree,
                            ..TabStyle::default()
                        };
                        let hidden = this.add_terminal_tab(panes, group, style, window, cx);
                        if let Some(tab) = this.open_tabs.get_mut(&hidden) {
                            tab.hidden = true;
                        }
                        this.layout_changed(cx);
                    }
                    PaneGroupEvent::Attention { source, attention } => {
                        this.handle_attention(id, source, attention, window, cx)
                    }
                    PaneGroupEvent::PaneDropped {
                        dragged,
                        target,
                        edge,
                    } => {
                        this.move_pane_into(id, dragged.clone(), target.clone(), *edge, window, cx)
                    }
                    PaneGroupEvent::TabDropped { tab, target, edge } => {
                        this.merge_tab_into(id, *tab, target.clone(), *edge, window, cx)
                    }
                },
            );
        self.open_tabs.insert(
            id,
            OpenTab {
                content: TabContent::Terminal(panes),
                hidden: false,
                _subscription: Some(subscription),
            },
        );
        id
    }

    fn add_settings_tab(
        &mut self,
        style: TabStyle,
        group: Option<GroupId>,
        cx: &mut Context<Self>,
    ) -> TabId {
        let page = cx.new(SettingsPage::new);
        let id = self.layout.add_tab(style, group);
        self.open_tabs.insert(
            id,
            OpenTab {
                content: TabContent::Settings(page),
                hidden: false,
                _subscription: None,
            },
        );
        id
    }

    fn settings_tab(&self) -> Option<TabId> {
        self.open_tabs
            .iter()
            .find(|(_, tab)| matches!(tab.content, TabContent::Settings(_)))
            .map(|(id, _)| *id)
    }

    pub(crate) fn is_tab_hidden(&self, id: TabId) -> bool {
        self.open_tabs.get(&id).is_some_and(|tab| tab.hidden)
    }

    fn ordered_open_tabs(&self) -> Vec<TabId> {
        self.layout
            .ordered_tabs()
            .into_iter()
            .filter(|id| !self.is_tab_hidden(*id))
            .collect()
    }

    /// A click on a tab in the sidebar: Cmd adds or removes it from the
    /// selection, Shift selects the visible tabs from the last one clicked
    /// to it, and a plain click selects only it and opens it.
    pub(crate) fn click_tab(
        &mut self,
        id: TabId,
        modifiers: Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if modifiers.platform {
            if self.selected_tabs.is_empty()
                && let Some(active) = self.active.filter(|active| *active != id)
            {
                self.selected_tabs.push(active);
            }
            match self.selected_tabs.iter().position(|tab| *tab == id) {
                Some(index) => {
                    self.selected_tabs.remove(index);
                }
                None => self.selected_tabs.push(id),
            }
            self.selection_anchor = Some(id);
            self.order_selection();
            cx.notify();
            return;
        }
        if modifiers.shift {
            let order = self.ordered_open_tabs();
            let anchor = self.selection_anchor.or(self.active).unwrap_or(id);
            if let (Some(from), Some(to)) = (
                order.iter().position(|tab| *tab == anchor),
                order.iter().position(|tab| *tab == id),
            ) {
                self.selected_tabs = order[from.min(to)..=from.max(to)].to_vec();
            }
            cx.notify();
            return;
        }
        self.selected_tabs.clear();
        self.selection_anchor = Some(id);
        self.activate(id, window, cx);
    }

    /// Whether `id` is one of several tabs selected together.
    pub(crate) fn is_tab_selected(&self, id: TabId) -> bool {
        self.selected_tabs.len() > 1 && self.selected_tabs.contains(&id)
    }

    fn order_selection(&mut self) {
        let order = self.layout.ordered_tabs();
        self.selected_tabs
            .sort_by_key(|tab| order.iter().position(|other| other == tab));
    }

    /// Put the selected tabs in a new group, in their sidebar order, and
    /// name it.
    pub(crate) fn group_selected_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tabs = std::mem::take(&mut self.selected_tabs);
        let Some((&first, rest)) = tabs.split_first() else {
            return;
        };
        let name = format!("Group {}", self.layout.groups().len() + 1);
        let group = self.layout.group_tab(first, name);
        for &tab in rest {
            self.layout.move_tab(tab, TabDestination::IntoGroup(group));
        }
        self.close_context_menu(cx);
        self.layout_changed(cx);
        self.start_rename(Target::Group(group), window, cx);
    }

    /// Move the selected tabs into `group`.
    pub(crate) fn move_selected_tabs(&mut self, group: GroupId, cx: &mut Context<Self>) {
        for tab in std::mem::take(&mut self.selected_tabs) {
            self.layout.move_tab(tab, TabDestination::IntoGroup(group));
        }
        self.close_context_menu(cx);
        self.layout_changed(cx);
    }

    /// The menu for several selected tabs.
    pub(crate) fn render_selected_tabs_menu(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = self.selected_tabs.len();
        let groups: Vec<(GroupId, String)> = self
            .layout
            .groups()
            .iter()
            .map(|group| (group.id, group.name.clone()))
            .collect();
        div()
            .flex()
            .flex_col()
            .child(
                components::menu_item(
                    "menu-group-selected",
                    "layers",
                    format!("Group {count} Tabs"),
                    theme,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.group_selected_tabs(window, cx)
                })),
            )
            .children(groups.into_iter().map(|(group, name)| {
                components::menu_item(
                    ("menu-move-selected", group.element_id()),
                    "folder",
                    format!("Move {count} Tabs to {name}"),
                    theme,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.move_selected_tabs(group, cx)
                }))
            }))
            .into_any_element()
    }

    pub(crate) fn activate(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.open_tabs.get_mut(&id) else {
            return;
        };
        tab.hidden = false;
        let focus = tab.content.focus_handle(cx);
        if let Some(group) = self.layout.group_of(id)
            && let Some(group) = self.layout.group_mut(group)
        {
            group.collapsed = false;
        }
        self.active = Some(id);
        self.attention.remove(&id);
        self.acknowledge_tab_attention(id, cx);
        window.focus(&focus, cx);
        if let Some(index) = sidebar_child_index(&self.visible_rows(cx), id) {
            self.tab_scroll.scroll_to_item(index);
        }
        self.layout_changed(cx);
    }

    pub(crate) fn close(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let next = next_tab_after_close(&self.ordered_open_tabs(), id);
        let Some(tab) = self.open_tabs.get_mut(&id) else {
            return;
        };
        if matches!(tab.content, TabContent::Settings(_)) {
            self.remove_tab(id, window, cx);
            return;
        }
        tab.hidden = true;
        if self.active == Some(id) {
            self.active = None;
            match next {
                Some(next) => self.activate(next, window, cx),
                None => window.focus(&self.focus_handle, cx),
            }
        }
        self.layout_changed(cx);
    }

    fn remove_tab(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let next = next_tab_after_close(&self.ordered_open_tabs(), id);
        if self.open_tabs.remove(&id).is_none() {
            return;
        }
        self.layout.remove_tab(id);
        self.attention.remove(&id);
        if self
            .renaming
            .as_ref()
            .is_some_and(|renaming| renaming.target == Target::Tab(id))
        {
            self.renaming = None;
        }

        if self.active == Some(id) {
            self.active = None;
            match next {
                Some(next) => self.activate(next, window, cx),
                None => window.focus(&self.focus_handle, cx),
            }
        }
        self.layout_changed(cx);
    }

    pub(crate) fn close_group(
        &mut self,
        group: GroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tabs: Vec<TabId> = self
            .layout
            .group(group)
            .map(|group| group.tabs().to_vec())
            .unwrap_or_default();
        for tab in tabs {
            self.close(tab, window, cx);
        }
    }

    /// Open a terminal in the active tab's directory, inside `group` if given.
    pub(crate) fn new_tab_in(
        &mut self,
        group: Option<GroupId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cwd = self.active_metadata(cx).and_then(|metadata| metadata.cwd);
        self.open_terminal(cwd.as_deref(), None, group, TabStyle::default(), window, cx);
    }

    /// Open a tab running an agent, next to the active tab.
    pub(crate) fn open_agent(
        &mut self,
        profile: &AgentProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let group = self.active.and_then(|id| self.layout.group_of(id));
        let cwd = self.active_metadata(cx).and_then(|metadata| metadata.cwd);
        let repository = cwd
            .clone()
            .filter(|_| SettingsStore::get(cx).agent_worktrees)
            .filter(|dir| git::repo_root(dir).is_some());
        let Some(repository) = repository else {
            self.launch_agent(profile, cwd.as_deref(), None, group, window, cx);
            return;
        };

        // Give the agent its own worktree so it never edits the same files
        // as another agent; creating one runs off the UI thread.
        let profile = profile.clone();
        cx.spawn_in(window, async move |this, cx| {
            let label = profile.name.clone();
            let created = cx
                .background_executor()
                .spawn(async move { git::create_worktree(&repository, &label) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match created {
                Ok(path) => {
                    this.launch_agent(&profile, Some(&path), Some(path.clone()), group, window, cx);
                }
                Err(error) => {
                    let detail = format!("{error:#}");
                    // Only informs; the answer is not needed.
                    let _acknowledged = window.prompt(
                        PromptLevel::Warning,
                        "Couldn't create a worktree",
                        Some(&detail),
                        &["OK"],
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    fn launch_agent(
        &mut self,
        profile: &AgentProfile,
        cwd: Option<&Path>,
        worktree: Option<PathBuf>,
        group: Option<GroupId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let style = TabStyle {
            name: None,
            color: profile.color,
            icon: profile.icon,
            pinned: false,
            worktree,
        };
        let command = profile.command_line();
        self.open_terminal(cwd, Some(&command), group, style, window, cx);
    }

    /// Stop sessions using a worktree and delete its directory, after asking.
    /// The worktree's branch is kept.
    pub(crate) fn request_remove_worktree(
        &mut self,
        id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.layout.style(id).worktree else {
            return;
        };
        let detail = format!(
            "{}\n\nSessions using this worktree stop and the folder is deleted. Its branch is kept.",
            shorten_home(&path)
        );
        let answer = window.prompt(
            PromptLevel::Warning,
            "Remove this worktree?",
            Some(&detail),
            &["Remove", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let Ok(owned) = this.update_in(cx, |workspace, _, cx| {
                workspace
                    .open_tabs
                    .iter()
                    .filter(|(tab, _)| {
                        workspace.layout.style(**tab).worktree.as_ref() == Some(&path)
                    })
                    .filter_map(|(_, tab)| match &tab.content {
                        TabContent::Terminal(panes) => Some(panes.read(cx).views()),
                        TabContent::Settings(_) => None,
                    })
                    .flatten()
                    .map(|view| view.read(cx).session_id())
                    .collect::<HashSet<_>>()
            }) else {
                return;
            };
            let worktree = path.clone();
            let removed = cx
                .background_executor()
                .spawn(async move {
                    let sessions = runtime::list_sessions()?;
                    for session in sessions_using_worktree(&sessions, &owned, &worktree) {
                        runtime::stop_session(session)?;
                    }
                    git::remove_worktree(&worktree)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match removed {
                Ok(()) => {
                    for tab in this.layout.ordered_tabs() {
                        if this.layout.style(tab).worktree.as_ref() == Some(&path) {
                            this.layout.update_style(tab, |style| style.worktree = None);
                        }
                    }
                    this.layout_changed(cx);
                    this.save_session(cx);
                }
                Err(error) => {
                    let detail = format!("{error:#}");
                    // Only informs; the answer is not needed.
                    let _acknowledged = window.prompt(
                        PromptLevel::Warning,
                        "Couldn't remove the worktree",
                        Some(&detail),
                        &["OK"],
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    pub(crate) fn active_grid_size(&self, cx: &App) -> Option<(u16, u16)> {
        match &self.open_tabs.get(&self.active?)?.content {
            TabContent::Terminal(panes) => Some(panes.read(cx).active_view().read(cx).grid_size()),
            TabContent::Settings(_) => None,
        }
    }

    pub(crate) fn active_metadata(&self, cx: &App) -> Option<TabMetadata> {
        match &self.open_tabs.get(&self.active?)?.content {
            TabContent::Terminal(panes) => Some(panes.read(cx).active_metadata(cx).clone()),
            TabContent::Settings(_) => None,
        }
    }

    pub(crate) fn display(&self, id: TabId, cx: &App) -> Option<TabDisplay> {
        let style = self.layout.style(id);
        let custom_name = style.name.clone().map(SharedString::from);
        let display = match &self.open_tabs.get(&id)?.content {
            TabContent::Terminal(panes) => {
                let metadata = panes.read(cx).active_metadata(cx);
                let views = panes.read(cx).views();
                TabDisplay {
                    title: custom_name.unwrap_or_else(|| metadata.title.clone()),
                    icon: style.icon.unwrap_or(TabIcon::Terminal).asset(),
                    color: style.color,
                    pinned: style.pinned,
                    hidden: self.open_tabs.get(&id)?.hidden,
                    exited: views.iter().all(|view| view.read(cx).is_exited()),
                    connected: views.iter().all(|view| view.read(cx).is_connected()),
                    unavailable: views.iter().all(|view| !view.read(cx).is_connected()),
                    directory: metadata.directory.clone(),
                    branch: metadata.branch.clone(),
                    attention: self.attention.contains(&id),
                    agent: metadata.agent,
                    diff: metadata.diff,
                    failed: metadata.command_started.is_none()
                        && metadata
                            .last_command
                            .is_some_and(|outcome| outcome.failed()),
                }
            }
            TabContent::Settings(_) => TabDisplay {
                title: custom_name.unwrap_or_else(|| "Settings".into()),
                icon: style.icon.map(TabIcon::asset).unwrap_or("settings"),
                color: style.color,
                pinned: style.pinned,
                hidden: false,
                exited: false,
                connected: true,
                unavailable: false,
                directory: None,
                branch: None,
                failed: false,
                attention: false,
                agent: None,
                diff: None,
            },
        };
        Some(display)
    }

    pub(crate) fn panes_of(&self, tab: TabId) -> Option<Entity<PaneGroup>> {
        match &self.open_tabs.get(&tab)?.content {
            TabContent::Terminal(panes) => Some(panes.clone()),
            TabContent::Settings(_) => None,
        }
    }

    pub(crate) fn tab_holding(&self, panes: &WeakEntity<PaneGroup>) -> Option<TabId> {
        self.open_tabs
            .iter()
            .find_map(|(id, tab)| match &tab.content {
                TabContent::Terminal(group) if group.entity_id() == panes.entity_id() => Some(*id),
                _ => None,
            })
    }

    /// Take a dragged pane out of its tab. Returns the pane's tab when the
    /// pane was that tab's only one (the tab is left in place).
    fn lift_pane(
        &mut self,
        dragged: &DraggedPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Result<(), TabId>> {
        let source = dragged.source.upgrade()?;
        let source_tab = self.tab_holding(&dragged.source)?;
        let detached = source.update(cx, |group, cx| group.detach(&dragged.view, window, cx));
        Some(match detached {
            Detached::Removed => Ok(()),
            Detached::WasLast => Err(source_tab),
        })
    }

    /// Move a pane dragged from another tab next to `target` in `tab`.
    pub(crate) fn move_pane_into(
        &mut self,
        tab: TabId,
        dragged: DraggedPane,
        target: Entity<TerminalView>,
        edge: Edge,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(destination) = self.panes_of(tab) else {
            return;
        };
        match self.lift_pane(&dragged, window, cx) {
            None => return,
            // The pane was its tab's only one: the tab goes away, its
            // terminal lives on in the destination.
            Some(Err(source_tab)) => self.remove_tab(source_tab, window, cx),
            Some(Ok(())) => {}
        }
        destination.update(cx, |group, cx| {
            group.adopt(PaneNode::Leaf(dragged.view), &target, edge, window, cx)
        });
        self.activate(tab, window, cx);
    }

    /// Merge every pane of `source_tab` into `tab`, next to `target`.
    pub(crate) fn merge_tab_into(
        &mut self,
        tab: TabId,
        source_tab: TabId,
        target: Entity<TerminalView>,
        edge: Edge,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if tab == source_tab {
            return;
        }
        let (Some(destination), Some(source)) = (self.panes_of(tab), self.panes_of(source_tab))
        else {
            return;
        };
        let panes = source.read(cx).root();
        self.remove_tab(source_tab, window, cx);
        destination.update(cx, |group, cx| {
            group.adopt(panes, &target, edge, window, cx)
        });
        self.activate(tab, window, cx);
    }

    /// Turn a dragged pane into a tab of its own at `destination` in the
    /// sidebar.
    pub(crate) fn pane_to_tab(
        &mut self,
        dragged: &DraggedPane,
        destination: TabDestination,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let worktree = self
            .tab_holding(&dragged.source)
            .and_then(|tab| self.layout.style(tab).worktree);
        let id = match self.lift_pane(dragged, window, cx) {
            None => return,
            // Already a tab of its own: just move it.
            Some(Err(source_tab)) => source_tab,
            Some(Ok(())) => {
                let panes = PaneGroup::from_view(dragged.view.clone(), window, cx);
                self.add_terminal_tab(
                    panes,
                    None,
                    TabStyle {
                        worktree,
                        ..TabStyle::default()
                    },
                    window,
                    cx,
                )
            }
        };
        self.layout.move_tab(id, destination);
        self.activate(id, window, cx);
    }

    /// Activate the tab whose element id is `element_id`, as carried by a
    /// notification.
    pub(crate) fn activate_by_element_id(
        &mut self,
        element_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self
            .layout
            .ordered_tabs()
            .into_iter()
            .find(|id| id.element_id() == element_id)
        {
            self.activate(id, window, cx);
        }
    }

    fn acknowledge_tab_attention(&self, id: TabId, cx: &mut Context<Self>) {
        if let Some(panes) = self.panes_of(id) {
            for view in panes.read(cx).views() {
                view.update(cx, |view, cx| view.acknowledge_attention(cx));
            }
        }
    }

    fn handle_attention(
        &mut self,
        id: TabId,
        source: &Entity<TerminalView>,
        attention: &Attention,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let window_active = window.is_window_active();
        if window_active && self.active == Some(id) {
            source.update(cx, |view, cx| view.acknowledge_attention(cx));
            return;
        }
        // Quick commands finishing are routine; only long ones are news.
        if let Attention::CommandFinished(outcome) = attention
            && outcome.duration < LONG_COMMAND
        {
            source.update(cx, |view, cx| view.acknowledge_attention(cx));
            return;
        }
        self.attention.insert(id);
        cx.notify();

        if window_active || !SettingsStore::get(cx).notifications {
            return;
        }
        let tab_title = self
            .display(id, cx)
            .map(|display| display.title.to_string())
            .unwrap_or_default();
        let (title, body) = match attention {
            Attention::Bell => return,
            // Programs choose this text, so the title always names the tab
            // it came from and their own title is shown as quoted content.
            Attention::Notification { title, body } => match title {
                Some(title) => (tab_title, format!("“{title}”: {body}")),
                None => (tab_title, body.clone()),
            },
            Attention::AgentWaiting { agent, needs_input } => {
                let body = if *needs_input {
                    format!("{} needs your input", agent.name())
                } else {
                    format!("{} is done", agent.name())
                };
                (tab_title, body)
            }
            Attention::CommandFinished(outcome) => {
                let duration = format_duration(outcome.duration);
                let body = match outcome.exit_code {
                    Some(code) if code != 0 => {
                        format!("Failed with exit code {code} after {duration}")
                    }
                    _ => format!("Finished after {duration}"),
                };
                (tab_title, body)
            }
        };
        notifications::show(id, title, body, cx);
    }

    pub(crate) fn update_style(
        &mut self,
        id: TabId,
        change: impl FnOnce(&mut TabStyle),
        cx: &mut Context<Self>,
    ) {
        self.layout.update_style(id, change);
        self.layout_changed(cx);
    }

    /// Persist and redraw after the arrangement of tabs changed.
    pub(crate) fn layout_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_save(cx);
        cx.notify();
    }

    pub(crate) fn new_group_with(
        &mut self,
        tab: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = format!("Group {}", self.layout.groups().len() + 1);
        let group = self.layout.group_tab(tab, name);
        if self.is_tab_hidden(tab) {
            self.activate(tab, window, cx);
        }
        self.layout_changed(cx);
        self.start_rename(Target::Group(group), window, cx);
    }

    pub(crate) fn open_context_menu(
        &mut self,
        kind: MenuKind,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.context_menu = Some(ContextMenu { kind, position });
        cx.notify();
    }

    pub(crate) fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            cx.notify();
        }
    }

    pub(crate) fn start_rename(
        &mut self,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = match target {
            Target::Tab(id) => self
                .display(id, cx)
                .map(|display| display.title.to_string()),
            Target::Group(id) => self.layout.group(id).map(|group| group.name.clone()),
        };
        let Some(current) = current else {
            return;
        };
        let input = cx.new(|cx| {
            let mut input = TextInput::new("Name", cx);
            input.set_text(current, cx);
            input
        });
        let input_focus = input.focus_handle(cx);
        let subscriptions = vec![
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &TextInputEvent, window, cx| match event {
                    TextInputEvent::Confirmed => this.finish_rename(true, window, cx),
                    TextInputEvent::Cancelled => this.finish_rename(false, window, cx),
                    TextInputEvent::Changed => {}
                },
            ),
            cx.on_blur(&input_focus, window, |this, window, cx| {
                this.finish_rename(true, window, cx);
            }),
        ];
        window.focus(&input_focus, cx);
        self.renaming = Some(Renaming {
            target,
            input,
            _subscriptions: subscriptions,
        });
        self.context_menu = None;
        cx.notify();
    }

    fn finish_rename(&mut self, apply: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(renaming) = self.renaming.take() else {
            return;
        };
        if apply {
            let name = renaming.input.read(cx).text().trim().to_string();
            match renaming.target {
                Target::Tab(id) => {
                    // An empty name falls back to the program's title.
                    let name = (!name.is_empty()).then_some(name);
                    self.layout.update_style(id, |style| style.name = name);
                }
                Target::Group(id) => {
                    if !name.is_empty()
                        && let Some(group) = self.layout.group_mut(id)
                    {
                        group.name = name;
                    }
                }
            }
        }
        self.focus_content(window, cx);
        self.layout_changed(cx);
    }

    /// Focus the active tab, or the workspace itself when no tab is open.
    pub(crate) fn focus_content(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.active.and_then(|id| self.open_tabs.get(&id)) {
            Some(tab) => window.focus(&tab.content.focus_handle(cx), cx),
            None => window.focus(&self.focus_handle, cx),
        }
    }

    pub(crate) fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save_task = Some(
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                cx.background_executor().timer(SESSION_SAVE_DELAY).await;
                let _ = this.update(cx, |workspace, cx| workspace.save_session(cx));
            }),
        );
    }

    fn save_session(&self, cx: &App) {
        if let Err(error) = session::save(&self.snapshot(cx)) {
            log::warn!("failed to save session: {error:#}");
        }
    }

    fn snapshot(&self, cx: &App) -> SessionState {
        let entries = self
            .layout
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                Entry::Tab(id) => self.tab_state(*id, cx).map(EntryState::Tab),
                Entry::Group(id) => self.layout.group(*id).map(|group| {
                    EntryState::Group(GroupState {
                        name: group.name.clone(),
                        color: group.color,
                        collapsed: group.collapsed,
                        tabs: group
                            .tabs()
                            .iter()
                            .filter_map(|id| self.tab_state(*id, cx))
                            .collect(),
                    })
                }),
            })
            .collect();
        let active = self.active.and_then(|active| {
            self.layout
                .ordered_tabs()
                .iter()
                .position(|id| *id == active)
        });
        SessionState {
            version: session::SESSION_VERSION,
            entries,
            active,
            sidebar_open: self.sidebar_open,
            sidebar_width: f32::from(self.sidebar_width),
        }
    }

    fn tab_state(&self, id: TabId, cx: &App) -> Option<TabState> {
        let tab = self.open_tabs.get(&id)?;
        let panes = match &tab.content {
            TabContent::Terminal(panes) => Some(panes.read(cx).snapshot(cx)),
            TabContent::Settings(_) => None,
        };
        Some(TabState {
            kind: tab.content.kind(),
            hidden: tab.hidden,
            panes,
            style: self.layout.style(id),
        })
    }

    fn new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        let group = self.active.and_then(|id| self.layout.group_of(id));
        self.new_tab_in(group, window, cx);
    }

    fn new_agent(&mut self, action: &NewAgent, window: &mut Window, cx: &mut Context<Self>) {
        let profile = SettingsStore::get(cx).agent_profiles.get(action.0).cloned();
        if let Some(profile) = profile {
            self.open_agent(&profile, window, cx);
        }
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active
            && !self.layout.style(id).pinned
        {
            self.request_close(id, window, cx);
        }
    }

    /// Programs other than the shell running in any of `tabs`.
    fn running_in(&self, tabs: &[TabId], cx: &App) -> Vec<SharedString> {
        tabs.iter()
            .filter_map(|id| match &self.open_tabs.get(id)?.content {
                TabContent::Terminal(panes) => Some(panes.read(cx).running_programs(cx)),
                TabContent::Settings(_) => None,
            })
            .flatten()
            .collect()
    }

    pub(crate) fn request_stop(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panes) = self.panes_of(id) else {
            return;
        };
        let running = self.running_in(&[id], cx);
        confirm_close(
            running,
            "Stop these sessions?",
            window,
            cx,
            move |_, _, cx| {
                for view in panes.read(cx).views() {
                    if !view.read(cx).is_exited() {
                        view.update(cx, |view, cx| view.stop(cx));
                    }
                }
            },
        );
    }

    fn stop_tab(&mut self, _: &StopTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active {
            self.request_stop(id, window, cx);
        }
    }

    pub(crate) fn archive(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panes) = self.panes_of(id) else {
            return;
        };
        let views = panes.read(cx).views();
        if views.iter().any(|view| !view.read(cx).is_exited()) {
            return;
        }
        let sessions: Vec<u64> = views
            .iter()
            .map(|view| view.read(cx).session_id())
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let available: HashMap<u64, SessionInfo> = runtime::list_sessions()?
                        .into_iter()
                        .map(|session| (session.id, session))
                        .collect();
                    for session in sessions {
                        if let Some(info) = available.get(&session) {
                            anyhow::ensure!(info.exited, "stop the session before removing it");
                            runtime::remove_session(session)?;
                        }
                    }
                    anyhow::Ok(())
                })
                .await;
            let _ = this.update_in(cx, |workspace, window, cx| match result {
                Ok(()) => {
                    workspace.remove_tab(id, window, cx);
                    workspace.save_session(cx);
                }
                Err(error) => {
                    let detail = format!("{error:#}");
                    let _acknowledged = window.prompt(
                        PromptLevel::Warning,
                        "Couldn't remove the sessions",
                        Some(&detail),
                        &["OK"],
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    pub(crate) fn request_unavailable_action(
        &mut self,
        id: TabId,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self
            .display(id, cx)
            .is_some_and(|display| display.unavailable)
        {
            return;
        }
        let (question, detail, button) = if replace {
            (
                "Replace unavailable views with a fresh shell?",
                "A fresh shell opens in the last known directory. Running sessions are recovered when you next open the app.",
                "Start Fresh Shell",
            )
        } else {
            (
                "Remove unavailable views?",
                "These views are removed from the workspace. Running sessions are recovered when you next open the app.",
                "Remove Views",
            )
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            question,
            Some(detail),
            &[button, "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update_in(cx, |workspace, window, cx| {
                if !workspace
                    .display(id, cx)
                    .is_some_and(|display| display.unavailable)
                {
                    return;
                }
                if replace {
                    let cwd = workspace
                        .panes_of(id)
                        .and_then(|panes| panes.read(cx).active_metadata(cx).cwd.clone());
                    let group = workspace.layout.group_of(id);
                    let style = workspace.layout.style(id);
                    let Some(replacement) =
                        workspace.open_terminal(cwd.as_deref(), None, group, style, window, cx)
                    else {
                        let _acknowledged = window.prompt(
                            PromptLevel::Warning,
                            "Couldn't start a fresh shell",
                            Some("The unavailable session views are still in the workspace."),
                            &["OK"],
                            cx,
                        );
                        return;
                    };
                    workspace
                        .layout
                        .move_tab(replacement, TabDestination::Before(id));
                }
                workspace.remove_tab(id, window, cx);
                workspace.save_session(cx);
            });
        })
        .detach();
    }

    pub(crate) fn needs_attention(&self, cx: &App) -> Vec<TabId> {
        self.layout
            .ordered_tabs()
            .into_iter()
            .filter(|id| {
                self.attention.contains(id)
                    || self.panes_of(*id).is_some_and(|panes| {
                        panes.read(cx).views().iter().any(|view| {
                            view.read(cx)
                                .metadata()
                                .agent
                                .is_some_and(|agent| agent.status == AgentStatus::NeedsInput)
                        })
                    })
            })
            .collect()
    }

    pub(crate) fn activate_attention(
        &mut self,
        id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate(id, window, cx);
        if let Some(panes) = self.panes_of(id) {
            let waiting = panes.read(cx).views().into_iter().find(|view| {
                view.read(cx)
                    .metadata()
                    .agent
                    .is_some_and(|agent| agent.status == AgentStatus::NeedsInput)
            });
            if let Some(view) = waiting {
                window.focus(&view.focus_handle(cx), cx);
            }
        }
    }

    fn next_attention(&mut self, _: &NextAttention, window: &mut Window, cx: &mut Context<Self>) {
        let tabs = self.needs_attention(cx);
        let next = self
            .active
            .and_then(|active| {
                tabs.iter()
                    .position(|id| *id == active)
                    .map(|index| (index + 1) % tabs.len())
            })
            .unwrap_or(0);
        if let Some(&id) = tabs.get(next) {
            self.activate_attention(id, window, cx);
        }
    }

    /// Hide a terminal tab without stopping its sessions.
    pub(crate) fn request_close(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        self.close(id, window, cx);
    }

    pub(crate) fn request_close_group(
        &mut self,
        group: GroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_group(group, window, cx);
    }

    fn quit(&mut self, _: &Quit, _: &mut Window, cx: &mut Context<Self>) {
        self.save_session(cx);
        cx.quit();
    }

    /// Save the arrangement before disconnecting the window from its sessions.
    pub fn should_close_window(&mut self, _: &mut Window, cx: &mut Context<Self>) -> bool {
        self.save_session(cx);
        true
    }

    fn close_window(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        if self.should_close_window(window, cx) {
            window.remove_window();
        }
    }

    fn step_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let ordered = self.ordered_open_tabs();
        if ordered.is_empty() {
            return;
        }
        let current = self
            .active
            .and_then(|id| ordered.iter().position(|tab| *tab == id))
            .unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(ordered.len() as isize) as usize;
        self.activate(ordered[next], window, cx);
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.step_tab(1, window, cx);
    }

    fn previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        self.step_tab(-1, window, cx);
    }

    fn activate_tab(&mut self, action: &ActivateTab, window: &mut Window, cx: &mut Context<Self>) {
        let ordered = self.ordered_open_tabs();
        if let Some(&id) = ordered.get(action.0.min(ordered.len().saturating_sub(1))) {
            self.activate(id, window, cx);
        }
    }

    pub(crate) fn toggle_sidebar(
        &mut self,
        _: &ToggleSidebar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_open = !self.sidebar_open;
        self.focus_content(window, cx);
        self.layout_changed(cx);
    }

    /// Follows the pointer while the sidebar's edge is being dragged. The
    /// sidebar starts at the window's left edge, so its width is the
    /// pointer's horizontal position.
    fn on_sidebar_resize(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.pressed_button != Some(MouseButton::Left) {
            self.finish_sidebar_resize(cx);
            return;
        }
        self.sidebar_width = clamp_sidebar_width(event.position.x);
        cx.notify();
    }

    pub(crate) fn finish_sidebar_resize(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_resizing {
            self.sidebar_resizing = false;
            self.layout_changed(cx);
        }
    }

    pub(crate) fn open_settings(
        &mut self,
        _: &OpenSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = match self.settings_tab() {
            Some(id) => id,
            None => self.add_settings_tab(TabStyle::default(), None, cx),
        };
        self.activate(id, window, cx);
    }

    fn rename_tab(&mut self, _: &RenameTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active {
            self.start_rename(Target::Tab(id), window, cx);
        }
    }

    #[tracing::instrument(skip_all)]
    fn render_titlebar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let display = self.active.and_then(|id| self.display(id, cx));
        // The toggle lives in the status bar by default; when the status bar
        // can't show it, it moves here so the sidebar stays reachable.
        let status_bar = &SettingsStore::get(cx).status_bar;
        let toggle_in_titlebar =
            !status_bar.visible || status_bar.side_of(StatusItem::SidebarToggle).is_none();

        div()
            .id("titlebar")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(2.))
            .h(theme::TITLEBAR_HEIGHT)
            .w_full()
            .pl(theme::TRAFFIC_LIGHT_PADDING)
            .pr(px(4.))
            .bg(theme.title_bar)
            .border_b_1()
            .border_color(theme.border)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_dragging = true),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_dragging = false),
            )
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.titlebar_dragging = false))
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if this.titlebar_dragging {
                    this.titlebar_dragging = false;
                    window.start_window_move();
                }
            }))
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    window.titlebar_double_click();
                }
            })
            .when(toggle_in_titlebar, |titlebar| {
                titlebar.child(
                    icon_button("titlebar-sidebar-toggle", "panel-left", theme)
                        .size(px(26.))
                        .rounded(px(6.))
                        .when(self.sidebar_open, |button| button.bg(theme.ghost_selected))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.toggle_sidebar(&ToggleSidebar, window, cx);
                        })),
                )
            })
            .children(display.map(|display| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .min_w_0()
                    .child(titlebar_label(display.title, theme.text))
                    .children(
                        display
                            .directory
                            .map(|directory| titlebar_label(directory, theme.text_muted)),
                    )
            }))
    }

    fn render_empty_state(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let hover = theme.ghost_hover;
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .gap(px(12.))
            .bg(theme.terminal_background())
            .child(icon("terminal", px(32.), theme.text_placeholder))
            .child(
                div()
                    .text_size(theme::TEXT_DEFAULT)
                    .text_color(theme.text_muted)
                    .child("No open terminals"),
            )
            .child(
                div()
                    .id("empty-new-tab")
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(28.))
                    .px(px(10.))
                    .rounded(theme::RADIUS_SM)
                    .bg(theme.element_background)
                    .border_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .text_size(theme::TEXT_DEFAULT)
                    .text_color(theme.text)
                    .hover(move |style| style.bg(hover))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.new_tab_in(None, window, cx);
                    }))
                    .child("New Terminal")
                    .child(keybinding("⌘T", theme)),
            )
    }
}

fn titlebar_label(text: SharedString, color: gpui::Hsla) -> impl IntoElement {
    div()
        .min_w_0()
        .px(px(6.))
        .truncate()
        .text_size(theme::TEXT_SMALL)
        .text_color(color)
        .child(text)
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    #[tracing::instrument(name = "Workspace::render", skip_all)]
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let content: AnyElement = match self.active.and_then(|id| self.open_tabs.get(&id)) {
            Some(OpenTab {
                content: TabContent::Terminal(panes),
                ..
            }) => panes.clone().into_any_element(),
            Some(OpenTab {
                content: TabContent::Settings(page),
                ..
            }) => page.clone().into_any_element(),
            None => self.render_empty_state(&theme, cx).into_any_element(),
        };

        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.terminal_background())
            .text_color(theme.text)
            .font_family(theme::UI_FONT_FAMILY)
            .on_action(cx.listener(Self::new_tab))
            .on_action(cx.listener(Self::new_agent))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::stop_tab))
            .on_action(cx.listener(Self::next_attention))
            .on_action(cx.listener(Self::close_window))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::activate_tab))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::rename_tab))
            .on_action(cx.listener(Self::quit))
            .when(self.sidebar_resizing, |root| {
                root.cursor(CursorStyle::ResizeLeftRight)
                    .on_mouse_move(cx.listener(Self::on_sidebar_resize))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.finish_sidebar_resize(cx)),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.finish_sidebar_resize(cx)),
                    )
            })
            .child(self.render_titlebar(&theme, cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .when(self.sidebar_open, |row| {
                        row.child(self.render_sidebar(&theme, cx))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .children(self.render_runtime_banner(&theme, cx))
                            .child(div().flex_1().min_h_0().child(content)),
                    ),
            )
            .children(self.render_status_bar(&theme, cx))
            .children(self.render_context_menu(&theme, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_selects_the_next_open_tab_or_the_previous_at_the_end() {
        let mut layout = TabLayout::default();
        let first = layout.add_tab(TabStyle::default(), None);
        let closed = layout.add_tab(TabStyle::default(), None);
        let middle = layout.add_tab(TabStyle::default(), None);
        let last = layout.add_tab(TabStyle::default(), None);
        let open = [first, middle, last];
        assert_eq!(next_tab_after_close(&open, first), Some(middle));
        assert_eq!(next_tab_after_close(&open, middle), Some(last));
        assert_eq!(next_tab_after_close(&open, last), Some(middle));
        assert_eq!(next_tab_after_close(&open, closed), None);
        assert_eq!(next_tab_after_close(&[last], last), None);
        assert_eq!(next_tab_after_close(&[], last), None);
    }

    #[test]
    fn restoring_selection_keeps_saved_slots_when_tabs_are_filtered() {
        let mut layout = TabLayout::default();
        let settings = layout.add_tab(TabStyle::default(), None);
        let terminal = layout.add_tab(TabStyle::default(), None);
        let restored = [Some(settings), None, Some(settings), Some(terminal)];
        assert_eq!(restored_tab(Some(3), &restored), Some(terminal));
        assert_eq!(restored_tab(Some(2), &restored), Some(settings));
        assert_eq!(restored_tab(Some(1), &restored), None);
        assert_eq!(restored_tab(None, &restored), None);
        assert_eq!(restored_tab(Some(9), &restored), None);
    }

    #[test]
    fn worktree_removal_stops_only_live_associated_sessions() {
        let session = |id, cwd: &str, exited| SessionInfo {
            id,
            cwd: Some(PathBuf::from(cwd)),
            exited,
            ..Default::default()
        };
        let sessions = [
            session(1, "/tmp/worktree", false),
            session(2, "/tmp/worktree/src", false),
            session(3, "/tmp/worktree-other", false),
            session(4, "/tmp/elsewhere", false),
            session(5, "/tmp/worktree", true),
        ];
        let owned = HashSet::from([4, 5, 999]);
        assert_eq!(
            sessions_using_worktree(&sessions, &owned, Path::new("/tmp/worktree")),
            vec![1, 2, 4],
        );
    }
}
