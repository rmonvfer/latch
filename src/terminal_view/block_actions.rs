use super::*;

impl TerminalView {
    pub(super) fn on_block_action(&self, cx: &Context<Self>) -> OnBlockAction {
        let view = cx.entity().downgrade();
        Rc::new(move |action, index, window, cx| {
            let _ = view.update(cx, |view, cx| view.block_action(action, index, window, cx));
        })
    }

    /// Run `command` in the shell, now or once it is ready, and remember it.
    pub(super) fn run_command(&mut self, command: &str, cx: &mut Context<Self>) {
        if command.trim().is_empty() {
            return;
        }
        self.selected_blocks.clear();
        self.history.push(command);
        if self.prompt.is_some() {
            self.send(
                Operation::RunCommand {
                    command: command.to_string(),
                },
                cx,
            );
        } else {
            self.queued_command = Some(command.to_string());
        }
    }

    /// Carry out an action on the block at `index`.
    fn block_action(
        &mut self,
        action: BlockAction,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(block) = self
            .blocks
            .as_ref()
            .and_then(|blocks| blocks.blocks().get(index))
        else {
            return;
        };
        let id = block.id;
        let command = block.command.clone();
        let cwd = block.context.cwd.clone();
        let branch = block.context.git_branch.clone();
        if !matches!(action, BlockAction::OpenMenu(_)) {
            self.block_menu = None;
        }
        // Copying and scrolling from a selected block's menu act on every
        // selected block.
        let targets: Vec<usize> = if self.selected_blocks.contains(index) {
            self.selected_blocks.indices().collect()
        } else {
            vec![index]
        };
        match action {
            BlockAction::OpenMenu(position) => {
                // The menu of an unselected block is for that block alone.
                if !self.selected_blocks.contains(index) {
                    self.select_blocks(|selection| selection.select(index), window, cx);
                }
                self.block_menu = Some((index, position));
            }
            BlockAction::ToggleCollapsed => {
                if let Some(block) = self.block_mut(index) {
                    block.collapsed = !block.collapsed;
                }
                self.block_list.remeasure_items(index..index + 1);
            }
            BlockAction::ToggleBookmark => {
                if let Some(block) = self.block_mut(index) {
                    block.bookmarked = !block.bookmarked;
                }
            }
            BlockAction::CopyAll => {
                let text = self.blocks_text(&targets, BlockText::CommandAndOutput, cx);
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            BlockAction::CopyCommand => {
                let text = self.blocks_text(&targets, BlockText::Command, cx);
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            BlockAction::CopyOutput => {
                let text = self.blocks_text(&targets, BlockText::Output, cx);
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            BlockAction::CopyDirectory => {
                if let Some(cwd) = cwd {
                    cx.write_to_clipboard(ClipboardItem::new_string(cwd.display().to_string()));
                }
            }
            BlockAction::CopyBranch => {
                if let Some(branch) = branch {
                    cx.write_to_clipboard(ClipboardItem::new_string(branch));
                }
            }
            BlockAction::Rerun => self.run_command(&command, cx),
            BlockAction::ScrollToTop => {
                let top = targets.first().copied().unwrap_or(index);
                self.block_list.scroll_to(block_view::block_start(top));
            }
            BlockAction::ScrollToBottom => {
                self.scroll_to_block_bottom(targets.last().copied().unwrap_or(index));
            }
            BlockAction::ToggleFilter => self.toggle_block_filter(id, index, window, cx),
            BlockAction::Filter(change) => {
                if let Some(filter) = self
                    .block_filter
                    .as_mut()
                    .filter(|filter| filter.block == id)
                {
                    filter.change(change);
                    self.block_selection = None;
                    self.block_list.remeasure_items(index..index + 1);
                    self.refresh_matches(cx);
                }
            }
            BlockAction::FindInBlock => {
                self.selected_blocks.select(index);
                self.open_search(Some(id), window, cx);
            }
        }
        cx.notify();
    }

    fn block_mut(&mut self, index: usize) -> Option<&mut crate::blocks::Block> {
        self.blocks.as_mut()?.blocks_mut().get_mut(index)
    }

    /// The block at which shortcuts act: the last selected, else the newest.
    fn target_block(&self) -> Option<usize> {
        let count = self.blocks.as_ref()?.blocks().len();
        self.selected_blocks
            .tail()
            .filter(|index| *index < count)
            .or(count.checked_sub(1))
    }

    /// Change which blocks are selected. While any are, the pane takes the
    /// keys from the input, as Warp's block list does.
    pub(super) fn select_blocks(
        &mut self,
        change: impl FnOnce(&mut SelectedBlocks),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        change(&mut self.selected_blocks);
        if self.selected_blocks.is_empty() {
            self.focus_input(window, cx);
        } else {
            self.block_selection = None;
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }

    /// Drop the selected blocks and text, and give the keys back to the
    /// input.
    pub(super) fn clear_block_selections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_blocks.clear();
        self.block_selection = None;
        self.focus_input(window, cx);
        cx.notify();
    }

    fn focus_input(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shows_editor() {
            window.focus(&self.editor.focus_handle(cx), cx);
        }
    }

    /// The text of blocks `indices`, each as `kind` asks, skipping blank
    /// ones, one block after another.
    pub(super) fn blocks_text(&self, indices: &[usize], kind: BlockText, cx: &App) -> String {
        let Some(blocks) = &self.blocks else {
            return String::new();
        };
        indices
            .iter()
            .filter_map(|&index| {
                let command = blocks.blocks().get(index)?.command.as_str();
                let text = match kind {
                    BlockText::Command => command.to_string(),
                    BlockText::Output => self.block_output_text(index, cx),
                    BlockText::CommandAndOutput if command.is_empty() => {
                        self.block_output_text(index, cx)
                    }
                    BlockText::CommandAndOutput => {
                        format!("{command}\n{}", self.block_output_text(index, cx))
                    }
                };
                (!text.trim().is_empty()).then_some(text)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(super) fn copy_block_commands(
        &mut self,
        _: &CopyBlockCommands,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.act_on_target_block(BlockAction::CopyCommand, window, cx);
    }

    pub(super) fn select_previous_block(
        &mut self,
        _: &SelectPreviousBlock,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_adjacent_block(false, false, window, cx);
    }

    pub(super) fn select_next_block(
        &mut self,
        _: &SelectNextBlock,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_adjacent_block(true, false, window, cx);
    }

    pub(super) fn extend_selection_up(
        &mut self,
        _: &ExtendBlockSelectionUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.extend_block_selection(false, window, cx);
    }

    pub(super) fn extend_selection_down(
        &mut self,
        _: &ExtendBlockSelectionDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.extend_block_selection(true, window, cx);
    }

    /// Grow or shrink the selected range by a block from its last end.
    fn extend_block_selection(&mut self, down: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(tail), Some(count)) = (
            self.selected_blocks.tail(),
            self.blocks.as_ref().map(|blocks| blocks.blocks().len()),
        ) else {
            return;
        };
        let next = if down {
            (tail + 1).min(count.saturating_sub(1))
        } else {
            tail.saturating_sub(1)
        };
        self.select_blocks(|selection| selection.extend_to(next), window, cx);
        self.reveal_block(next);
    }

    /// Scroll so block `index` starts at the top of the list, unless its
    /// top is already in view.
    fn reveal_block(&mut self, index: usize) {
        let viewport = self.block_list.viewport_bounds();
        let top_visible = self
            .block_list
            .bounds_for_item(index)
            .is_some_and(|bounds| {
                bounds.top() >= viewport.top() && bounds.top() < viewport.bottom()
            });
        if !top_visible {
            self.block_list.scroll_to(block_view::block_start(index));
        }
    }

    fn act_on_target_block(
        &mut self,
        action: BlockAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.target_block() {
            Some(index) if self.shows_blocks() => self.block_action(action, index, window, cx),
            _ => cx.propagate(),
        }
    }

    pub(super) fn copy_block_output(
        &mut self,
        _: &CopyBlockOutput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.act_on_target_block(BlockAction::CopyOutput, window, cx);
    }

    pub(super) fn toggle_block_bookmark(
        &mut self,
        _: &ToggleBlockBookmark,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.act_on_target_block(BlockAction::ToggleBookmark, window, cx);
    }

    pub(super) fn scroll_to_block_top(
        &mut self,
        _: &ScrollToBlockTop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.act_on_target_block(BlockAction::ScrollToTop, window, cx);
    }

    pub(super) fn scroll_to_block_end(
        &mut self,
        _: &ScrollToBlockBottom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.act_on_target_block(BlockAction::ScrollToBottom, window, cx);
    }

    pub(super) fn toggle_target_filter(
        &mut self,
        _: &ToggleBlockFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.act_on_target_block(BlockAction::ToggleFilter, window, cx);
    }

    pub(super) fn find_in_target_block(
        &mut self,
        _: &FindInBlock,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.act_on_target_block(BlockAction::FindInBlock, window, cx);
    }

    pub(super) fn open_target_menu(
        &mut self,
        _: &OpenBlockMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.target_block() else {
            return;
        };
        // From the keyboard the menu opens by the block's top, or failing
        // that the pane's.
        let position = self
            .block_list
            .bounds_for_item(index)
            .map(|bounds| bounds.origin + gpui::point(px(40.), px(24.)))
            .or(self
                .bounds
                .map(|bounds| bounds.origin + gpui::point(px(40.), px(24.))))
            .unwrap_or_default();
        self.block_action(BlockAction::OpenMenu(position), index, window, cx);
    }

    pub(super) fn previous_bookmark(
        &mut self,
        _: &PreviousBookmark,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.jump_to_bookmark(false, cx);
    }

    pub(super) fn next_bookmark(
        &mut self,
        _: &NextBookmark,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.jump_to_bookmark(true, cx);
    }

    /// Select and scroll to the nearest bookmarked block before (or after)
    /// the selected one.
    fn jump_to_bookmark(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(blocks) = &self.blocks else {
            return;
        };
        let bookmarked: Vec<usize> = blocks
            .blocks()
            .iter()
            .enumerate()
            .filter(|(_, block)| block.bookmarked)
            .map(|(index, _)| index)
            .collect();
        let from = self.selected_blocks.tail();
        let next = if forward {
            bookmarked
                .iter()
                .copied()
                .find(|index| from.is_none_or(|from| *index > from))
        } else {
            bookmarked
                .iter()
                .rev()
                .copied()
                .find(|index| from.is_none_or(|from| *index < from))
        };
        if let Some(index) = next {
            self.selected_blocks.select(index);
            self.block_list.scroll_to(block_view::block_start(index));
            cx.notify();
        }
    }

    /// Scroll so the last rows of the block at `index` sit at the bottom
    /// of the list.
    fn scroll_to_block_bottom(&mut self, index: usize) {
        let viewport = self.block_list.viewport_bounds().size.height;
        let height = self
            .block_list
            .bounds_for_item(index)
            .map(|bounds| bounds.size.height)
            .unwrap_or(viewport);
        self.block_list.scroll_to(gpui::ListOffset {
            item_ix: index,
            offset_in_item: (height - viewport).max(px(0.)),
        });
    }

    /// Filter the output of block `id` (at `index`), or stop filtering it.
    fn toggle_block_filter(
        &mut self,
        id: u64,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .block_filter
            .as_ref()
            .is_some_and(|filter| filter.block == id)
        {
            self.close_block_filter(window, cx);
            return;
        }
        let input = cx.new(|cx| TextInput::new("Filter output", cx));
        let subscription =
            cx.subscribe_in(
                &input,
                window,
                move |view, _, event, window, cx| match event {
                    TextInputEvent::Changed => {
                        view.block_selection = None;
                        view.block_list.remeasure_items(index..index + 1);
                        view.refresh_matches(cx);
                        cx.notify();
                    }
                    TextInputEvent::Confirmed => {}
                    TextInputEvent::Cancelled => view.close_block_filter(window, cx),
                },
            );
        window.focus(&input.focus_handle(cx), cx);
        self.block_selection = None;
        self.block_filter = Some(BlockFilter {
            block: id,
            input,
            regex: false,
            case_sensitive: false,
            invert: false,
            context: 0,
            cache: RefCell::new(None),
            _subscription: subscription,
        });
        self.block_list.remeasure_items(index..index + 1);
    }

    fn close_block_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(filter) = self.block_filter.take() else {
            return;
        };
        self.block_selection = None;
        if let Some(index) = self.block_index(filter.block) {
            self.block_list.remeasure_items(index..index + 1);
        }
        window.focus(&self.editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn block_index(&self, id: u64) -> Option<usize> {
        self.blocks
            .as_ref()?
            .blocks()
            .iter()
            .position(|block| block.id == id)
    }

    /// The filter's query for block `id`, if it is filtered by one.
    fn filter_query(&self, id: u64, cx: &App) -> Option<String> {
        self.filter_of(id)
            .map(|filter| filter.input.read(cx).text().to_string())
            .filter(|query| !query.is_empty())
    }

    pub(super) fn filter_of(&self, id: u64) -> Option<&BlockFilter> {
        self.block_filter
            .as_ref()
            .filter(|filter| filter.block == id)
    }

    /// A block's rows as shown: all of them, or those its filter keeps.
    pub(super) fn shown_rows(&self, block: &crate::blocks::Block, cx: &App) -> Rows {
        let rows = block.rows();
        match self.filter_of(block.id) {
            Some(filter) => filter.apply(&rows, cx).rows,
            None => rows,
        }
    }

    /// The text of the block at `index` as shown, one line per row.
    fn block_output_text(&self, index: usize, cx: &App) -> String {
        self.block_row_texts(index, cx)
            .iter()
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Matches of `query` in the shown rows of block `scope`, or of every
    /// block, ignoring case.
    pub(super) fn block_matches(
        &self,
        query: &str,
        scope: Option<u64>,
        cx: &App,
    ) -> Vec<(usize, RowMatch)> {
        let Some(blocks) = &self.blocks else {
            return Vec::new();
        };
        let query = query.to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        let mut matches = Vec::new();
        for (index, block) in blocks.blocks().iter().enumerate() {
            if scope.is_some_and(|scope| scope != block.id) {
                continue;
            }
            for (row, text) in self.block_row_texts(index, cx).iter().enumerate() {
                let lower = text.to_lowercase();
                for (start, found) in lower.match_indices(&query) {
                    let start_col = editor_buffer::columns(&lower[..start]);
                    matches.push((
                        index,
                        RowMatch {
                            row,
                            start: start_col,
                            end: start_col + editor_buffer::columns(found),
                            current: false,
                        },
                    ));
                }
            }
        }
        matches
    }

    /// Scroll the current block match into view.
    pub(super) fn reveal_block_match(&mut self, cx: &mut Context<Self>) {
        let Some(search) = &self.search else {
            return;
        };
        let Some((index, found)) = search
            .blocks
            .as_ref()
            .and_then(|blocks| blocks.matches.get(search.current))
            .copied()
        else {
            return;
        };
        let Some(metrics) = self.metrics else {
            return;
        };
        let has_header = self
            .blocks
            .as_ref()
            .and_then(|blocks| blocks.blocks().get(index))
            .is_some_and(|block| !block.command.is_empty());
        let top = if has_header {
            block_view::output_top(metrics)
        } else {
            metrics.height * 0.5
        };
        // Leave a few rows above the match for context.
        let offset = top + metrics.height * found.row.saturating_sub(3) as f32;
        self.block_list.scroll_to(gpui::ListOffset {
            item_ix: index,
            offset_in_item: offset,
        });
        cx.notify();
    }

    /// The menu of the block at `index`, at `position`.
    pub(super) fn render_block_menu(
        &self,
        index: usize,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let block = self.blocks.as_ref()?.blocks().get(index)?;
        let theme = cx.theme().clone();
        let filtered = self.filter_query(block.id, cx).is_some();
        let has_command = !block.command.is_empty();
        let has_cwd = block.context.cwd.is_some();
        let has_branch = block.context.git_branch.is_some();
        let bookmarked = block.bookmarked;
        let collapsed = block.collapsed;
        let running = block.is_running();
        // For several selected blocks, the menu copies and scrolls them all
        // and leaves out what only makes sense for one.
        let several = self.selected_blocks.contains(index) && self.selected_blocks.len() > 1;
        let single = !several;
        let item = |id: &'static str,
                    icon_name: &'static str,
                    label: &'static str,
                    shortcut: Option<&'static str>,
                    action: BlockAction| {
            components::menu_item(id, icon_name, label, &theme)
                .children(shortcut.map(|shortcut| {
                    div()
                        .flex_1()
                        .flex()
                        .justify_end()
                        .pl(px(16.))
                        .child(keybinding(shortcut, &theme))
                }))
                .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                    view.block_action(action, index, window, cx);
                }))
        };
        let copy_output_label = if filtered {
            "Copy filtered output"
        } else {
            "Copy output"
        };
        let menu = components::menu_surface(&theme)
            .id("block-menu")
            .occlude()
            .min_w(px(280.))
            // Clicks in the menu are its own, not a click on the blocks.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|view, _, _, cx| {
                view.block_menu = None;
                cx.notify();
            }))
            .child(item(
                "block-copy",
                "copy",
                "Copy",
                Some("⌘C"),
                BlockAction::CopyAll,
            ))
            .when(has_command || several, |menu| {
                menu.child(item(
                    "block-copy-command",
                    "terminal",
                    if several {
                        "Copy commands"
                    } else {
                        "Copy command"
                    },
                    Some("⌘⇧C"),
                    BlockAction::CopyCommand,
                ))
            })
            .child(item(
                "block-copy-output",
                "copy",
                if several {
                    "Copy outputs"
                } else {
                    copy_output_label
                },
                Some("⌥⌘⇧C"),
                BlockAction::CopyOutput,
            ))
            .when(has_cwd && single, |menu| {
                menu.child(item(
                    "block-copy-cwd",
                    "folder",
                    "Copy working directory",
                    None,
                    BlockAction::CopyDirectory,
                ))
            })
            .when(has_branch && single, |menu| {
                menu.child(item(
                    "block-copy-branch",
                    "git-branch",
                    "Copy git branch",
                    None,
                    BlockAction::CopyBranch,
                ))
            })
            .child(components::menu_separator(&theme))
            .when(single, |menu| {
                menu.child(item(
                    "block-find",
                    "search",
                    "Find within block",
                    Some("⌘⇧F"),
                    BlockAction::FindInBlock,
                ))
                .child(item(
                    "block-filter",
                    "list-filter",
                    "Toggle block filter",
                    Some("⌥⇧F"),
                    BlockAction::ToggleFilter,
                ))
            })
            .child(item(
                "block-bookmark",
                "bookmark",
                if bookmarked {
                    "Remove bookmark"
                } else {
                    "Bookmark"
                },
                Some("⌘⇧B"),
                BlockAction::ToggleBookmark,
            ))
            .child(components::menu_separator(&theme))
            .child(item(
                "block-top",
                "arrow-up-to-line",
                if several {
                    "Scroll to top of blocks"
                } else {
                    "Scroll to top of block"
                },
                Some("⌘⇧↑"),
                BlockAction::ScrollToTop,
            ))
            .child(item(
                "block-bottom",
                "arrow-down-to-line",
                if several {
                    "Scroll to bottom of blocks"
                } else {
                    "Scroll to bottom of block"
                },
                Some("⌘⇧↓"),
                BlockAction::ScrollToBottom,
            ))
            .when(has_command && single, |menu| {
                menu.child(components::menu_separator(&theme))
                    .when(!running, |menu| {
                        menu.child(item(
                            "block-rerun",
                            "rotate-ccw",
                            "Run again",
                            None,
                            BlockAction::Rerun,
                        ))
                    })
                    .child(item(
                        "block-collapse",
                        if collapsed {
                            "chevron-right"
                        } else {
                            "chevron-down"
                        },
                        if collapsed { "Expand" } else { "Collapse" },
                        None,
                        BlockAction::ToggleCollapsed,
                    ))
            });
        Some(
            deferred(anchored().position(position).snap_to_window().child(menu))
                .with_priority(1)
                .into_any_element(),
        )
    }

    /// Select the block before (or after) the last selected one, or with
    /// none selected, the newest, and bring its top into view. Past the
    /// newest block, the newest is scrolled fully into view, and once it
    /// is, `to_input` hands the keys back to the input.
    pub(super) fn select_adjacent_block(
        &mut self,
        forward: bool,
        to_input: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(count) = self.blocks.as_ref().map(|blocks| blocks.blocks().len()) else {
            return;
        };
        let next = match (self.selected_blocks.tail(), forward) {
            (None, false) => count.checked_sub(1),
            (None, true) => None,
            (Some(index), false) => Some(index.saturating_sub(1)),
            (Some(index), true) if index + 1 < count => Some(index + 1),
            (Some(index), true) => {
                let viewport = self.block_list.viewport_bounds();
                let fully_visible = self
                    .block_list
                    .bounds_for_item(index)
                    .is_some_and(|bounds| bounds.bottom() <= viewport.bottom());
                if !fully_visible {
                    self.scroll_to_block_bottom(index);
                    cx.notify();
                } else if to_input && self.shows_editor() {
                    self.clear_block_selections(window, cx);
                }
                return;
            }
        };
        if let Some(index) = next {
            self.select_blocks(|selection| selection.select(index), window, cx);
            self.reveal_block(index);
        }
    }
}
