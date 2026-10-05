use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::Duration,
};

use gpui::{
    Action, AnyElement, App, AsyncApp, ClickEvent, Context, Entity, FocusHandle, Focusable,
    MouseButton, Pixels, Point, ScrollHandle, SharedString, Subscription, Task, WeakEntity, Window,
    actions, div, prelude::*, px,
};

use crate::{
    components::{icon, icon_button, keybinding},
    confirm::confirm_close,
    notifications,
    pane_group::{PaneGroup, PaneGroupEvent},
    session::{self, EntryState, GroupState, SessionState, TabKind, TabState},
    settings::SettingsStore,
    settings_page::SettingsPage,
    sidebar::sidebar_child_index,
    status_bar::{StatusItem, format_duration},
    tabs::{Entry, GroupId, Row, TabColor, TabIcon, TabId, TabLayout, TabStyle},
    terminal_view::{AgentState, Attention, TabMetadata},
    text_input::{TextInput, TextInputEvent},
    theme::{self, ActiveTheme, ActiveThemeExt, Theme},
};

actions!(
    workspace,
    [
        NewTab,
        CloseTab,
        NextTab,
        PreviousTab,
        ToggleSidebar,
        OpenSettings,
        RenameTab,
        Quit
    ]
);

/// Activate the tab at the given index; `usize::MAX` selects the last tab.
#[derive(Clone, Debug, PartialEq, Action)]
#[action(namespace = workspace, no_json)]
pub struct ActivateTab(pub usize);

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
    Group(GroupId),
    ViewOptions,
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
    pub directory: Option<SharedString>,
    pub branch: Option<SharedString>,
    /// The last command in the focused pane exited with an error.
    pub failed: bool,
    /// Something happened in this tab while it was in the background.
    pub attention: bool,
    pub agent: Option<AgentState>,
}

