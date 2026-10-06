//! The terminals inside one tab, arranged as resizable splits.

use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc};

use anyhow::{Result, anyhow};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, CursorStyle, DragMoveEvent, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels,
    SharedString, Subscription, WeakEntity, Window, actions, canvas, div, prelude::*, px, relative,
};
use serde::{Deserialize, Serialize};

use crate::{
    components::{DragPreview, icon_button},
    pane_tree::{Axis, PaneNode, PaneTree, Split},
    sidebar::DraggedTab,
    tabs::TabId,
    terminal_view::{Attention, TabMetadata, TerminalEvent, TerminalView},
    theme::ActiveThemeExt,
    workspace::CloseTab,
};

actions!(
    panes,
    [
        SplitRight,
        SplitDown,
        ClosePane,
        FocusLeft,
        FocusRight,
        FocusUp,
        FocusDown,
        ToggleZoom,
        EqualizePanes,
    ]
);

pub const KEY_CONTEXT: &str = "Panes";

/// Width of the grabbable strip between panes.
const DIVIDER_HIT_SIZE: f32 = 5.;

pub enum PaneGroupEvent {
    /// The focused pane changed, or its title, directory, or branch did.
    MetadataChanged,
    /// A pane was hidden and needs a session entry outside this arrangement.
    Hidden(Entity<TerminalView>),
    Attention {
        source: Entity<TerminalView>,
        attention: Attention,
    },
    /// A pane from another tab was dropped on one of this group's panes.
    PaneDropped {
        dragged: DraggedPane,
        target: Entity<TerminalView>,
        edge: Edge,
    },
    /// A tab from the sidebar was dropped on one of this group's panes.
    TabDropped {
        tab: TabId,
        target: Entity<TerminalView>,
        edge: Edge,
    },
}

/// Payload while a pane is dragged by its handle.
#[derive(Clone)]
pub struct DraggedPane {
    pub source: WeakEntity<PaneGroup>,
    pub view: Entity<TerminalView>,
    pub label: SharedString,
}

/// The side of a pane something is dropped on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// The edge of `bounds` nearest to `position`, relative to its size.
    fn nearest(bounds: Bounds<Pixels>, position: gpui::Point<Pixels>) -> Self {
        let x = ((position.x - bounds.left()) / bounds.size.width).clamp(0., 1.);
        let y = ((position.y - bounds.top()) / bounds.size.height).clamp(0., 1.);
        let distances = [
            (x, Edge::Left),
            (1. - x, Edge::Right),
            (y, Edge::Top),
            (1. - y, Edge::Bottom),
        ];
        distances
            .into_iter()
            .min_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(_, edge)| edge)
            .unwrap_or(Edge::Right)
    }

    /// The split axis and whether the dropped pane goes first.
    fn placement(self) -> (Axis, bool) {
        match self {
            Edge::Left => (Axis::Horizontal, true),
            Edge::Right => (Axis::Horizontal, false),
            Edge::Top => (Axis::Vertical, true),
            Edge::Bottom => (Axis::Vertical, false),
        }
    }
}

/// What happened to a group when one of its panes was taken out.
pub enum Detached {
    /// The pane was removed; others remain.
    Removed,
    /// It was the only pane, so the group is left as it was.
    WasLast,
}

/// A saved pane arrangement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaneState {
    Terminal {
        #[serde(default)]
        session_id: Option<u64>,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    Split {
        axis: Axis,
        ratios: Vec<f32>,
        children: Vec<PaneState>,
    },
}

#[derive(Clone, Copy)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

struct Dragging {
    split_id: usize,
    index: usize,
    axis: Axis,
}

pub struct PaneGroup {
    tree: PaneTree<Entity<TerminalView>>,
    active: Entity<TerminalView>,
    zoomed: bool,
    /// Last measured bounds of each split, for turning drags into ratios.
    split_bounds: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
    dragging: Option<Dragging>,
    /// The pane and edge a drag is currently over.
    drop_target: Option<(EntityId, Edge)>,
    pane_subscriptions: HashMap<EntityId, Vec<Subscription>>,
}

