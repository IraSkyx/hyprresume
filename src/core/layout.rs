use crate::models::WindowEntry;

/// Small buffer added to the measured gap to account for border widths and sub-pixel rounding.
const GAP_ROUNDING_BUFFER: i32 = 4;

/// A rectangle in screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

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

/// An indexed window: pairs a global index with position/size data.
#[derive(Clone)]
struct IndexedWindow {
    idx: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

fn extract_indexed(
    windows: &[&WindowEntry],
    global_indices: &[usize],
) -> Option<Vec<IndexedWindow>> {
    windows
        .iter()
        .zip(global_indices)
        .map(|(w, &idx)| {
            let (x, y) = w.position?;
            let (ww, hh) = w.size?;
            Some(IndexedWindow {
                idx,
                x,
                y,
                w: ww,
                h: hh,
            })
        })
        .collect()
}

/// Measure the largest pixel gap between adjacent tiled windows by examining
/// pairs that overlap in the perpendicular axis. Returns 0 when no gaps are
/// found (e.g. `gaps_in = 0` or a single window).
fn infer_gap_from_geometry(indexed: &[IndexedWindow]) -> i32 {
    if indexed.len() < 2 {
        return 0;
    }

    let mut max_gap = 0i32;
    for a in indexed {
        for b in indexed {
            // b is to the right of a, and they share vertical overlap
            let h_gap = b.x - (a.x + a.w);
            if h_gap > 0 && ranges_overlap(a.y, a.y + a.h, b.y, b.y + b.h) {
                max_gap = max_gap.max(h_gap);
            }

            // b is below a, and they share horizontal overlap
            let v_gap = b.y - (a.y + a.h);
            if v_gap > 0 && ranges_overlap(a.x, a.x + a.w, b.x, b.x + b.w) {
                max_gap = max_gap.max(v_gap);
            }
        }
    }
    max_gap
}

const fn ranges_overlap(a_start: i32, a_end: i32, b_start: i32, b_end: i32) -> bool {
    a_start < b_end && b_start < a_end
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
    let mut edges: Vec<i32> = Vec::new();
    for iw in indexed {
        match dir {
            SplitDir::Horizontal => {
                edges.push(iw.x);
                edges.push(iw.x + iw.w);
            }
            SplitDir::Vertical => {
                edges.push(iw.y);
                edges.push(iw.y + iw.h);
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();

    // Try both raw edges and gap midpoints as split candidates.
    // Gap midpoints handle Hyprland's gaps_in where the true split line
    // falls between the end of one window and the start of the next.
    let mut candidates = edges.clone();
    for pair in edges.windows(2) {
        let gap = pair[1] - pair[0];
        if gap > 0 && gap <= tolerance {
            candidates.push(pair[0] + gap / 2);
        }
    }
    candidates.sort_unstable();
    candidates.dedup();

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
    let (range_start, range_end) = match dir {
        SplitDir::Horizontal => (bounds.x, bounds.x + bounds.w),
        SplitDir::Vertical => (bounds.y, bounds.y + bounds.h),
    };

    if split_at <= range_start || split_at >= range_end {
        return None;
    }

    let mut first_group: Vec<&IndexedWindow> = Vec::new();
    let mut second_group: Vec<&IndexedWindow> = Vec::new();

    for iw in indexed {
        let (start, end) = match dir {
            SplitDir::Horizontal => (iw.x, iw.x + iw.w),
            SplitDir::Vertical => (iw.y, iw.y + iw.h),
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

    let (first_bounds, second_bounds) = split_bounds(bounds, dir, split_at);

    let first_owned: Vec<IndexedWindow> = first_group.iter().map(|iw| (**iw).clone()).collect();
    let second_owned: Vec<IndexedWindow> = second_group.iter().map(|iw| (**iw).clone()).collect();

    let first_node = infer_bsp_inner(&first_owned, first_bounds, tolerance)?;
    let second_node = infer_bsp_inner(&second_owned, second_bounds, tolerance)?;

    let total = match dir {
        SplitDir::Horizontal => f64::from(bounds.w),
        SplitDir::Vertical => f64::from(bounds.h),
    };
    let first_size = match dir {
        SplitDir::Horizontal => f64::from(first_bounds.w),
        SplitDir::Vertical => f64::from(first_bounds.h),
    };

    Some(BspNode::Split {
        dir,
        ratio: first_size / total,
        first: Box::new(first_node),
        second: Box::new(second_node),
    })
}

const fn split_bounds(bounds: Rect, dir: SplitDir, split_at: i32) -> (Rect, Rect) {
    match dir {
        SplitDir::Horizontal => (
            Rect {
                x: bounds.x,
                y: bounds.y,
                w: split_at - bounds.x,
                h: bounds.h,
            },
            Rect {
                x: split_at,
                y: bounds.y,
                w: bounds.x + bounds.w - split_at,
                h: bounds.h,
            },
        ),
        SplitDir::Vertical => (
            Rect {
                x: bounds.x,
                y: bounds.y,
                w: bounds.w,
                h: split_at - bounds.y,
            },
            Rect {
                x: bounds.x,
                y: split_at,
                w: bounds.w,
                h: bounds.y + bounds.h - split_at,
            },
        ),
    }
}

/// Compute the bounding rectangle of a set of windows.
pub fn bounding_rect(windows: &[&WindowEntry]) -> Option<Rect> {
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;

    for w in windows {
        let (x, y) = w.position?;
        let (ww, hh) = w.size?;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + ww);
        max_y = max_y.max(y + hh);
    }

    if min_x >= max_x || min_y >= max_y {
        return None;
    }

    Some(Rect {
        x: min_x,
        y: min_y,
        w: max_x - min_x,
        h: max_y - min_y,
    })
}

/// Convert a BSP tree into an ordered list of restore steps.
pub fn plan_from_bsp(tree: &BspNode) -> Vec<RestoreStep> {
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

/// Result of BSP inference for a workspace: the ordered restore steps.
pub struct WorkspacePlan {
    pub steps: Vec<RestoreStep>,
}

/// Build a restore plan for a set of tiled windows on a single workspace.
///
/// `global_indices` maps each window in `windows` to its index in the full
/// session window list.
///
/// Returns `None` if BSP inference fails (falls back to simple restore).
pub fn build_workspace_plan(
    windows: &[&WindowEntry],
    global_indices: &[usize],
) -> Option<WorkspacePlan> {
    let bounds = bounding_rect(windows)?;
    let indexed = extract_indexed(windows, global_indices)?;
    let tree = infer_bsp(&indexed, bounds)?;
    let steps = plan_from_bsp(&tree);
    Some(WorkspacePlan { steps })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(app_id: &str, ws: &str, x: i32, y: i32, w: i32, h: i32) -> WindowEntry {
        WindowEntry {
            app_id: app_id.to_string(),
            launch_cmd: app_id.to_string(),
            workspace: ws.to_string(),
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
                match second.as_ref() {
                    BspNode::Split {
                        dir: SplitDir::Vertical,
                        ..
                    } => {}
                    other => panic!("expected vertical split, got {other:?}"),
                }
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
        assert!(wp.steps[0].focus_idx.is_none());

        assert_eq!(wp.steps[1].window_idx, 2);
        assert_eq!(wp.steps[1].focus_idx, Some(0));
        assert!(matches!(wp.steps[1].preselect, Some(PreselDir::Right)));

        assert_eq!(wp.steps[2].window_idx, 1);
        assert_eq!(wp.steps[2].focus_idx, Some(0));
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
        let wp = plan.unwrap();
        assert_eq!(wp.steps.len(), 2);
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
        let wp = plan.unwrap();
        assert_eq!(wp.steps.len(), 3);
    }

    #[test]
    fn multi_monitor_offset_handled() {
        // Windows on a second monitor at x=1920 with gaps
        let a = make_entry("a", "2", 1925, 5, 950, 1070);
        let b = make_entry("b", "2", 2885, 5, 950, 1070);
        let refs = vec![&a, &b];
        let plan = build_workspace_plan(&refs, &[0, 1]);
        assert!(plan.is_some(), "should handle windows on offset monitor");
        let wp = plan.unwrap();
        assert_eq!(wp.steps.len(), 2);
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
            BspNode::Split { ratio, .. } => {
                assert!((ratio - 0.6).abs() < 0.01);
            }
            BspNode::Leaf { .. } => panic!("expected split"),
        }
    }

    #[test]
    fn missing_geometry_returns_none() {
        let w = WindowEntry {
            app_id: "x".to_string(),
            launch_cmd: "x".to_string(),
            workspace: "1".to_string(),
            floating: false,
            fullscreen: false,
            position: None,
            size: None,
            cwd: None,
            profile: None,
        };
        let refs = vec![&w];
        assert!(bounding_rect(&refs).is_none());
    }
}
