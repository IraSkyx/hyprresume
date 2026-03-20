use crate::models::WindowEntry;

use super::{
    GAP_ROUNDING_BUFFER, IndexedWindow, Rect, bounding_rect, extract_indexed,
    infer_gap_from_geometry, split_bounds,
};

/// Binary split direction in the Dwindle BSP tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    /// Left / Right
    Horizontal,
    /// Top / Bottom
    Vertical,
}

/// A node in the inferred BSP tree, storing window indices.
#[derive(Debug)]
#[allow(dead_code)]
pub enum BspNode {
    Leaf {
        idx: usize,
    },
    Split {
        dir: SplitDir,
        ratio: f64,
        first: Box<Self>,
        second: Box<Self>,
    },
}

/// A single step in the restore plan, executed in order.
#[derive(Debug, Clone)]
pub struct RestoreStep {
    /// Index into the original window list for the window to open.
    pub window_idx: usize,
    /// If Some, focus this previously-opened window index before opening.
    pub focus_idx: Option<usize>,
    /// If Some, send `layoutmsg preselect <dir>` before opening.
    pub preselect: Option<PreselDir>,
}

/// Post-placement correction: focus this window and apply
/// `layoutmsg splitratio <delta>` to set its parent split precisely.
#[derive(Debug, Clone)]
pub struct SplitRatioStep {
    pub focus_window_idx: usize,
    pub ratio: f64,
}

#[derive(Debug, Clone, Copy)]
pub enum PreselDir {
    Right,
    Bottom,
}

impl std::fmt::Display for PreselDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Right => write!(f, "r"),
            Self::Bottom => write!(f, "b"),
        }
    }
}

/// Complete restore plan for one workspace under the dwindle layout.
pub struct DwindlePlan {
    pub steps: Vec<RestoreStep>,
    pub ratio_steps: Vec<SplitRatioStep>,
}

/// Infer a BSP tree from a set of tiled windows based on their geometry.
///
/// The gap tolerance is measured from the actual window positions rather than
/// from a config value, so it adapts to any `gaps_in` / `gaps_out` setting.
///
/// Returns `None` if the windows can't form a valid BSP partition
/// (e.g. overlapping, gaps, or missing geometry).
fn infer_bsp(indexed: &[IndexedWindow], bounds: Rect) -> Option<BspNode> {
    if indexed.is_empty() {
        return None;
    }
    let tolerance = infer_gap_from_geometry(indexed) + GAP_ROUNDING_BUFFER;
    infer_bsp_inner(indexed, bounds, tolerance)
}

fn infer_bsp_inner(indexed: &[IndexedWindow], bounds: Rect, tolerance: i32) -> Option<BspNode> {
    if indexed.is_empty() {
        return None;
    }
    if indexed.len() == 1 {
        return Some(BspNode::Leaf {
            idx: indexed[0].idx,
        });
    }

    try_split(indexed, bounds, tolerance, SplitDir::Horizontal)
        .or_else(|| try_split(indexed, bounds, tolerance, SplitDir::Vertical))
}

fn try_split(
    indexed: &[IndexedWindow],
    bounds: Rect,
    tolerance: i32,
    dir: SplitDir,
) -> Option<BspNode> {
    let horizontal = dir == SplitDir::Horizontal;
    let candidates = super::split_candidates(indexed, horizontal, tolerance);

    for &split_at in &candidates {
        if let Some(node) = try_split_at(indexed, bounds, tolerance, dir, split_at) {
            return Some(node);
        }
    }
    None
}

