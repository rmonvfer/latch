//! The arrangement of tabs in the sidebar: their order, groups, and the
//! per-tab customizations (name, color, icon, pin).
//!
//! Pinned tabs always sit at the start of their container (the top level or
//! a group); every mutation restores that ordering.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GroupId(u64);

impl TabId {
    /// A number for building UI element ids.
    pub fn element_id(self) -> usize {
        self.0 as usize
    }
}

impl GroupId {
    /// A number for building UI element ids.
    pub fn element_id(self) -> usize {
        self.0 as usize
    }
}

/// Accent colors for tabs and groups, drawn from the active theme's ANSI
/// palette so they suit every theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabColor {
    Red,
    Yellow,
    Green,
    Cyan,
    Blue,
    Magenta,
}

impl TabColor {
    pub const ALL: [TabColor; 6] = [
        TabColor::Red,
        TabColor::Yellow,
        TabColor::Green,
        TabColor::Cyan,
        TabColor::Blue,
        TabColor::Magenta,
    ];

    pub fn ansi_index(self) -> usize {
        match self {
            TabColor::Red => 1,
            TabColor::Green => 2,
            TabColor::Yellow => 3,
            TabColor::Blue => 4,
            TabColor::Magenta => 5,
            TabColor::Cyan => 6,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabIcon {
    Terminal,
    Server,
    Database,
    Code,
    Globe,
    Package,
    Flask,
    Rocket,
}

impl TabIcon {
    pub const ALL: [TabIcon; 8] = [
        TabIcon::Terminal,
        TabIcon::Server,
        TabIcon::Database,
        TabIcon::Code,
        TabIcon::Globe,
        TabIcon::Package,
        TabIcon::Flask,
        TabIcon::Rocket,
    ];

    /// Name of the bundled icon asset.
    pub fn asset(self) -> &'static str {
        match self {
            TabIcon::Terminal => "terminal",
            TabIcon::Server => "server",
            TabIcon::Database => "database",
            TabIcon::Code => "code",
            TabIcon::Globe => "globe",
            TabIcon::Package => "package",
            TabIcon::Flask => "flask-conical",
            TabIcon::Rocket => "rocket",
        }
    }
}

/// User customizations of a tab.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabStyle {
    /// Replaces the title reported by the running program.
    pub name: Option<String>,
    pub color: Option<TabColor>,
    pub icon: Option<TabIcon>,
    pub pinned: bool,
    /// A git worktree created for this tab, offered for removal later.
    pub worktree: Option<std::path::PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub id: GroupId,
    pub name: String,
    pub color: Option<TabColor>,
    pub collapsed: bool,
    tabs: Vec<TabId>,
}

impl Group {
    pub fn tabs(&self) -> &[TabId] {
        &self.tabs
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    Tab(TabId),
    Group(GroupId),
}

/// Where a moved tab should land.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabDestination {
    /// Directly before another tab, in that tab's container.
    Before(TabId),
    /// At the end of a group.
    IntoGroup(GroupId),
    /// At the end of the top level.
    End,
}

/// One visible line of the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Group(GroupId),
    Tab { id: TabId, group: Option<GroupId> },
}

#[derive(Debug, Default)]
pub struct TabLayout {
    entries: Vec<Entry>,
    groups: Vec<Group>,
    styles: HashMap<TabId, TabStyle>,
    next_id: u64,
}

impl TabLayout {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Add a tab at the end of the top level, or of `group` when given.
    pub fn add_tab(&mut self, style: TabStyle, group: Option<GroupId>) -> TabId {
        let id = TabId(self.next_id());
        self.styles.insert(id, style);
        match group.and_then(|group| self.group_mut(group)) {
            Some(group) => group.tabs.push(id),
            None => self.entries.push(Entry::Tab(id)),
        }
        self.normalize();
        id
    }