impl EventEmitter<PaneGroupEvent> for PaneGroup {}

impl Focusable for PaneGroup {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.active.focus_handle(cx)
    }
}

impl PaneGroup {
    /// A group with one terminal started in `cwd`.
    pub fn build(
        cwd: Option<&std::path::Path>,
        startup: Option<&str>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        let view = TerminalView::build(cwd, startup, cx)?;
        Ok(cx.new(|cx| Self::from_tree(PaneTree::new(view.clone()), view, window, cx)))
    }

    /// Attach the sessions in a saved arrangement. Fails if none can be displayed.
    pub fn restore(state: &PaneState, window: &mut Window, cx: &mut App) -> Result<Entity<Self>> {
        let root = restore_node(state, cx).ok_or_else(|| anyhow!("no pane could be restored"))?;
        let tree = PaneTree::from_root(root);
        let first = tree
            .leaves()
            .into_iter()
            .next()
            .expect("restored tree has a leaf");
        Ok(cx.new(|cx| Self::from_tree(tree, first, window, cx)))
    }

    fn from_tree(
        tree: PaneTree<Entity<TerminalView>>,
        active: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut group = Self {
            tree,
            active,
            zoomed: false,
            split_bounds: Rc::default(),
            dragging: None,
            drop_target: None,
            pane_subscriptions: HashMap::new(),
        };
        for view in group.tree.leaves() {
            group.watch(&view, window, cx);
        }
        group
    }

