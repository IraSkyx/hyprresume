use crate::models::WindowEntry;

/// A rectangle in screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Binary split direction in the Dwindle BSP tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SplitDir {
    /// Left / Right
    Horizontal,
    /// Top / Bottom
    Vertical,
}

/// A node in the inferred BSP tree.
#[derive(Debug)]
#[allow(dead_code)]
pub enum BspNode<'a> {
    Leaf {
        window: &'a WindowEntry,
    },
    Split {
        dir: SplitDir,
        ratio: f64,
        first: Box<BspNode<'a>>,
        second: Box<BspNode<'a>>,
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
            PreselDir::Right => write!(f, "r"),
            PreselDir::Bottom => write!(f, "b"),
        }
    }
}

/// Infer a BSP tree from a set of tiled windows based on their geometry.
///
/// Returns `None` if the windows can't form a valid BSP partition
/// (e.g. overlapping, gaps, or missing geometry).
pub fn infer_bsp<'a>(windows: &[&'a WindowEntry], bounds: Rect) -> Option<BspNode<'a>> {
    if windows.is_empty() {
        return None;
    }
    if windows.len() == 1 {
        return Some(BspNode::Leaf {
            window: windows[0],
        });
    }

    // Collect all interior vertical edges (potential vertical split lines).
    // A valid split line at `sx` means every window is entirely left or right of it.
    if let Some(node) = try_split(windows, bounds, SplitDir::Horizontal) {
        return Some(node);
    }
    if let Some(node) = try_split(windows, bounds, SplitDir::Vertical) {
        return Some(node);
    }

    None
}

fn try_split<'a>(
    windows: &[&'a WindowEntry],
    bounds: Rect,
    dir: SplitDir,
) -> Option<BspNode<'a>> {
    // Collect candidate split positions from window edges
    let mut candidates: Vec<i32> = Vec::new();
    for w in windows {
        let (Some((x, y)), Some((ww, hh))) = (w.position, w.size) else {
            return None;
        };
        match dir {
            SplitDir::Horizontal => {
                candidates.push(x);
                candidates.push(x + ww);
            }
            SplitDir::Vertical => {
                candidates.push(y);
                candidates.push(y + hh);
            }
        }
    }
    candidates.sort_unstable();
    candidates.dedup();

    for &split_at in &candidates {
        let (range_start, range_end) = match dir {
            SplitDir::Horizontal => (bounds.x, bounds.x + bounds.w),
            SplitDir::Vertical => (bounds.y, bounds.y + bounds.h),
        };

        // Split must be interior to bounds
        if split_at <= range_start || split_at >= range_end {
            continue;
        }

        let (mut first_group, mut second_group): (Vec<&WindowEntry>, Vec<&WindowEntry>) =
            (Vec::new(), Vec::new());
        let mut valid = true;

        for w in windows {
            let (Some((x, y)), Some((ww, hh))) = (w.position, w.size) else {
                valid = false;
                break;
            };
            let (start, end) = match dir {
                SplitDir::Horizontal => (x, x + ww),
                SplitDir::Vertical => (y, y + hh),
            };

            if end <= split_at {
                first_group.push(w);
            } else if start >= split_at {
                second_group.push(w);
            } else {
                valid = false;
                break;
            }
        }

        if !valid || first_group.is_empty() || second_group.is_empty() {
            continue;
        }

        let (first_bounds, second_bounds) = match dir {
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
        };

        if let (Some(first_node), Some(second_node)) = (
            infer_bsp(&first_group, first_bounds),
            infer_bsp(&second_group, second_bounds),
        ) {
            let total = match dir {
                SplitDir::Horizontal => bounds.w as f64,
                SplitDir::Vertical => bounds.h as f64,
            };
            let first_size = match dir {
                SplitDir::Horizontal => first_bounds.w as f64,
                SplitDir::Vertical => first_bounds.h as f64,
            };
            let ratio = first_size / total;

            return Some(BspNode::Split {
                dir,
                ratio,
                first: Box::new(first_node),
                second: Box::new(second_node),
            });
        }
    }

    None
}