fn try_split_at(
    indexed: &[IndexedWindow],
    bounds: Rect,
    tolerance: i32,
    dir: SplitDir,
    split_at: i32,
) -> Option<BspNode> {
    let horizontal = dir == SplitDir::Horizontal;
    let (range_start, range_end) = if horizontal {
        (bounds.x, bounds.x + bounds.w)
    } else {
        (bounds.y, bounds.y + bounds.h)
    };

    if split_at <= range_start || split_at >= range_end {
        return None;
    }

    let mut first_group: Vec<&IndexedWindow> = Vec::new();
    let mut second_group: Vec<&IndexedWindow> = Vec::new();

    for iw in indexed {
        let (start, end) = if horizontal {
            (iw.x, iw.x + iw.w)
        } else {
            (iw.y, iw.y + iw.h)
        };

        if end <= split_at + tolerance {
            first_group.push(iw);
        } else if start >= split_at - tolerance {
            second_group.push(iw);
        } else {
            return None;
        }
    }

    if first_group.is_empty() || second_group.is_empty() {
        return None;
    }

    let (first_bounds, second_bounds) = split_bounds(bounds, horizontal, split_at);

    let first_owned: Vec<IndexedWindow> = first_group.into_iter().cloned().collect();
    let second_owned: Vec<IndexedWindow> = second_group.into_iter().cloned().collect();

    let first_node = infer_bsp_inner(&first_owned, first_bounds, tolerance)?;
    let second_node = infer_bsp_inner(&second_owned, second_bounds, tolerance)?;

    let total = if horizontal {
        f64::from(bounds.w)
    } else {
        f64::from(bounds.h)
    };
    let first_size = if horizontal {
        f64::from(first_bounds.w)
    } else {
        f64::from(first_bounds.h)
    };

    Some(BspNode::Split {
        dir,
        ratio: first_size / total,
        first: Box::new(first_node),
        second: Box::new(second_node),
    })
}

fn plan_from_bsp(tree: &BspNode) -> Vec<RestoreStep> {
    let mut steps = Vec::new();
    walk_bsp(tree, None, None, &mut steps);
    steps
}

/// Walk the BSP tree and emit restore steps in an order compatible with
/// Hyprland's dwindle layout. In dwindle, a new window always splits the
/// *focused window's cell*, so each level's split must be created before
/// any deeper splits within its children.
///
/// For each Split node:
/// 1. Open first child's leftmost leaf (inherits parent's focus/preselect)
/// 2. Open second child's leftmost leaf (creates THIS level's split)
/// 3. Fill remaining leaves of first child (deeper splits)
/// 4. Fill remaining leaves of second child (deeper splits)
fn walk_bsp(
    node: &BspNode,
    focus_idx: Option<usize>,
    preselect: Option<PreselDir>,
    steps: &mut Vec<RestoreStep>,
) {
    match node {
        BspNode::Leaf { idx } => {
            steps.push(RestoreStep {
                window_idx: *idx,
                focus_idx,
                preselect,
            });
        }
        BspNode::Split {
            dir, first, second, ..
        } => {
            let first_leaf = leftmost_leaf_idx(first);
            let second_leaf = leftmost_leaf_idx(second);
            let presel = match dir {
                SplitDir::Horizontal => PreselDir::Right,
                SplitDir::Vertical => PreselDir::Bottom,
            };

            steps.push(RestoreStep {
                window_idx: first_leaf,
                focus_idx,
                preselect,
            });
            steps.push(RestoreStep {
                window_idx: second_leaf,
                focus_idx: Some(first_leaf),
                preselect: Some(presel),
            });

            walk_remaining(first, steps);
            walk_remaining(second, steps);
        }
    }
}

/// Emit steps for all leaves of a subtree except its leftmost leaf (which
/// was already opened by the parent's `walk_bsp`). Each split in the subtree
/// is created by opening the second child's leftmost leaf while focusing the
/// first child's leftmost leaf.
fn walk_remaining(node: &BspNode, steps: &mut Vec<RestoreStep>) {
    if let BspNode::Split {
        dir, first, second, ..
    } = node
    {
        let first_leaf = leftmost_leaf_idx(first);
        let second_leaf = leftmost_leaf_idx(second);
        let presel = match dir {
            SplitDir::Horizontal => PreselDir::Right,
            SplitDir::Vertical => PreselDir::Bottom,
        };

        steps.push(RestoreStep {
            window_idx: second_leaf,
            focus_idx: Some(first_leaf),
            preselect: Some(presel),
        });

        walk_remaining(first, steps);
        walk_remaining(second, steps);
    }
}