    /// Remove a tab; a group left empty is removed with it.
    pub fn remove_tab(&mut self, id: TabId) {
        self.detach(id);
        self.styles.remove(&id);
    }

    pub fn contains(&self, id: TabId) -> bool {
        self.styles.contains_key(&id)
    }

    pub fn style(&self, id: TabId) -> TabStyle {
        self.styles.get(&id).cloned().unwrap_or_default()
    }

    pub fn update_style(&mut self, id: TabId, change: impl FnOnce(&mut TabStyle)) {
        if let Some(style) = self.styles.get_mut(&id) {
            change(style);
            self.normalize();
        }
    }

    pub fn group(&self, id: GroupId) -> Option<&Group> {
        self.groups.iter().find(|group| group.id == id)
    }

    pub fn group_mut(&mut self, id: GroupId) -> Option<&mut Group> {
        self.groups.iter_mut().find(|group| group.id == id)
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    pub fn group_of(&self, tab: TabId) -> Option<GroupId> {
        self.groups
            .iter()
            .find(|group| group.tabs.contains(&tab))
            .map(|group| group.id)
    }

    /// Wrap a tab in a new group that takes its place.
    pub fn group_tab(&mut self, tab: TabId, name: String) -> GroupId {
        let id = GroupId(self.next_id());
        let position = self.top_level_position_of(tab);
        self.detach(tab);
        self.groups.push(Group {
            id,
            name,
            color: None,
            collapsed: false,
            tabs: vec![tab],
        });
        let position = position
            .unwrap_or(self.entries.len())
            .min(self.entries.len());
        self.entries.insert(position, Entry::Group(id));
        self.normalize();
        id
    }

    /// Create an empty group at the end of the top level.
    pub fn add_group(&mut self, name: String) -> GroupId {
        let id = GroupId(self.next_id());
        self.groups.push(Group {
            id,
            name,
            color: None,
            collapsed: false,
            tabs: Vec::new(),
        });
        self.entries.push(Entry::Group(id));
        id
    }

    /// Dissolve a group, putting its tabs where the group was.
    pub fn ungroup(&mut self, id: GroupId) {
        let Some(index) = self.groups.iter().position(|group| group.id == id) else {
            return;
        };
        let group = self.groups.remove(index);
        let position = self
            .entries
            .iter()
            .position(|entry| *entry == Entry::Group(id))
            .unwrap_or(self.entries.len());
        self.entries.retain(|entry| *entry != Entry::Group(id));
        for (offset, tab) in group.tabs.into_iter().enumerate() {
            self.entries.insert(position + offset, Entry::Tab(tab));
        }
        self.normalize();
    }

    pub fn move_tab(&mut self, tab: TabId, destination: TabDestination) {
        if destination == TabDestination::Before(tab) || !self.contains(tab) {
            return;
        }
        // Detaching may delete the source group, so resolve the target first.
        let target_group = match destination {
            TabDestination::Before(other) => self.group_of(other),
            TabDestination::IntoGroup(group) => Some(group),
            TabDestination::End => None,
        };
        let source_group = self.group_of(tab);
        if let (Some(target), TabDestination::IntoGroup(_)) = (target_group, destination)
            && self.group(target).is_none()
        {
            return;
        }
        // Keep the source group alive while the tab moves within it.
        self.remove_from_container(tab);

        match (destination, target_group) {
            (TabDestination::Before(other), Some(group)) => {
                if let Some(group) = self.group_mut(group) {
                    let index = group
                        .tabs
                        .iter()
                        .position(|id| *id == other)
                        .unwrap_or(group.tabs.len());
                    group.tabs.insert(index, tab);
                }
            }
            (TabDestination::Before(other), None) => {
                let index = self
                    .entries
                    .iter()
                    .position(|entry| *entry == Entry::Tab(other))
                    .unwrap_or(self.entries.len());
                self.entries.insert(index, Entry::Tab(tab));
            }
            (TabDestination::IntoGroup(_), Some(group)) => {
                if let Some(group) = self.group_mut(group) {
                    group.tabs.push(tab);
                }
            }
            _ => self.entries.push(Entry::Tab(tab)),
        }
        if let Some(source) = source_group {
            self.remove_group_if_empty(source);
        }
        self.normalize();
    }

    /// Move a group to just before a top-level entry, or to the end.
    pub fn move_group(&mut self, group: GroupId, before: Option<Entry>) {
        if before == Some(Entry::Group(group)) || self.group(group).is_none() {
            return;
        }
        self.entries.retain(|entry| *entry != Entry::Group(group));
        let index = before
            .and_then(|before| self.entries.iter().position(|entry| *entry == before))
            .unwrap_or(self.entries.len());
        self.entries.insert(index, Entry::Group(group));
        self.normalize();
    }

    /// Tabs in display order, including those inside collapsed groups.
    pub fn ordered_tabs(&self) -> Vec<TabId> {
        let mut tabs = Vec::new();
        for entry in &self.entries {
            match entry {
                Entry::Tab(id) => tabs.push(*id),
                Entry::Group(id) => {
                    if let Some(group) = self.group(*id) {
                        tabs.extend(group.tabs.iter().copied());
                    }
                }
            }
        }
        tabs
    }

    /// The visible sidebar lines; tabs in collapsed groups are hidden.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for entry in &self.entries {
            match entry {
                Entry::Tab(id) => rows.push(Row::Tab {
                    id: *id,
                    group: None,
                }),
                Entry::Group(id) => {
                    let Some(group) = self.group(*id) else {
                        continue;
                    };
                    rows.push(Row::Group(*id));
                    if !group.collapsed {
                        rows.extend(group.tabs.iter().map(|tab| Row::Tab {
                            id: *tab,
                            group: Some(*id),
                        }));
                    }
                }
            }
        }
        rows
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn top_level_position_of(&self, tab: TabId) -> Option<usize> {
        match self.group_of(tab) {
            Some(group) => self
                .entries
                .iter()
                .position(|entry| *entry == Entry::Group(group))
                .map(|index| index + 1),
            None => self
                .entries
                .iter()
                .position(|entry| *entry == Entry::Tab(tab)),
        }
    }