/// Compute the bounding rectangle of a set of windows.
pub fn bounding_rect(windows: &[&WindowEntry]) -> Option<Rect> {
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;

    for w in windows {
        let (Some((x, y)), Some((ww, hh))) = (w.position, w.size) else {
            return None;
        };
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
///
/// `window_index` maps each `WindowEntry` pointer to its index in the
/// original session windows list so restore can look them up.
pub fn plan_from_bsp(
    tree: &BspNode<'_>,
    window_index: &std::collections::HashMap<*const WindowEntry, usize>,
) -> Vec<RestoreStep> {
    let mut steps = Vec::new();
    walk_bsp(tree, None, None, window_index, &mut steps);

    // Compute resize deltas: compare saved size vs default 50/50 split sizes.
    // We do this after all steps are generated so we know the expected
    // default sizes. For now, we skip resize — Hyprland's preselect + open
    // gets the tree structure right, and resizewindowpixel adjustments
    // can be applied as a post-pass.
    steps
}

fn walk_bsp(
    node: &BspNode<'_>,
    focus_idx: Option<usize>,
    preselect: Option<PreselDir>,
    window_index: &std::collections::HashMap<*const WindowEntry, usize>,
    steps: &mut Vec<RestoreStep>,
) {
    match node {
        BspNode::Leaf { window } => {
            let idx = window_index[&(*window as *const WindowEntry)];
            steps.push(RestoreStep {
                window_idx: idx,
                focus_idx,
                preselect,
            });
        }
        BspNode::Split {
            dir,
            first,
            second,
            ..
        } => {
            // Open the entire first subtree (inherits our focus/preselect context)
            walk_bsp(first, focus_idx, preselect, window_index, steps);

            // For the second subtree: focus the first subtree's leftmost leaf,
            // preselect the appropriate direction, then open the second subtree.
            let first_leaf_idx = leftmost_leaf_idx(first, window_index);
            let presel = match dir {
                SplitDir::Horizontal => PreselDir::Right,
                SplitDir::Vertical => PreselDir::Bottom,
            };
            walk_bsp(
                second,
                Some(first_leaf_idx),
                Some(presel),
                window_index,
                steps,
            );
        }
    }
}

fn leftmost_leaf_idx(
    node: &BspNode<'_>,
    window_index: &std::collections::HashMap<*const WindowEntry, usize>,
) -> usize {
    match node {
        BspNode::Leaf { window } => window_index[&(*window as *const WindowEntry)],
        BspNode::Split { first, .. } => leftmost_leaf_idx(first, window_index),
    }
}

/// Build a restore plan for a set of tiled windows on a single workspace.
///
/// Returns `None` if BSP inference fails (falls back to simple restore).
pub fn build_workspace_plan(
    windows: &[&WindowEntry],
    window_index: &std::collections::HashMap<*const WindowEntry, usize>,
) -> Option<Vec<RestoreStep>> {
    let bounds = bounding_rect(windows)?;
    let tree = infer_bsp(windows, bounds)?;
    Some(plan_from_bsp(&tree, window_index))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(
        app_id: &str,
        ws: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> WindowEntry {
        WindowEntry {
            app_id: app_id.to_string(),
            launch_cmd: app_id.to_string(),
            workspace: ws.to_string(),
            floating: false,
            fullscreen: false,
            position: Some((x, y)),
            size: Some((w, h)),
        }
    }

    #[test]
    fn single_window_is_leaf() {
        let w = make_entry("firefox", "1", 0, 0, 1920, 1080);
        let refs = vec![&w];
        let bounds = bounding_rect(&refs).unwrap();
        let tree = infer_bsp(&refs, bounds).unwrap();
        assert!(matches!(tree, BspNode::Leaf { .. }));
    }

    #[test]
    fn two_windows_horizontal_split() {
        // Left half and right half
        let a = make_entry("firefox", "1", 0, 0, 960, 1080);
        let b = make_entry("code", "1", 960, 0, 960, 1080);
        let refs = vec![&a, &b];
        let bounds = bounding_rect(&refs).unwrap();
        let tree = infer_bsp(&refs, bounds).unwrap();

        match &tree {
            BspNode::Split { dir, ratio, .. } => {
                assert_eq!(*dir, SplitDir::Horizontal);
                assert!((ratio - 0.5).abs() < 0.01);
            }
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn two_windows_vertical_split() {
        // Top half and bottom half
        let a = make_entry("firefox", "1", 0, 0, 1920, 540);
        let b = make_entry("code", "1", 0, 540, 1920, 540);
        let refs = vec![&a, &b];
        let bounds = bounding_rect(&refs).unwrap();
        let tree = infer_bsp(&refs, bounds).unwrap();

        match &tree {
            BspNode::Split { dir, ratio, .. } => {
                assert_eq!(*dir, SplitDir::Vertical);
                assert!((ratio - 0.5).abs() < 0.01);
            }
            _ => panic!("expected split"),
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
        let tree = infer_bsp(&refs, bounds).unwrap();

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

        let all = vec![a.clone(), b.clone(), c.clone()];
        let refs: Vec<&WindowEntry> = all.iter().collect();

        let mut idx_map = std::collections::HashMap::new();
        for (i, w) in refs.iter().enumerate() {
            idx_map.insert(*w as *const WindowEntry, i);
        }

        let steps = build_workspace_plan(&refs, &idx_map).unwrap();
        assert_eq!(steps.len(), 3);

        // First window: no focus, no preselect
        assert!(steps[0].focus_idx.is_none());
        assert!(steps[0].preselect.is_none());

        // Second window: focus first, preselect right
        assert!(steps[1].focus_idx.is_some());
        assert!(matches!(steps[1].preselect, Some(PreselDir::Right)));

        // Third window: focus second (b), preselect bottom
        assert!(steps[2].focus_idx.is_some());
        assert!(matches!(steps[2].preselect, Some(PreselDir::Bottom)));
    }

    #[test]
    fn uneven_ratio() {
        // 60/40 split
        let a = make_entry("a", "1", 0, 0, 1152, 1080);
        let b = make_entry("b", "1", 1152, 0, 768, 1080);
        let refs = vec![&a, &b];
        let bounds = bounding_rect(&refs).unwrap();
        let tree = infer_bsp(&refs, bounds).unwrap();

        match &tree {
            BspNode::Split { ratio, .. } => {
                assert!((ratio - 0.6).abs() < 0.01);
            }
            _ => panic!("expected split"),
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
        };
        let refs = vec![&w];
        assert!(bounding_rect(&refs).is_none());
    }
}
