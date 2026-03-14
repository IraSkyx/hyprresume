use crate::models::WindowEntry;

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

/// Infer a BSP tree from a set of tiled windows based on their geometry.
///
/// Returns `None` if the windows can't form a valid BSP partition
/// (e.g. overlapping, gaps, or missing geometry).
fn infer_bsp(indexed: &[IndexedWindow], bounds: Rect) -> Option<BspNode> {
    if indexed.is_empty() {
        return None;
    }
    if indexed.len() == 1 {
        return Some(BspNode::Leaf {
            idx: indexed[0].idx,
        });
    }

    try_split(indexed, bounds, SplitDir::Horizontal)
        .or_else(|| try_split(indexed, bounds, SplitDir::Vertical))
}

fn try_split(indexed: &[IndexedWindow], bounds: Rect, dir: SplitDir) -> Option<BspNode> {
    let mut candidates: Vec<i32> = Vec::new();
    for iw in indexed {
        match dir {
            SplitDir::Horizontal => {
                candidates.push(iw.x);
                candidates.push(iw.x + iw.w);
            }
            SplitDir::Vertical => {
                candidates.push(iw.y);
                candidates.push(iw.y + iw.h);
            }
        }
    }
    candidates.sort_unstable();
    candidates.dedup();

    for &split_at in &candidates {
        if let Some(node) = try_split_at(indexed, bounds, dir, split_at) {
            return Some(node);
        }
    }

    None
}

fn try_split_at(
    indexed: &[IndexedWindow],
    bounds: Rect,
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

        if end <= split_at {
            first_group.push(iw);
        } else if start >= split_at {
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

    let first_node = infer_bsp(&first_owned, first_bounds)?;
    let second_node = infer_bsp(&second_owned, second_bounds)?;

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
            walk_bsp(first, focus_idx, preselect, steps);

            let first_leaf = leftmost_leaf_idx(first);
            let presel = match dir {
                SplitDir::Horizontal => PreselDir::Right,
                SplitDir::Vertical => PreselDir::Bottom,
            };
            walk_bsp(second, Some(first_leaf), Some(presel), steps);
        }
    }
}

fn leftmost_leaf_idx(node: &BspNode) -> usize {
    match node {
        BspNode::Leaf { idx } => *idx,
        BspNode::Split { first, .. } => leftmost_leaf_idx(first),
    }
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
) -> Option<Vec<RestoreStep>> {
    let bounds = bounding_rect(windows)?;
    let indexed = extract_indexed(windows, global_indices)?;
    let tree = infer_bsp(&indexed, bounds)?;
    Some(plan_from_bsp(&tree))
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
    fn plan_respects_opening_order() {
        let a = make_entry("a", "1", 0, 0, 960, 1080);
        let b = make_entry("b", "1", 960, 0, 960, 540);
        let c = make_entry("c", "1", 960, 540, 960, 540);

        let refs: Vec<&WindowEntry> = vec![&a, &b, &c];
        let steps = build_workspace_plan(&refs, &[0, 1, 2]).unwrap();
        assert_eq!(steps.len(), 3);

        assert!(steps[0].focus_idx.is_none());
        assert!(steps[0].preselect.is_none());

        assert!(steps[1].focus_idx.is_some());
        assert!(matches!(steps[1].preselect, Some(PreselDir::Right)));

        assert!(steps[2].focus_idx.is_some());
        assert!(matches!(steps[2].preselect, Some(PreselDir::Bottom)));
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