    /// A group holding an existing terminal, such as a pane dragged out of
    /// another tab.
    pub fn from_view(
        view: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| Self::from_tree(PaneTree::new(view.clone()), view, window, cx))
    }

    /// The whole arrangement, for merging into another tab.
    pub fn root(&self) -> PaneNode<Entity<TerminalView>> {
        self.tree.root().clone()
    }

    /// Take a pane out of this group without ending its shell.
    pub fn detach(
        &mut self,
        view: &Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Detached {
        if self.pane_count() == 1 {
            return Detached::WasLast;
        }
        self.remove_from_tree(view.clone(), window, cx);
        Detached::Removed
    }

    /// Add panes (one, or a whole arrangement) beside `target`.
    pub fn adopt(
        &mut self,
        node: PaneNode<Entity<TerminalView>>,
        target: &Entity<TerminalView>,
        edge: Edge,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut adopted = Vec::new();
        collect_views(&node, &mut adopted);
        let (axis, before) = edge.placement();
        if !self.tree.insert(target, node, axis, before) {
            return;
        }
        self.zoomed = false;
        for view in &adopted {
            self.watch(view, window, cx);
        }
        if let Some(first) = adopted.into_iter().next() {
            self.focus(first, window, cx);
        }
    }

    fn track_drop(
        &mut self,
        pane: EntityId,
        bounds: Bounds<Pixels>,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let next = if bounds.contains(&position) {
            Some((pane, Edge::nearest(bounds, position)))
        } else if self.drop_target.is_some_and(|(target, _)| target == pane) {
            None
        } else {
            return;
        };
        if next != self.drop_target {
            self.drop_target = next;
            cx.notify();
        }
    }

    fn drop_pane(
        &mut self,
        dragged: DraggedPane,
        target: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let edge = self.take_drop_edge(&target);
        cx.notify();
        let Some(edge) = edge else {
            return;
        };
        if dragged.view == target {
            return;
        }
        let from_here = dragged.source.entity_id() == cx.entity_id();
        if from_here {
            // Rearranging within the tab: lift the pane out, then insert it.
            self.tree.remove(&dragged.view);
            let (axis, before) = edge.placement();
            self.tree
                .insert(&target, PaneNode::Leaf(dragged.view.clone()), axis, before);
            self.zoomed = false;
            self.focus(dragged.view, window, cx);
        } else {
            cx.emit(PaneGroupEvent::PaneDropped {
                dragged,
                target,
                edge,
            });
        }
    }

    fn drop_tab(&mut self, tab: TabId, target: Entity<TerminalView>, cx: &mut Context<Self>) {
        if let Some(edge) = self.take_drop_edge(&target) {
            cx.emit(PaneGroupEvent::TabDropped { tab, target, edge });
        }
        cx.notify();
    }

    fn take_drop_edge(&mut self, target: &Entity<TerminalView>) -> Option<Edge> {
        self.drop_target
            .take()
            .filter(|(pane, _)| *pane == target.entity_id())
            .map(|(_, edge)| edge)
    }

    pub fn active_view(&self) -> &Entity<TerminalView> {
        &self.active
    }

    pub fn active_metadata<'a>(&self, cx: &'a App) -> &'a TabMetadata {
        self.active.read(cx).metadata()
    }

    pub fn pane_count(&self) -> usize {
        self.tree.leaves().len()
    }

    pub fn snapshot(&self, cx: &App) -> PaneState {
        snapshot_node(self.tree.root(), cx)
    }

    fn watch(&mut self, view: &Entity<TerminalView>, window: &mut Window, cx: &mut Context<Self>) {
        let focus = view.focus_handle(cx);
        let subscriptions = vec![
            cx.subscribe_in(view, window, |group, view, event, _, cx| match event {
                TerminalEvent::MetadataChanged => {
                    if *view == group.active {
                        cx.emit(PaneGroupEvent::MetadataChanged);
                    }
                }
                TerminalEvent::Exited => {
                    cx.emit(PaneGroupEvent::MetadataChanged);
                    cx.notify();
                }
                TerminalEvent::Attention(attention) => {
                    cx.emit(PaneGroupEvent::Attention {
                        source: view.clone(),
                        attention: attention.clone(),
                    });
                }
            }),
            cx.on_focus_in(&focus, window, {
                let view = view.clone();
                move |group, _, cx| {
                    if group.active != view {
                        group.active = view.clone();
                        cx.emit(PaneGroupEvent::MetadataChanged);
                        cx.notify();
                    }
                }
            }),
        ];
        self.pane_subscriptions
            .insert(view.entity_id(), subscriptions);
    }

    fn split(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.active.clone();
        if let Err(error) = self.split_pane(&target, axis, None, window, cx) {
            log::error!("failed to open terminal: {error:#}");
        }
    }

    /// Open a terminal beside `target` in its directory, optionally typing
    /// `startup` into it, and focus it. Returns the new pane.
    pub fn split_pane(
        &mut self,
        target: &Entity<TerminalView>,
        axis: Axis,
        startup: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Entity<TerminalView>> {
        let cwd = target.read(cx).metadata().cwd.clone();
        let view = TerminalView::build(cwd.as_deref(), startup, cx)?;
        if !self.tree.split(target, view.clone(), axis) {
            return Err(anyhow!("the pane is not in this tab"));
        }
        self.zoomed = false;
        self.watch(&view, window, cx);
        self.focus(view.clone(), window, cx);
        Ok(view)
    }

    /// Every pane, in reading order.
    pub fn views(&self) -> Vec<Entity<TerminalView>> {
        self.tree.leaves()
    }

    fn focus(&mut self, view: Entity<TerminalView>, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&view.focus_handle(cx), cx);
        self.active = view;
        cx.emit(PaneGroupEvent::MetadataChanged);
        cx.notify();
    }

    fn remove_from_tree(
        &mut self,
        view: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let leaves = self.tree.leaves();
        let position = leaves.iter().position(|leaf| *leaf == view);
        if !self.tree.remove(&view) {
            return;
        }
        self.pane_subscriptions.remove(&view.entity_id());
        self.zoomed = false;
        if self.active == view {
            let remaining = self.tree.leaves();
            let next = position
                .map(|position| position.saturating_sub(1).min(remaining.len() - 1))
                .and_then(|index| remaining.get(index).cloned())
                .unwrap_or_else(|| remaining[0].clone());
            self.focus(next, window, cx);
        } else {
            cx.notify();
        }
    }

    fn split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        self.split(Axis::Horizontal, window, cx);
    }

    fn split_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
        self.split(Axis::Vertical, window, cx);
    }

    /// Programs other than the shell running in any pane.
    pub fn running_programs(&self, cx: &App) -> Vec<SharedString> {
        self.tree
            .leaves()
            .iter()
            .filter_map(|view| {
                let metadata = view.read(cx).metadata();
                metadata.running.then(|| metadata.process.clone()).flatten()
            })
            .collect()
    }

    fn close_pane(&mut self, _: &ClosePane, window: &mut Window, cx: &mut Context<Self>) {
        self.request_close_pane(self.active.clone(), window, cx);
    }

    /// Hide a pane while its session remains available in the sidebar.
    fn request_close_pane(
        &mut self,
        pane: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane_count() > 1 {
            self.remove_from_tree(pane.clone(), window, cx);
            cx.emit(PaneGroupEvent::Hidden(pane));
        } else {
            // A lone pane closes the whole tab, which respects pinning.
            window.dispatch_action(Box::new(CloseTab), cx);
        }
    }

    fn toggle_zoom(&mut self, _: &ToggleZoom, _: &mut Window, cx: &mut Context<Self>) {
        if self.pane_count() > 1 {
            self.zoomed = !self.zoomed;
            cx.notify();
        }
    }

    fn equalize(&mut self, _: &EqualizePanes, _: &mut Window, cx: &mut Context<Self>) {
        self.tree.equalize();
        cx.notify();
    }

    fn focus_left(&mut self, _: &FocusLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_direction(Direction::Left, window, cx);
    }

    fn focus_right(&mut self, _: &FocusRight, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_direction(Direction::Right, window, cx);
    }

    fn focus_up(&mut self, _: &FocusUp, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_direction(Direction::Up, window, cx);
    }

    fn focus_down(&mut self, _: &FocusDown, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_direction(Direction::Down, window, cx);
    }

    /// Focus the nearest pane in `direction` that overlaps the active one
    /// across that direction.
    fn focus_direction(
        &mut self,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(from) = self.active.read(cx).bounds() else {
            return;
        };
        let candidate = self
            .tree
            .leaves()
            .into_iter()
            .filter(|view| *view != self.active)
            .filter_map(|view| view.read(cx).bounds().map(|bounds| (view, bounds)))
            .filter_map(|(view, to)| {
                let (gap, overlaps) = match direction {
                    Direction::Left => (
                        from.left() - to.right(),
                        ranges_overlap(from.top(), from.bottom(), to.top(), to.bottom()),
                    ),
                    Direction::Right => (
                        to.left() - from.right(),
                        ranges_overlap(from.top(), from.bottom(), to.top(), to.bottom()),
                    ),
                    Direction::Up => (
                        from.top() - to.bottom(),
                        ranges_overlap(from.left(), from.right(), to.left(), to.right()),
                    ),
                    Direction::Down => (
                        to.top() - from.bottom(),
                        ranges_overlap(from.left(), from.right(), to.left(), to.right()),
                    ),
                };
                (overlaps && gap >= px(-1.)).then_some((view, gap))
            })
            .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((view, _)) = candidate {
            self.focus(view, window, cx);
        }
    }

    fn on_drag_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(dragging) = &self.dragging else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.dragging = None;
            return;
        }
        let Some(bounds) = self.split_bounds.borrow().get(&dragging.split_id).copied() else {
            return;
        };
        let position = match dragging.axis {
            Axis::Horizontal => (event.position.x - bounds.left()) / bounds.size.width,
            Axis::Vertical => (event.position.y - bounds.top()) / bounds.size.height,
        };
        self.tree
            .resize(dragging.split_id, dragging.index, position);
        cx.notify();
    }

    fn render_node(
        &self,
        node: &PaneNode<Entity<TerminalView>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            PaneNode::Leaf(view) => self.render_leaf(view, cx),
            PaneNode::Split(split) => self.render_split(split, cx),
        }
    }

    fn render_leaf(&self, view: &Entity<TerminalView>, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let split = self.pane_count() > 1;
        let dimmed = split && *view != self.active;
        let id = view.entity_id();
        let hover_group = SharedString::from(format!("pane-{id}"));
        let accent = theme.text_accent;
        let drop_edge = self
            .drop_target
            .filter(|(target, _)| *target == id && cx.has_active_drag())
            .map(|(_, edge)| edge);
        let view_for_pane = view.clone();
        let view_for_tab = view.clone();

        div()
            .id(("pane", id.as_u64() as usize))
            .group(hover_group.clone())
            .relative()
            .size_full()
            .child(view.clone())
            // Unfocused panes recede slightly so the active one stands out.
            .when(dimmed, |pane| {
                pane.child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(theme.terminal_background().opacity(0.3)),
                )
            })
            .on_drag_move::<DraggedPane>(cx.listener(
                move |group, event: &DragMoveEvent<DraggedPane>, _, cx| {
                    group.track_drop(id, event.bounds, event.event.position, cx);
                },
            ))
            .on_drag_move::<DraggedTab>(cx.listener(
                move |group, event: &DragMoveEvent<DraggedTab>, _, cx| {
                    group.track_drop(id, event.bounds, event.event.position, cx);
                },
            ))
            .on_drop(
                cx.listener(move |group, dragged: &DraggedPane, window, cx| {
                    group.drop_pane(dragged.clone(), view_for_pane.clone(), window, cx);
                }),
            )
            .on_drop(cx.listener(move |group, dragged: &DraggedTab, _, cx| {
                group.drop_tab(dragged.id, view_for_tab.clone(), cx);
            }))
            .children(drop_edge.map(|edge| {
                div()
                    .absolute()
                    .map(|zone| match edge {
                        Edge::Left => zone.left_0().top_0().bottom_0().w(relative(0.5)),
                        Edge::Right => zone.right_0().top_0().bottom_0().w(relative(0.5)),
                        Edge::Top => zone.top_0().left_0().right_0().h(relative(0.5)),
                        Edge::Bottom => zone.bottom_0().left_0().right_0().h(relative(0.5)),
                    })
                    .bg(accent.opacity(0.14))
                    .border_2()
                    .border_color(accent.opacity(0.6))
            }))
            .when(split, |pane| {
                pane.child(self.render_pane_chrome(view, &hover_group, cx))
            })
            .into_any_element()
    }

    /// The grab handle (top center) and close button (top right) shown on
    /// panes of a split tab.
    fn render_pane_chrome(
        &self,
        view: &Entity<TerminalView>,
        hover_group: &SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let id = view.entity_id().as_u64() as usize;
        let label = view.read(cx).metadata().title.clone();
        let dragged = DraggedPane {
            source: cx.weak_entity(),
            view: view.clone(),
            label,
        };
        let close_view = view.clone();
        let handle_color = theme.text_muted;

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(18.))
            .flex()
            .justify_center()
            .child(
                div()
                    .id(("pane-handle", id))
                    .mt(px(3.))
                    .w(px(44.))
                    .h(px(12.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .cursor(CursorStyle::OpenHand)
                    .opacity(0.35)
                    .group_hover(hover_group.clone(), |style| style.opacity(1.))
                    .hover(|style| style.bg(theme.ghost_hover))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_drag(dragged, |dragged, _, _, cx| {
                        cx.new(|_| DragPreview {
                            label: dragged.label.clone(),
                            icon: "terminal",
                        })
                    })
                    .child(div().w(px(28.)).h(px(4.)).rounded_full().bg(handle_color)),
            )
            .child(
                icon_button(("pane-close", id), "x", &theme)
                    .absolute()
                    .top(px(4.))
                    .right(px(6.))
                    .size(px(18.))
                    .invisible()
                    .group_hover(hover_group.clone(), |style| style.visible())
                    .on_click(cx.listener(move |group, _: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        group.request_close_pane(close_view.clone(), window, cx);
                    })),
            )
    }

    fn render_split(
        &self,
        split: &Split<Entity<TerminalView>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let horizontal = split.axis == Axis::Horizontal;
        let split_id = split.id;
        let axis = split.axis;
        let bounds_store = self.split_bounds.clone();
        let mut children: Vec<AnyElement> = Vec::new();

        for (index, (child, ratio)) in split.children.iter().zip(&split.ratios).enumerate() {
            if index > 0 {
                let divider_index = index - 1;
                children.push(
                    div()
                        .id(("pane-divider", split_id * 1000 + divider_index))
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .map(|divider| {
                            if horizontal {
                                divider
                                    .w(px(DIVIDER_HIT_SIZE))
                                    .h_full()
                                    .cursor(CursorStyle::ResizeLeftRight)
                            } else {
                                divider
                                    .h(px(DIVIDER_HIT_SIZE))
                                    .w_full()
                                    .cursor(CursorStyle::ResizeUpDown)
                            }
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |group, _: &MouseDownEvent, _, cx| {
                                group.dragging = Some(Dragging {
                                    split_id,
                                    index: divider_index,
                                    axis,
                                });
                                cx.stop_propagation();
                            }),
                        )
                        .on_click(cx.listener(move |group, event: &gpui::ClickEvent, _, cx| {
                            // Double-clicking a divider evens out the split.
                            if event.click_count() == 2 {
                                group.tree.equalize();
                                cx.notify();
                            }
                        }))
                        .child(div().bg(theme.border).map(|line| {
                            if horizontal {
                                line.w(px(1.)).h_full()
                            } else {
                                line.h(px(1.)).w_full()
                            }
                        }))
                        .into_any_element(),
                );
            }
            children.push(
                div()
                    .flex_basis(relative(*ratio))
                    .flex_grow_0()
                    .flex_shrink(1.)
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .map(|cell| {
                        if horizontal {
                            cell.h_full()
                        } else {
                            cell.w_full()
                        }
                    })
                    .child(self.render_node(child, cx))
                    .into_any_element(),
            );
        }

        div()
            .relative()
            .flex()
            .size_full()
            .map(|container| {
                if horizontal {
                    container.flex_row()
                } else {
                    container.flex_col()
                }
            })
            .child(
                canvas(
                    move |bounds, _, _| {
                        bounds_store.borrow_mut().insert(split_id, bounds);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .children(children)
            .into_any_element()
    }
}

fn ranges_overlap(a_start: Pixels, a_end: Pixels, b_start: Pixels, b_end: Pixels) -> bool {
    a_start < b_end && b_start < a_end
}

fn collect_views(node: &PaneNode<Entity<TerminalView>>, views: &mut Vec<Entity<TerminalView>>) {
    match node {
        PaneNode::Leaf(view) => views.push(view.clone()),
        PaneNode::Split(split) => split
            .children
            .iter()
            .for_each(|child| collect_views(child, views)),
    }
}

fn snapshot_node(node: &PaneNode<Entity<TerminalView>>, cx: &App) -> PaneState {
    match node {
        PaneNode::Leaf(view) => PaneState::Terminal {
            session_id: Some(view.read(cx).session_id()),
            cwd: view.read(cx).metadata().cwd.clone(),
        },
        PaneNode::Split(split) => PaneState::Split {
            axis: split.axis,
            ratios: split.ratios.clone(),
            children: split
                .children
                .iter()
                .map(|child| snapshot_node(child, cx))
                .collect(),
        },
    }
}

fn restore_node(state: &PaneState, cx: &mut App) -> Option<PaneNode<Entity<TerminalView>>> {
    match state {
        PaneState::Terminal { session_id, cwd } => match match session_id {
            Some(id) => TerminalView::attach_in(*id, cwd.as_deref(), cx),
            None => TerminalView::build(cwd.as_deref(), None, cx),
        } {
            Ok(view) => Some(PaneNode::Leaf(view)),
            Err(error) => {
                log::error!("failed to restore terminal: {error:#}");
                None
            }
        },
        PaneState::Split {
            axis,
            ratios,
            children,
        } => {
            let mut restored = Vec::new();
            let mut kept_ratios = Vec::new();
            for (index, child) in children.iter().enumerate() {
                if let Some(node) = restore_node(child, cx) {
                    restored.push(node);
                    kept_ratios.push(ratios.get(index).copied().unwrap_or(1.).max(0.01));
                }
            }
            match restored.len() {
                0 => None,
                1 => restored.pop(),
                _ => {
                    let total: f32 = kept_ratios.iter().sum();
                    Some(PaneNode::Split(Split {
                        id: 0,
                        axis: *axis,
                        children: restored,
                        ratios: kept_ratios.into_iter().map(|ratio| ratio / total).collect(),
                    }))
                }
            }
        }
    }
}

impl Render for PaneGroup {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if self.zoomed {
            self.render_leaf(&self.active.clone(), cx)
        } else {
            let root = self.tree.root().clone();
            self.render_node(&root, cx)
        };

        div()
            .key_context(KEY_CONTEXT)
            .size_full()
            .on_action(cx.listener(Self::split_right))
            .on_action(cx.listener(Self::split_down))
            .on_action(cx.listener(Self::close_pane))
            .on_action(cx.listener(Self::focus_left))
            .on_action(cx.listener(Self::focus_right))
            .on_action(cx.listener(Self::focus_up))
            .on_action(cx.listener(Self::focus_down))
            .on_action(cx.listener(Self::toggle_zoom))
            .on_action(cx.listener(Self::equalize))
            .when(
                !cx.has_active_drag() && self.drop_target.is_some(),
                |root| {
                    // A drag that ended elsewhere leaves no highlight behind.
                    self.drop_target = None;
                    root
                },
            )
            .when(self.dragging.is_some(), |root| {
                root.on_mouse_move(cx.listener(Self::on_drag_move))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|group, _, _, _| group.dragging = None),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|group, _, _, _| group.dragging = None),
                    )
            })
            .child(content)
    }
}

