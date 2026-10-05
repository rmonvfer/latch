//! The arrangement of panes inside a tab: a tree of splits whose leaves are
//! panes. Generic over the leaf so the layout logic is testable on its own.

/// Direction in which a split lays out its children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    /// Children side by side, divided by vertical lines.
    Horizontal,
    /// Children stacked, divided by horizontal lines.
    Vertical,
}

/// The smallest share of a split one child may shrink to.
pub const MIN_RATIO: f32 = 0.08;

#[derive(Clone, Debug, PartialEq)]
pub enum PaneNode<T> {
    Leaf(T),
    Split(Split<T>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Split<T> {
    /// Stable identity for UI state such as measured bounds.
    pub id: usize,
    pub axis: Axis,
    pub children: Vec<PaneNode<T>>,
    /// Each child's share of the split; always sums to 1.
    pub ratios: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PaneTree<T> {
    root: PaneNode<T>,
    next_split_id: usize,
}

impl<T: Clone + PartialEq> PaneTree<T> {
    pub fn new(leaf: T) -> Self {
        Self {
            root: PaneNode::Leaf(leaf),
            next_split_id: 0,
        }
    }

    /// Build a tree from a restored root, renumbering its splits.
    pub fn from_root(root: PaneNode<T>) -> Self {
        let mut tree = Self {
            root,
            next_split_id: 0,
        };
        let mut next = 0;
        renumber(&mut tree.root, &mut next);
        tree.next_split_id = next;
        tree
    }

    pub fn root(&self) -> &PaneNode<T> {
        &self.root
    }

    /// Leaves in reading order (left to right, top to bottom).
    pub fn leaves(&self) -> Vec<T> {
        let mut leaves = Vec::new();
        collect_leaves(&self.root, &mut leaves);
        leaves
    }

    /// Place `new` after `target` along `axis`, sharing `target`'s space.
    pub fn split(&mut self, target: &T, new: T, axis: Axis) -> bool {
        self.insert(target, PaneNode::Leaf(new), axis, false)
    }

    /// Place `node` (a pane or a whole arrangement) beside `target` along
    /// `axis`, before or after it, sharing `target`'s space.
    pub fn insert(&mut self, target: &T, node: PaneNode<T>, axis: Axis, before: bool) -> bool {
        let mut node = Some(node);
        if !insert_node(&mut self.root, target, &mut node, axis, before) {
            return false;
        }
        collapse(&mut self.root);
        let mut next = 0;
        renumber(&mut self.root, &mut next);
        self.next_split_id = next;
        true
    }

    /// Remove a leaf. Its space goes to its neighbours, and a split left with
    /// one child is replaced by that child. Removing the last leaf fails.
    pub fn remove(&mut self, target: &T) -> bool {
        if matches!(&self.root, PaneNode::Leaf(leaf) if leaf == target) {
            return false;
        }
        let removed = remove_node(&mut self.root, target);
        if removed {
            collapse(&mut self.root);
        }
        removed
    }

    /// Move the boundary after child `index` of split `split_id` so that it
    /// sits at `position`, a fraction (0..1) of the split's length.
    pub fn resize(&mut self, split_id: usize, index: usize, position: f32) {
        if let Some(split) = find_split(&mut self.root, split_id) {
            if index + 1 >= split.ratios.len() {
                return;
            }
            let before: f32 = split.ratios[..index].iter().sum();
            let pair = split.ratios[index] + split.ratios[index + 1];
            let first = (position - before).clamp(MIN_RATIO, pair - MIN_RATIO);
            split.ratios[index] = first;
            split.ratios[index + 1] = pair - first;
        }
    }

    /// Give every child of every split an equal share.
    pub fn equalize(&mut self) {
        equalize_node(&mut self.root);
    }
}

fn renumber<T>(node: &mut PaneNode<T>, next: &mut usize) {
    if let PaneNode::Split(split) = node {
        split.id = *next;
        *next += 1;
        for child in &mut split.children {
            renumber(child, next);
        }
    }
}

fn collect_leaves<T: Clone>(node: &PaneNode<T>, leaves: &mut Vec<T>) {
    match node {
        PaneNode::Leaf(leaf) => leaves.push(leaf.clone()),
        PaneNode::Split(split) => {
            for child in &split.children {
                collect_leaves(child, leaves);
            }
        }
    }
}

fn insert_node<T: Clone + PartialEq>(
    node: &mut PaneNode<T>,
    target: &T,
    new: &mut Option<PaneNode<T>>,
    axis: Axis,
    before: bool,
) -> bool {
    match node {
        PaneNode::Leaf(leaf) if leaf == target => {
            let existing = PaneNode::Leaf(leaf.clone());
            let Some(new) = new.take() else {
                return false;
            };
            let children = if before {
                vec![new, existing]
            } else {
                vec![existing, new]
            };
            *node = PaneNode::Split(Split {
                id: 0,
                axis,
                children,
                ratios: vec![0.5, 0.5],
            });
            true
        }
        PaneNode::Leaf(_) => false,
        PaneNode::Split(split) => {
            // Inserting along the parent's own axis adds a sibling instead
            // of nesting another split.
            if split.axis == axis
                && let Some(index) = split
                    .children
                    .iter()
                    .position(|child| matches!(child, PaneNode::Leaf(leaf) if leaf == target))
                && let Some(new) = new.take()
            {
                let half = split.ratios[index] / 2.;
                split.ratios[index] = half;
                let at = if before { index } else { index + 1 };
                split.ratios.insert(at, half);
                split.children.insert(at, new);
                return true;
            }
            split
                .children
                .iter_mut()
                .any(|child| insert_node(child, target, new, axis, before))
        }
    }
}

fn remove_node<T: PartialEq>(node: &mut PaneNode<T>, target: &T) -> bool {
    let PaneNode::Split(split) = node else {
        return false;
    };
    if let Some(index) = split
        .children
        .iter()
        .position(|child| matches!(child, PaneNode::Leaf(leaf) if leaf == target))
    {
        let freed = split.ratios.remove(index);
        split.children.remove(index);
        let remaining: f32 = split.ratios.iter().sum();
        for ratio in &mut split.ratios {
            *ratio += freed * (*ratio / remaining);
        }
        return true;
    }
    split
        .children
        .iter_mut()
        .any(|child| remove_node(child, target))
}

/// Replace single-child splits with their child, and merge a child split
/// into its parent when both run along the same axis.
fn collapse<T>(node: &mut PaneNode<T>) {
    if let PaneNode::Split(split) = node {
        for child in &mut split.children {
            collapse(child);
        }
        let mut children = Vec::new();
        let mut ratios = Vec::new();
        for (child, ratio) in split.children.drain(..).zip(split.ratios.drain(..)) {
            match child {
                PaneNode::Split(inner) if inner.axis == split.axis => {
                    for (grandchild, inner_ratio) in inner.children.into_iter().zip(inner.ratios) {
                        children.push(grandchild);
                        ratios.push(ratio * inner_ratio);
                    }
                }
                other => {
                    children.push(other);
                    ratios.push(ratio);
                }
            }
        }
        split.children = children;
        split.ratios = ratios;
        if split.children.len() == 1 {
            let only = split.children.pop().expect("one child");
            *node = only;
        }
    }
}

fn find_split<T>(node: &mut PaneNode<T>, id: usize) -> Option<&mut Split<T>> {
    let PaneNode::Split(split) = node else {
        return None;
    };
    if split.id == id {
        return Some(split);
    }
    split
        .children
        .iter_mut()
        .find_map(|child| find_split(child, id))
}

fn equalize_node<T>(node: &mut PaneNode<T>) {
    if let PaneNode::Split(split) = node {
        let share = 1. / split.children.len() as f32;
        split.ratios.iter_mut().for_each(|ratio| *ratio = share);
        split.children.iter_mut().for_each(equalize_node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ratios(tree: &PaneTree<u32>) -> Vec<f32> {
        match tree.root() {
            PaneNode::Split(split) => split.ratios.clone(),
            PaneNode::Leaf(_) => vec![1.],
        }
    }

    #[test]
    fn splitting_creates_and_extends_splits() {
        let mut tree = PaneTree::new(1);
        assert!(tree.split(&1, 2, Axis::Horizontal));
        assert!(tree.split(&2, 3, Axis::Horizontal));
        assert_eq!(tree.leaves(), vec![1, 2, 3]);
        assert_eq!(ratios(&tree), vec![0.5, 0.25, 0.25]);

        assert!(tree.split(&3, 4, Axis::Vertical));
        assert_eq!(tree.leaves(), vec![1, 2, 3, 4]);
        let PaneNode::Split(root) = tree.root() else {
            panic!("expected split");
        };
        assert!(
            matches!(&root.children[2], PaneNode::Split(inner) if inner.axis == Axis::Vertical)
        );
    }

    #[test]
    fn inserting_before_and_merging_subtrees() {
        let mut tree = PaneTree::new(1);
        tree.split(&1, 2, Axis::Horizontal);
        assert!(tree.insert(&1, PaneNode::Leaf(0), Axis::Horizontal, true));
        assert_eq!(tree.leaves(), vec![0, 1, 2]);

        let mut other = PaneTree::new(7);
        other.split(&7, 8, Axis::Horizontal);
        // A horizontal arrangement dropped beside a pane of a horizontal
        // split flattens into it.
        assert!(tree.insert(&2, other.root().clone(), Axis::Horizontal, false));
        assert_eq!(tree.leaves(), vec![0, 1, 2, 7, 8]);
        let PaneNode::Split(root) = tree.root() else {
            panic!("expected split");
        };
        assert_eq!(root.children.len(), 5);
        let total: f32 = root.ratios.iter().sum();
        assert!((total - 1.).abs() < 1e-5);
    }

    #[test]
    fn splitting_unknown_leaf_fails() {
        let mut tree = PaneTree::new(1);
        assert!(!tree.split(&9, 2, Axis::Vertical));
        assert_eq!(tree.leaves(), vec![1]);
    }

    #[test]
    fn removing_gives_space_to_siblings_and_collapses() {
        let mut tree = PaneTree::new(1);
        tree.split(&1, 2, Axis::Horizontal);
        tree.split(&2, 3, Axis::Horizontal);
        assert!(tree.remove(&1));
        let total: f32 = ratios(&tree).iter().sum();
        assert!((total - 1.).abs() < 1e-5);
        assert!(tree.remove(&2));
        assert_eq!(tree.root(), &PaneNode::Leaf(3));
    }

    #[test]
    fn last_leaf_cannot_be_removed() {
        let mut tree = PaneTree::new(1);
        assert!(!tree.remove(&1));
    }

    #[test]
    fn nested_split_merges_into_parent_with_same_axis() {
        let mut tree = PaneTree::new(1);
        tree.split(&1, 2, Axis::Horizontal);
        tree.split(&2, 3, Axis::Vertical);
        tree.split(&3, 4, Axis::Horizontal);
        // Removing 2 leaves the vertical split with one child (a horizontal
        // split), which then merges into the horizontal root.
        tree.remove(&2);
        assert_eq!(tree.leaves(), vec![1, 3, 4]);
        let PaneNode::Split(root) = tree.root() else {
            panic!("expected split");
        };
        assert_eq!(root.axis, Axis::Horizontal);
        assert_eq!(root.children.len(), 3);
    }

    #[test]
    fn resize_respects_minimum() {
        let mut tree = PaneTree::new(1);
        tree.split(&1, 2, Axis::Horizontal);
        let id = match tree.root() {
            PaneNode::Split(split) => split.id,
            PaneNode::Leaf(_) => unreachable!(),
        };
        tree.resize(id, 0, 0.7);
        assert!((ratios(&tree)[0] - 0.7).abs() < 1e-5);
        tree.resize(id, 0, 0.99);
        assert!((ratios(&tree)[1] - MIN_RATIO).abs() < 1e-5);
        tree.equalize();
        assert_eq!(ratios(&tree), vec![0.5, 0.5]);
    }
}
