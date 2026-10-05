//! The terminals inside one tab, arranged as resizable splits.

use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc};

use anyhow::{Result, anyhow};
use gpui::{
    AnyElement, App, Bounds, Context, CursorStyle, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, SharedString, Subscription,
    Window, actions, canvas, div, prelude::*, px, relative,
};
use serde::{Deserialize, Serialize};

use crate::{
    confirm::confirm_close,
    pane_tree::{Axis, PaneNode, PaneTree, Split},
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
    /// The last pane's shell exited.
    Exited,
    Attention(Attention),
}

/// A saved pane arrangement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaneState {
    Terminal {
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
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        let view = TerminalView::build(cwd, cx)?;
        Ok(cx.new(|cx| Self::from_tree(PaneTree::new(view.clone()), view, window, cx)))
    }

    /// Recreate a saved arrangement. Panes whose shell fails to start are
    /// left out; fails only if none start.
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
            pane_subscriptions: HashMap::new(),
        };
        for view in group.tree.leaves() {
            group.watch(&view, window, cx);
        }
        group
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
            cx.subscribe_in(view, window, |group, view, event, window, cx| match event {
                TerminalEvent::MetadataChanged => {
                    if *view == group.active {
                        cx.emit(PaneGroupEvent::MetadataChanged);
                    }
                }
                TerminalEvent::Exited => group.remove_pane(view.clone(), window, cx),
                TerminalEvent::Attention(attention) => {
                    cx.emit(PaneGroupEvent::Attention(attention.clone()));
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
        let cwd = self.active_metadata(cx).cwd.clone();
        let view = match TerminalView::build(cwd.as_deref(), cx) {
            Ok(view) => view,
            Err(error) => {
                log::error!("failed to open terminal: {error:#}");
                return;
            }
        };
        let target = self.active.clone();
        if self.tree.split(&target, view.clone(), axis) {
            self.zoomed = false;
            self.watch(&view, window, cx);
            self.focus(view, window, cx);
        }
    }

    fn focus(&mut self, view: Entity<TerminalView>, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&view.focus_handle(cx), cx);
        self.active = view;
        cx.emit(PaneGroupEvent::MetadataChanged);
        cx.notify();
    }

    fn remove_pane(
        &mut self,
        view: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let leaves = self.tree.leaves();
        if leaves.len() == 1 {
            cx.emit(PaneGroupEvent::Exited);
            return;
        }
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
        if self.pane_count() > 1 {
            let pane = self.active.clone();
            let metadata = pane.read(cx).metadata();
            let running = metadata
                .running
                .then(|| metadata.process.clone())
                .flatten()
                .into_iter()
                .collect();
            confirm_close(
                running,
                "Close this pane?",
                window,
                cx,
                move |group, window, cx| {
                    group.remove_pane(pane, window, cx);
                },
            );
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
        let dimmed = self.pane_count() > 1 && *view != self.active;
        div()
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
            .into_any_element()
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

fn snapshot_node(node: &PaneNode<Entity<TerminalView>>, cx: &App) -> PaneState {
    match node {
        PaneNode::Leaf(view) => PaneState::Terminal {
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
        PaneState::Terminal { cwd } => match TerminalView::build(cwd.as_deref(), cx) {
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