    fn remove_from_container(&mut self, tab: TabId) {
        self.entries.retain(|entry| *entry != Entry::Tab(tab));
        for group in &mut self.groups {
            group.tabs.retain(|id| *id != tab);
        }
    }

    fn detach(&mut self, tab: TabId) {
        let group = self.group_of(tab);
        self.remove_from_container(tab);
        if let Some(group) = group {
            self.remove_group_if_empty(group);
        }
    }

    fn remove_group_if_empty(&mut self, id: GroupId) {
        if self.group(id).is_some_and(|group| group.tabs.is_empty()) {
            self.groups.retain(|group| group.id != id);
            self.entries.retain(|entry| *entry != Entry::Group(id));
        }
    }

    /// Stable-partition every container so pinned tabs come first.
    fn normalize(&mut self) {
        let styles = &self.styles;
        let pinned = |id: &TabId| styles.get(id).is_some_and(|style| style.pinned);
        let (mut front, back): (Vec<Entry>, Vec<Entry>) = self
            .entries
            .iter()
            .partition(|entry| matches!(entry, Entry::Tab(id) if pinned(id)));
        front.extend(back);
        self.entries = front;
        for group in &mut self.groups {
            let (mut front, back): (Vec<TabId>, Vec<TabId>) =
                group.tabs.iter().partition(|id| pinned(id));
            front.extend(back);
            group.tabs = front;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_with(count: usize) -> (TabLayout, Vec<TabId>) {
        let mut layout = TabLayout::default();
        let tabs = (0..count)
            .map(|_| layout.add_tab(TabStyle::default(), None))
            .collect();
        (layout, tabs)
    }

    #[test]
    fn grouping_a_tab_keeps_its_position() {
        let (mut layout, tabs) = layout_with(3);
        let group = layout.group_tab(tabs[1], "Servers".into());
        assert_eq!(
            layout.entries(),
            &[
                Entry::Tab(tabs[0]),
                Entry::Group(group),
                Entry::Tab(tabs[2])
            ]
        );
        assert_eq!(layout.ordered_tabs(), tabs);
    }

    #[test]
    fn moving_last_tab_out_removes_group() {
        let (mut layout, tabs) = layout_with(2);
        let group = layout.group_tab(tabs[0], "Work".into());
        layout.move_tab(tabs[0], TabDestination::End);
        assert!(layout.group(group).is_none());
        assert_eq!(layout.ordered_tabs(), vec![tabs[1], tabs[0]]);
    }

    #[test]
    fn moving_within_a_group_reorders() {
        let (mut layout, tabs) = layout_with(3);
        let group = layout.group_tab(tabs[0], "Work".into());
        layout.move_tab(tabs[1], TabDestination::IntoGroup(group));
        layout.move_tab(tabs[2], TabDestination::IntoGroup(group));
        layout.move_tab(tabs[2], TabDestination::Before(tabs[0]));
        assert_eq!(
            layout.group(group).unwrap().tabs(),
            &[tabs[2], tabs[0], tabs[1]]
        );
    }

    #[test]
    fn moving_sole_tab_before_itself_is_a_no_op() {
        let (mut layout, tabs) = layout_with(1);
        let group = layout.group_tab(tabs[0], "Solo".into());
        layout.move_tab(tabs[0], TabDestination::Before(tabs[0]));
        assert!(layout.group(group).is_some());
    }

    #[test]
    fn pinned_tabs_come_first() {
        let (mut layout, tabs) = layout_with(3);
        layout.update_style(tabs[2], |style| style.pinned = true);
        assert_eq!(layout.ordered_tabs(), vec![tabs[2], tabs[0], tabs[1]]);
        layout.move_tab(tabs[0], TabDestination::Before(tabs[2]));
        assert_eq!(layout.ordered_tabs()[0], tabs[2]);
    }

    #[test]
    fn collapsed_groups_hide_their_tabs() {
        let (mut layout, tabs) = layout_with(2);
        let group = layout.group_tab(tabs[0], "Hidden".into());
        layout.group_mut(group).unwrap().collapsed = true;
        assert_eq!(
            layout.rows(),
            vec![
                Row::Group(group),
                Row::Tab {
                    id: tabs[1],
                    group: None
                }
            ]
        );
        assert_eq!(layout.ordered_tabs(), tabs);
    }

    #[test]
    fn ungroup_restores_tabs_in_place() {
        let (mut layout, tabs) = layout_with(3);
        let group = layout.group_tab(tabs[1], "Temp".into());
        layout.move_tab(tabs[2], TabDestination::IntoGroup(group));
        layout.ungroup(group);
        assert_eq!(
            layout.entries(),
            &[
                Entry::Tab(tabs[0]),
                Entry::Tab(tabs[1]),
                Entry::Tab(tabs[2])
            ]
        );
    }

    #[test]
    fn groups_can_be_reordered() {
        let (mut layout, tabs) = layout_with(2);
        let group = layout.group_tab(tabs[1], "Later".into());
        layout.move_group(group, Some(Entry::Tab(tabs[0])));
        assert_eq!(
            layout.entries(),
            &[Entry::Group(group), Entry::Tab(tabs[0])]
        );
    }

    #[test]
    fn removing_a_tab_drops_its_style() {
        let (mut layout, tabs) = layout_with(1);
        layout.update_style(tabs[0], |style| style.name = Some("api".into()));
        layout.remove_tab(tabs[0]);
        assert!(!layout.contains(tabs[0]));
        assert!(layout.ordered_tabs().is_empty());
    }
}
