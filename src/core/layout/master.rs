use crate::models::WindowEntry;

use super::{
    GAP_ROUNDING_BUFFER, IndexedWindow, bounding_rect, extract_indexed, infer_gap_from_geometry,
    partition_at, split_candidates,
};

/// Orientation of the master area relative to the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterOrientation {
    Left,
    Right,
    Top,
    Bottom,
}

impl std::fmt::Display for MasterOrientation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Left => write!(f, "left"),
            Self::Right => write!(f, "right"),
            Self::Top => write!(f, "top"),
            Self::Bottom => write!(f, "bottom"),
        }
    }
}

/// Restore plan for one workspace under the master layout.
pub struct MasterPlan {
    pub master_indices: Vec<usize>,
    pub stack_indices: Vec<usize>,
    pub orientation: MasterOrientation,
    /// Master-factor: `master_extent / total_extent` along the split axis.
    pub mfact: f64,
}

/// Try to infer a `MasterPlan` from saved window geometry.
///
/// Heuristic: find the dominant split axis (horizontal or vertical) that
/// separates windows into two groups along an edge. The side with fewer
/// windows (or the left/top side in a tie) is treated as the master area.
pub fn build_workspace_plan(
    windows: &[&WindowEntry],
    global_indices: &[usize],
) -> Option<MasterPlan> {
    let indexed = extract_indexed(windows, global_indices)?;
    if indexed.is_empty() {
        return None;
    }
    if indexed.len() == 1 {
        return Some(MasterPlan {
            master_indices: vec![indexed[0].idx],
            stack_indices: Vec::new(),
            orientation: MasterOrientation::Left,
            mfact: 0.5,
        });
    }

    let bounds = bounding_rect(windows)?;
    let tolerance = infer_gap_from_geometry(&indexed) + GAP_ROUNDING_BUFFER;

    for orientation in [
        MasterOrientation::Left,
        MasterOrientation::Top,
        MasterOrientation::Right,
        MasterOrientation::Bottom,
    ] {
        if let Some(plan) = try_master_split(&indexed, bounds, tolerance, orientation) {
            return Some(plan);
        }
    }

    None
}

fn try_master_split(
    indexed: &[IndexedWindow],
    bounds: super::Rect,
    tolerance: i32,
    orientation: MasterOrientation,
) -> Option<MasterPlan> {
    let (horizontal, master_is_first) = match orientation {
        MasterOrientation::Left => (true, true),
        MasterOrientation::Right => (true, false),
        MasterOrientation::Top => (false, true),
        MasterOrientation::Bottom => (false, false),
    };

    let candidates = split_candidates(indexed, horizontal, tolerance);

    let (range_start, range_end) = if horizontal {
        (bounds.x, bounds.x + bounds.w)
    } else {
        (bounds.y, bounds.y + bounds.h)
    };

    for &split_at in &candidates {
        if split_at <= range_start || split_at >= range_end {
            continue;
        }

        let Some((first_group, second_group)) =
            partition_at(indexed, horizontal, tolerance, split_at)
        else {
            continue;
        };

        let (master_indices, stack_indices) = if master_is_first {
            (first_group, second_group)
        } else {
            (second_group, first_group)
        };

        if master_indices.len() > stack_indices.len() + 1 {
            continue;
        }

        let master_extent = f64::from(split_at - range_start);
        let total_extent = f64::from(range_end - range_start);
        let mfact = if master_is_first {
            master_extent / total_extent
        } else {
            1.0 - master_extent / total_extent
        };

        if !(0.1..=0.9).contains(&mfact) {
            continue;
        }

        return Some(MasterPlan {
            master_indices,
            stack_indices,
            orientation,
            mfact,
        });
    }

    None
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
    fn two_windows_left() {
        let a = make_entry("a", "1", 0, 0, 1152, 1080);
        let b = make_entry("b", "1", 1152, 0, 768, 1080);
        let refs = vec![&a, &b];
        let plan = build_workspace_plan(&refs, &[0, 1]).unwrap();
        assert_eq!(plan.orientation, MasterOrientation::Left);
        assert_eq!(plan.master_indices, vec![0]);
        assert_eq!(plan.stack_indices, vec![1]);
        assert!((plan.mfact - 0.6).abs() < 0.01);
    }

    #[test]
    fn three_windows_left() {
        let a = make_entry("a", "1", 0, 0, 960, 1080);
        let b = make_entry("b", "1", 960, 0, 960, 540);
        let c = make_entry("c", "1", 960, 540, 960, 540);
        let refs = vec![&a, &b, &c];
        let plan = build_workspace_plan(&refs, &[0, 1, 2]).unwrap();
        assert_eq!(plan.orientation, MasterOrientation::Left);
        assert_eq!(plan.master_indices, vec![0]);
        assert_eq!(plan.stack_indices.len(), 2);
        assert!((plan.mfact - 0.5).abs() < 0.01);
    }

    #[test]
    fn top_orientation() {
        let a = make_entry("a", "1", 0, 0, 1920, 540);
        let b = make_entry("b", "1", 0, 540, 960, 540);
        let c = make_entry("c", "1", 960, 540, 960, 540);
        let refs = vec![&a, &b, &c];
        let plan = build_workspace_plan(&refs, &[0, 1, 2]).unwrap();
        assert_eq!(plan.orientation, MasterOrientation::Top);
        assert_eq!(plan.master_indices, vec![0]);
        assert_eq!(plan.stack_indices.len(), 2);
    }

    #[test]
    fn single_window() {
        let a = make_entry("a", "1", 0, 0, 1920, 1080);
        let refs = vec![&a];
        let plan = build_workspace_plan(&refs, &[0]).unwrap();
        assert_eq!(plan.master_indices, vec![0]);
        assert!(plan.stack_indices.is_empty());
    }

    #[test]
    fn with_gaps() {
        let a = make_entry("a", "1", 5, 5, 950, 1070);
        let b = make_entry("b", "1", 965, 5, 950, 1070);
        let refs = vec![&a, &b];
        let plan = build_workspace_plan(&refs, &[0, 1]).unwrap();
        assert_eq!(plan.orientation, MasterOrientation::Left);
        assert_eq!(plan.master_indices, vec![0]);
        assert_eq!(plan.stack_indices, vec![1]);
    }
}