/// Key bindings for splits.
pub fn key_bindings() -> Vec<gpui::KeyBinding> {
    let context = Some(KEY_CONTEXT);
    vec![
        gpui::KeyBinding::new("cmd-d", SplitRight, context),
        gpui::KeyBinding::new("cmd-shift-d", SplitDown, context),
        gpui::KeyBinding::new("cmd-w", ClosePane, context),
        gpui::KeyBinding::new("cmd-alt-left", FocusLeft, context),
        gpui::KeyBinding::new("cmd-alt-right", FocusRight, context),
        gpui::KeyBinding::new("cmd-alt-up", FocusUp, context),
        gpui::KeyBinding::new("cmd-alt-down", FocusDown, context),
        gpui::KeyBinding::new("cmd-shift-enter", ToggleZoom, context),
        gpui::KeyBinding::new("cmd-ctrl-=", EqualizePanes, context),
    ]
}

#[cfg(test)]
mod tests {
    use gpui::{point, size};

    use super::*;

    #[test]
    fn nearest_edge_follows_the_pointer() {
        let bounds = Bounds::new(point(px(100.), px(50.)), size(px(400.), px(200.)));
        assert_eq!(Edge::nearest(bounds, point(px(110.), px(150.))), Edge::Left);
        assert_eq!(
            Edge::nearest(bounds, point(px(490.), px(150.))),
            Edge::Right
        );
        assert_eq!(Edge::nearest(bounds, point(px(300.), px(55.))), Edge::Top);
        assert_eq!(
            Edge::nearest(bounds, point(px(300.), px(245.))),
            Edge::Bottom
        );
        assert_eq!(Edge::Left.placement(), (Axis::Horizontal, true));
        assert_eq!(Edge::Bottom.placement(), (Axis::Vertical, false));
    }
}