/// The window contents: a titlebar, a collapsible sidebar of tabs and tab
/// groups, the active tab, and a status bar. The layout is saved as it
/// changes and restored on the next launch.
pub struct Workspace {
    pub(crate) layout: TabLayout,
    open_tabs: HashMap<TabId, OpenTab>,
    pub(crate) active: Option<TabId>,
    pub(crate) sidebar_open: bool,
    titlebar_dragging: bool,
    pub(crate) tab_scroll: ScrollHandle,
    pub(crate) context_menu: Option<ContextMenu>,
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
            titlebar_dragging: false,
            tab_scroll: ScrollHandle::new(),
            context_menu: None,
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
        if workspace.open_tabs.is_empty() {
            workspace.open_terminal(None, None, TabStyle::default(), window, cx);
        }
        workspace
    }

    fn restore(&mut self, state: SessionState, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = state.sidebar_open;
        for entry in state.entries {
            match entry {
                EntryState::Tab(tab) => self.restore_tab(tab, None, window, cx),
                EntryState::Group(group) => {
                    let id = self.layout.add_group(group.name);
                    if let Some(created) = self.layout.group_mut(id) {
                        created.color = group.color;
                        created.collapsed = group.collapsed;
                    }
                    for tab in group.tabs {
                        self.restore_tab(tab, Some(id), window, cx);
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
        let ordered = self.layout.ordered_tabs();
        if let Some(&active) = ordered.get(state.active).or(ordered.first()) {
            self.activate(active, window, cx);
        }
    }

    fn restore_tab(
        &mut self,
        tab: TabState,
        group: Option<GroupId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match tab.kind {
            TabKind::Terminal => {
                let panes = match &tab.panes {
                    Some(state) => PaneGroup::restore(state, window, cx),
                    None => PaneGroup::build(None, window, cx),
                };
                match panes {
                    Ok(panes) => {
                        self.add_terminal_tab(panes, group, tab.style, window, cx);
                    }
                    Err(error) => log::error!("failed to restore tab: {error:#}"),
                }
            }
            TabKind::Settings => {
                if self.settings_tab().is_none() {
                    self.add_settings_tab(tab.style, group, cx);
                }
            }
        }
    }

    fn open_terminal(
        &mut self,
        cwd: Option<&Path>,
        group: Option<GroupId>,
        style: TabStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<TabId> {
        match PaneGroup::build(cwd, window, cx) {
            Ok(panes) => Some(self.add_terminal_tab(panes, group, style, window, cx)),
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
                    PaneGroupEvent::Exited => this.close(id, window, cx),
                    PaneGroupEvent::Attention(attention) => {
                        this.handle_attention(id, attention, window, cx)
                    }
                },
            );
        self.open_tabs.insert(
            id,
            OpenTab {
                content: TabContent::Terminal(panes),
                _subscription: Some(subscription),
            },
        );
        self.activate(id, window, cx);
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

    pub(crate) fn activate(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.open_tabs.get(&id) else {
            return;
        };
        let focus = tab.content.focus_handle(cx);
        if let Some(group) = self.layout.group_of(id)
            && let Some(group) = self.layout.group_mut(group)
        {
            group.collapsed = false;
        }
        self.active = Some(id);
        self.attention.remove(&id);
        window.focus(&focus, cx);
        if let Some(index) = sidebar_child_index(&self.visible_rows(cx), id) {
            self.tab_scroll.scroll_to_item(index);
        }
        self.layout_changed(cx);
    }

    pub(crate) fn close(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_tabs.remove(&id).is_none() {
            return;
        }
        let position = self.layout.ordered_tabs().iter().position(|tab| *tab == id);
        self.layout.remove_tab(id);
        if self
            .renaming
            .as_ref()
            .is_some_and(|renaming| renaming.target == Target::Tab(id))
        {
            self.renaming = None;
        }

        if self.active == Some(id) {
            self.active = None;
            let remaining = self.layout.ordered_tabs();
            let next = position
                .and_then(|position| remaining.get(position.min(remaining.len().saturating_sub(1))))
                .copied();
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
        self.open_terminal(cwd.as_deref(), group, TabStyle::default(), window, cx);
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
                TabDisplay {
                    title: custom_name.unwrap_or_else(|| metadata.title.clone()),
                    icon: style.icon.unwrap_or(TabIcon::Terminal).asset(),
                    color: style.color,
                    pinned: style.pinned,
                    directory: metadata.directory.clone(),
                    branch: metadata.branch.clone(),
                    attention: self.attention.contains(&id),
                    agent: metadata.agent,
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
                directory: None,
                branch: None,
                failed: false,
                attention: false,
                agent: None,
            },
        };
        Some(display)
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

    fn handle_attention(
        &mut self,
        id: TabId,
        attention: &Attention,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let window_active = window.is_window_active();
        if window_active && self.active == Some(id) {
            return;
        }
        // Quick commands finishing are routine; only long ones are news.
        if let Attention::CommandFinished(outcome) = attention
            && outcome.duration < LONG_COMMAND
        {
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

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
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
        let active = self
            .active
            .and_then(|active| {
                self.layout
                    .ordered_tabs()
                    .iter()
                    .position(|id| *id == active)
            })
            .unwrap_or(0);
        SessionState {
            entries,
            active,
            sidebar_open: self.sidebar_open,
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
            panes,
            style: self.layout.style(id),
        })
    }

    fn new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        let group = self.active.and_then(|id| self.layout.group_of(id));
        self.new_tab_in(group, window, cx);
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

    /// Close a tab the user asked to close, confirming if it is busy.
    pub(crate) fn request_close(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let running = self.running_in(&[id], cx);
        confirm_close(
            running,
            "Close this tab?",
            window,
            cx,
            move |this, window, cx| {
                this.close(id, window, cx);
            },
        );
    }

    pub(crate) fn request_close_group(
        &mut self,
        group: GroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tabs = self
            .layout
            .group(group)
            .map(|group| group.tabs().to_vec())
            .unwrap_or_default();
        let running = self.running_in(&tabs, cx);
        confirm_close(
            running,
            "Close this group?",
            window,
            cx,
            move |this, window, cx| {
                this.close_group(group, window, cx);
            },
        );
    }

    fn quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        let running = self.running_in(&self.layout.ordered_tabs(), cx);
        confirm_close(running, "Quit?", window, cx, |_, _, cx| cx.quit());
    }

    /// Whether the window may close right away. When programs are running
    /// this asks first and closes the window itself if the user agrees.
    pub fn should_close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let running = self.running_in(&self.layout.ordered_tabs(), cx);
        if running.is_empty() || !SettingsStore::get(cx).confirm_close {
            return true;
        }
        confirm_close(running, "Close this window?", window, cx, |_, window, _| {
            window.remove_window();
        });
        false
    }

    fn step_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let ordered = self.layout.ordered_tabs();
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
        let ordered = self.layout.ordered_tabs();
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
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::activate_tab))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::rename_tab))
            .on_action(cx.listener(Self::quit))
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
                    .child(div().flex_1().min_w_0().h_full().child(content)),
            )
            .children(self.render_status_bar(&theme, cx))
            .children(self.render_context_menu(&theme, cx))
    }
}