fn leftmost_leaf_idx(node: &BspNode) -> usize {
    match node {
        BspNode::Leaf { idx } => *idx,
        BspNode::Split { first, .. } => leftmost_leaf_idx(first),
    }
}

/// Collect `SplitRatioStep`s for every split node that has at least one
/// direct Leaf child.
fn collect_splitratio_steps(tree: &BspNode) -> Vec<SplitRatioStep> {
    let mut steps = Vec::new();
    collect_ratios_inner(tree, &mut steps);
    steps
}

fn collect_ratios_inner(node: &BspNode, steps: &mut Vec<SplitRatioStep>) {
    if let BspNode::Split {
        ratio,
        first,
        second,
        ..
    } = node
    {
        if let BspNode::Leaf { idx } = first.as_ref() {
            steps.push(SplitRatioStep {
                focus_window_idx: *idx,
                ratio: *ratio,
            });
        } else if let BspNode::Leaf { idx } = second.as_ref() {
            steps.push(SplitRatioStep {
                focus_window_idx: *idx,
                ratio: *ratio,
            });
        }

        collect_ratios_inner(first, steps);
        collect_ratios_inner(second, steps);
    }
}

/// Build a dwindle restore plan for tiled windows on a single workspace.
///
/// `global_indices` maps each window in `windows` to its index in the full
/// session window list.
///
/// Returns `None` if BSP inference fails.
pub fn build_workspace_plan(
    windows: &[&WindowEntry],
    global_indices: &[usize],
) -> Option<DwindlePlan> {
    let bounds = bounding_rect(windows)?;
    let indexed = extract_indexed(windows, global_indices)?;
    let tree = infer_bsp(&indexed, bounds)?;
    let steps = plan_from_bsp(&tree);
    let ratio_steps = collect_splitratio_steps(&tree);
    Some(DwindlePlan { steps, ratio_steps })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(app_id: &str, ws: &str, x: i32, y: i32, w: i32, h: i32) -> WindowEntry {
        WindowEntry {
            app_id: app_id.to_string(),
            launch_cmd: app_id.to_string(),
            workspace: ws.to_string(),
            monitor: None,
            floating: false,
            fullscreen: false,
            position: Some((x, y)),
            size: Some((w, h)),
            cwd: None,
            profile: None,
        }
    }

    #[test]
    fn single_window_is_leaf() {
        let w = make_entry("firefox", "1", 0, 0, 1920, 1080);
        let refs = vec![&w];
        let bounds = bounding_rect(&refs).unwrap();
        let indexed = extract_indexed(&refs, &[0]).unwrap();
        let tree = infer_bsp(&indexed, bounds).unwrap();
        assert!(matches!(tree, BspNode::Leaf { .. }));
    }

    #[test]
    fn two_windows_horizontal_split() {
        let a = make_entry("firefox", "1", 0, 0, 960, 1080);
        let b = make_entry("code", "1", 960, 0, 960, 1080);
        let refs = vec![&a, &b];
        let bounds = bounding_rect(&refs).unwrap();
        let indexed = extract_indexed(&refs, &[0, 1]).unwrap();
        let tree = infer_bsp(&indexed, bounds).unwrap();
        match &tree {
            BspNode::Split { dir, ratio, .. } => {
                assert_eq!(*dir, SplitDir::Horizontal);
                assert!((ratio - 0.5).abs() < 0.01);
            }
            BspNode::Leaf { .. } => panic!("expected split"),
        }
    }

    #[test]
    fn two_windows_vertical_split() {
        let a = make_entry("firefox", "1", 0, 0, 1920, 540);
        let b = make_entry("code", "1", 0, 540, 1920, 540);
        let refs = vec![&a, &b];
        let bounds = bounding_rect(&refs).unwrap();
        let indexed = extract_indexed(&refs, &[0, 1]).unwrap();
        let tree = infer_bsp(&indexed, bounds).unwrap();
        match &tree {
            BspNode::Split { dir, ratio, .. } => {
                assert_eq!(*dir, SplitDir::Vertical);
                assert!((ratio - 0.5).abs() < 0.01);
            }
            BspNode::Leaf { .. } => panic!("expected split"),
        }
    }

    #[test]
    fn three_windows_nested() {
        //  +-------+-------+
        //  |       |   B   |
        //  |   A   +-------+
        //  |       |   C   |
        //  +-------+-------+
        let a = make_entry("a", "1", 0, 0, 960, 1080);
        let b = make_entry("b", "1", 960, 0, 960, 540);
        let c = make_entry("c", "1", 960, 540, 960, 540);
        let refs = vec![&a, &b, &c];
        let bounds = bounding_rect(&refs).unwrap();
        let indexed = extract_indexed(&refs, &[0, 1, 2]).unwrap();
        let tree = infer_bsp(&indexed, bounds).unwrap();
        match &tree {
            BspNode::Split {
                dir: SplitDir::Horizontal,
                first,
                second,
                ..
            } => {
                assert!(matches!(first.as_ref(), BspNode::Leaf { .. }));
                assert!(matches!(
                    second.as_ref(),
                    BspNode::Split {
                        dir: SplitDir::Vertical,
                        ..
                    }
                ));
            }
            other => panic!("expected horizontal split at root, got {other:?}"),
        }
    }

    #[test]
    fn plan_right_heavy_tree() {
        // A (left) | B (top-right) / C (bottom-right)
        let a = make_entry("a", "1", 0, 0, 960, 1080);
        let b = make_entry("b", "1", 960, 0, 960, 540);
        let c = make_entry("c", "1", 960, 540, 960, 540);
        let refs: Vec<&WindowEntry> = vec![&a, &b, &c];
        let wp = build_workspace_plan(&refs, &[0, 1, 2]).unwrap();
        assert_eq!(wp.steps.len(), 3);

        // Order: A (full), B (right of A → root H split), C (below B → nested V split)
        assert_eq!(wp.steps[0].window_idx, 0);
        assert!(wp.steps[0].focus_idx.is_none());
        assert_eq!(wp.steps[1].window_idx, 1);
        assert_eq!(wp.steps[1].focus_idx, Some(0));
        assert!(matches!(wp.steps[1].preselect, Some(PreselDir::Right)));
        assert_eq!(wp.steps[2].window_idx, 2);
        assert_eq!(wp.steps[2].focus_idx, Some(1));
        assert!(matches!(wp.steps[2].preselect, Some(PreselDir::Bottom)));
    }

    #[test]
    fn plan_left_heavy_tree() {
        // A (top-left) / B (bottom-left) | C (right, full height)
        let a = make_entry("a", "1", 0, 0, 400, 540);
        let b = make_entry("b", "1", 0, 540, 400, 540);
        let c = make_entry("c", "1", 400, 0, 560, 1080);
        let refs: Vec<&WindowEntry> = vec![&a, &b, &c];
        let wp = build_workspace_plan(&refs, &[0, 1, 2]).unwrap();
        assert_eq!(wp.steps.len(), 3);

        // Order: A (full), C (right of A → root H split), B (below A → nested V split)
        assert_eq!(wp.steps[0].window_idx, 0);
        assert_eq!(wp.steps[1].window_idx, 2);
        assert!(matches!(wp.steps[1].preselect, Some(PreselDir::Right)));
        assert_eq!(wp.steps[2].window_idx, 1);
        assert!(matches!(wp.steps[2].preselect, Some(PreselDir::Bottom)));
    }

    #[test]
    fn two_windows_with_gap_horizontal() {
        // 10px gap between windows (simulating gaps_in = 5)
        let a = make_entry("a", "1", 0, 0, 955, 1080);
        let b = make_entry("b", "1", 965, 0, 955, 1080);
        let refs = vec![&a, &b];
        let plan = build_workspace_plan(&refs, &[0, 1]);
        assert!(plan.is_some(), "should handle gaps between windows");
        assert_eq!(plan.unwrap().steps.len(), 2);
    }

    #[test]
    fn three_windows_with_gaps() {
        //  +-------+  gap  +-------+
        //  |       |       |   B   |
        //  |   A   |  gap  +--gap--+
        //  |       |       |   C   |
        //  +-------+       +-------+
        let a = make_entry("a", "1", 5, 5, 950, 1070);
        let b = make_entry("b", "1", 965, 5, 950, 530);
        let c = make_entry("c", "1", 965, 545, 950, 530);
        let refs = vec![&a, &b, &c];
        let plan = build_workspace_plan(&refs, &[0, 1, 2]);
        assert!(plan.is_some(), "should handle gaps in nested splits");
        assert_eq!(plan.unwrap().steps.len(), 3);
    }

    #[test]
    fn multi_monitor_offset_handled() {
        // Windows on a second monitor at x=1920 with gaps
        let a = make_entry("a", "2", 1925, 5, 950, 1070);
        let b = make_entry("b", "2", 2885, 5, 950, 1070);
        let refs = vec![&a, &b];
        let plan = build_workspace_plan(&refs, &[0, 1]);
        assert!(plan.is_some(), "should handle windows on offset monitor");
        assert_eq!(plan.unwrap().steps.len(), 2);
    }

    #[test]
    fn uneven_ratio() {
        let a = make_entry("a", "1", 0, 0, 1152, 1080);
        let b = make_entry("b", "1", 1152, 0, 768, 1080);
        let refs = vec![&a, &b];
        let bounds = bounding_rect(&refs).unwrap();
        let indexed = extract_indexed(&refs, &[0, 1]).unwrap();
        let tree = infer_bsp(&indexed, bounds).unwrap();
        match &tree {
            BspNode::Split { ratio, .. } => assert!((ratio - 0.6).abs() < 0.01),
            BspNode::Leaf { .. } => panic!("expected split"),
        }
    }

    #[test]
    fn ratio_steps_two_windows() {
        let a = make_entry("a", "1", 0, 0, 960, 1080);
        let b = make_entry("b", "1", 960, 0, 960, 1080);
        let refs = vec![&a, &b];
        let wp = build_workspace_plan(&refs, &[0, 1]).unwrap();
        assert_eq!(wp.ratio_steps.len(), 1);
        assert_eq!(wp.ratio_steps[0].focus_window_idx, 0);
        assert!((wp.ratio_steps[0].ratio - 0.5).abs() < 0.01);
    }

    #[test]
    fn ratio_steps_three_windows_right_heavy() {
        let a = make_entry("a", "1", 0, 0, 960, 1080);
        let b = make_entry("b", "1", 960, 0, 960, 540);
        let c = make_entry("c", "1", 960, 540, 960, 540);
        let refs = vec![&a, &b, &c];
        let wp = build_workspace_plan(&refs, &[0, 1, 2]).unwrap();
        assert_eq!(wp.ratio_steps.len(), 2);
    }

    #[test]
    fn ratio_steps_uneven_ratio_preserved() {
        let a = make_entry("a", "1", 0, 0, 1152, 1080);
        let b = make_entry("b", "1", 1152, 0, 768, 1080);
        let refs = vec![&a, &b];
        let wp = build_workspace_plan(&refs, &[0, 1]).unwrap();
        assert_eq!(wp.ratio_steps.len(), 1);
        assert!((wp.ratio_steps[0].ratio - 0.6).abs() < 0.01);
    }

    #[test]
    fn four_windows_nested_with_large_gap_between_non_adjacent() {
        //  +--------+----------+------+
        //  |        |          |  c   |
        //  |   a    |    d     |      |
        //  |        |          +------+
        //  |        |          |  b   |
        //  +--------+----------+------+
        let a = make_entry("a", "2", 21, 69, 1112, 1350);
        let b = make_entry("b", "2", 2062, 1124, 477, 295);
        let c = make_entry("c", "2", 2062, 69, 477, 1033);
        let d = make_entry("d", "2", 1155, 69, 885, 1350);
        let refs: Vec<&WindowEntry> = vec![&a, &b, &c, &d];
        let plan = build_workspace_plan(&refs, &[0, 1, 2, 3]);
        assert!(
            plan.is_some(),
            "BSP inference should succeed for 4-window dwindle layout with gaps"
        );
        assert_eq!(plan.unwrap().steps.len(), 4);
    }
}
