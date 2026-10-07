//! Which blocks of a pane are selected, as Warp selects them: a click picks
//! one block, cmd-click adds or removes one, and shift-click or shift-arrows
//! extend a range from the block the selection started at.

use std::collections::BTreeSet;

/// The selected blocks, by index in the block list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectedBlocks {
    blocks: BTreeSet<usize>,
    /// Where a range extends from.
    pivot: usize,
    /// The block most recently selected, which keyboard moves start from.
    tail: usize,
}

/// How a selected block's border is drawn, in pixels per side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelectionBorder {
    pub top: f32,
    pub bottom: f32,
    pub sides: f32,
}

/// A lone selected block's border.
const SINGLE_BORDER: f32 = 2.;
/// With several blocks selected, the tail's border and the others'.
const TAIL_BORDER: f32 = 3.;
const MEMBER_BORDER: f32 = 1.5;

impl SelectedBlocks {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn contains(&self, index: usize) -> bool {
        self.blocks.contains(&index)
    }

    pub fn tail(&self) -> Option<usize> {
        (!self.is_empty()).then_some(self.tail)
    }

    /// The selected indices, top to bottom.
    pub fn indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.blocks.iter().copied()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Select only `index`.
    pub fn select(&mut self, index: usize) {
        self.blocks = BTreeSet::from([index]);
        self.pivot = index;
        self.tail = index;
    }

    /// Add `index` to the selection, or take it out.
    pub fn toggle(&mut self, index: usize) {
        if self.blocks.remove(&index) {
            // The nearest block still selected takes over as the tail.
            let nearest = self
                .blocks
                .range(..index)
                .next_back()
                .or_else(|| self.blocks.range(index..).next())
                .copied();
            match nearest {
                Some(nearest) => {
                    self.pivot = nearest;
                    self.tail = nearest;
                }
                None => self.clear(),
            }
        } else {
            self.blocks.insert(index);
            self.pivot = index;
            self.tail = index;
        }
    }

    /// Select every block from the pivot to `tail`, and nothing else. With
    /// nothing selected, this selects `tail` alone.
    pub fn extend_to(&mut self, tail: usize) {
        if self.is_empty() {
            self.select(tail);
            return;
        }
        let (from, to) = (self.pivot.min(tail), self.pivot.max(tail));
        self.blocks = (from..=to).collect();
        self.tail = tail;
    }

    /// Select blocks `0..count`, with the tail on the newest.
    pub fn select_all(&mut self, count: usize) {
        let Some(last) = count.checked_sub(1) else {
            self.clear();
            return;
        };
        self.blocks = (0..count).collect();
        self.pivot = 0;
        self.tail = last;
    }

    /// Account for the oldest `dropped` blocks leaving the list.
    pub fn shift(&mut self, dropped: usize) {
        if dropped == 0 || self.is_empty() {
            return;
        }
        self.blocks = self
            .blocks
            .iter()
            .filter_map(|index| index.checked_sub(dropped))
            .collect();
        match (self.blocks.first(), self.blocks.last()) {
            (Some(&first), Some(&last)) => {
                self.pivot = self.pivot.checked_sub(dropped).unwrap_or(first);
                self.tail = self.tail.checked_sub(dropped).unwrap_or(last);
            }
            _ => self.clear(),
        }
    }

    /// The border of block `index`, if selected. A run of adjacent selected
    /// blocks reads as one box: only its ends get a top or bottom edge, and
    /// the tail is boxed more heavily.
    pub fn border(&self, index: usize) -> Option<SelectionBorder> {
        if !self.contains(index) {
            return None;
        }
        if self.len() == 1 {
            return Some(SelectionBorder {
                top: SINGLE_BORDER,
                bottom: SINGLE_BORDER,
                sides: SINGLE_BORDER,
            });
        }
        if index == self.tail {
            return Some(SelectionBorder {
                top: TAIL_BORDER,
                bottom: TAIL_BORDER,
                sides: TAIL_BORDER,
            });
        }
        let starts_run = index == 0 || !self.contains(index - 1) || index - 1 == self.tail;
        let ends_run = !self.contains(index + 1) || index + 1 == self.tail;
        Some(SelectionBorder {
            top: if starts_run { MEMBER_BORDER } else { 0. },
            bottom: if ends_run { MEMBER_BORDER } else { 0. },
            sides: MEMBER_BORDER,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selected(selection: &SelectedBlocks) -> Vec<usize> {
        selection.indices().collect()
    }

    #[test]
    fn toggling_adds_and_removes_blocks() {
        let mut selection = SelectedBlocks::default();
        selection.select(2);
        selection.toggle(5);
        assert_eq!(selected(&selection), vec![2, 5]);
        assert_eq!(selection.tail(), Some(5));
        selection.toggle(5);
        assert_eq!(selected(&selection), vec![2]);
        assert_eq!(selection.tail(), Some(2));
        selection.toggle(2);
        assert!(selection.is_empty());
    }

    #[test]
    fn extending_selects_the_range_from_the_pivot() {
        let mut selection = SelectedBlocks::default();
        selection.select(4);
        selection.toggle(9);
        selection.extend_to(6);
        assert_eq!(selected(&selection), vec![6, 7, 8, 9]);
        selection.extend_to(11);
        assert_eq!(selected(&selection), vec![9, 10, 11]);
        assert_eq!(selection.tail(), Some(11));
    }

    #[test]
    fn dropped_blocks_shift_the_selection() {
        let mut selection = SelectedBlocks::default();
        selection.select(1);
        selection.extend_to(4);
        selection.shift(2);
        assert_eq!(selected(&selection), vec![0, 1, 2]);
        assert_eq!(selection.tail(), Some(2));
        selection.shift(5);
        assert!(selection.is_empty());
    }

    #[test]
    fn adjacent_blocks_share_one_border() {
        let mut selection = SelectedBlocks::default();
        selection.select(1);
        assert_eq!(selection.border(1).map(|border| border.top), Some(2.));
        selection.extend_to(4);
        let edges = |index| {
            selection
                .border(index)
                .map(|border| (border.top, border.bottom))
        };
        assert_eq!(edges(1), Some((1.5, 0.)));
        assert_eq!(edges(2), Some((0., 0.)));
        assert_eq!(edges(3), Some((0., 1.5)));
        assert_eq!(edges(4), Some((3., 3.)));
        assert_eq!(edges(5), None);
    }
}
