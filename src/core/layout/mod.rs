pub mod dwindle;
pub mod master;

use crate::models::WindowEntry;

/// Small buffer added to the measured gap to account for border widths and sub-pixel rounding.
pub const GAP_ROUNDING_BUFFER: i32 = 4;

/// A rectangle in screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// An indexed window: pairs a global index with position/size data.
#[derive(Clone)]
pub struct IndexedWindow {
    pub idx: usize,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

pub fn extract_indexed(
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
pub fn infer_gap_from_geometry(indexed: &[IndexedWindow]) -> i32 {
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

pub const fn ranges_overlap(a_start: i32, a_end: i32, b_start: i32, b_end: i32) -> bool {
    a_start < b_end && b_start < a_end
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

pub const fn split_bounds(bounds: Rect, split_axis_is_h: bool, split_at: i32) -> (Rect, Rect) {
    if split_axis_is_h {
        (
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
        )
    } else {
        (
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
        )
    }
}

/// Collect split-edge candidates along one axis, including gap midpoints
/// that account for Hyprland's `gaps_in`.
pub fn split_candidates(indexed: &[IndexedWindow], horizontal: bool, tolerance: i32) -> Vec<i32> {
    let mut edges: Vec<i32> = Vec::new();
    for iw in indexed {
        if horizontal {
            edges.push(iw.x);
            edges.push(iw.x + iw.w);
        } else {
            edges.push(iw.y);
            edges.push(iw.y + iw.h);
        }
    }
    edges.sort_unstable();
    edges.dedup();

    let mut candidates = edges.clone();
    for pair in edges.windows(2) {
        let gap = pair[1] - pair[0];
        if gap > 0 && gap <= tolerance {
            candidates.push(pair[0] + gap / 2);
        }
    }
    candidates.sort_unstable();
    candidates.dedup();
    candidates
}

/// Partition `indexed` windows into two groups along a split edge.
/// Returns `None` if any window straddles the split line.
pub fn partition_at(
    indexed: &[IndexedWindow],
    horizontal: bool,
    tolerance: i32,
    split_at: i32,
) -> Option<(Vec<usize>, Vec<usize>)> {
    let mut first = Vec::new();
    let mut second = Vec::new();

    for iw in indexed {
        let (start, end) = if horizontal {
            (iw.x, iw.x + iw.w)
        } else {
            (iw.y, iw.y + iw.h)
        };

        if end <= split_at + tolerance {
            first.push(iw.idx);
        } else if start >= split_at - tolerance {
            second.push(iw.idx);
        } else {
            return None;
        }
    }

    if first.is_empty() || second.is_empty() {
        return None;
    }

    Some((first, second))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounding_rect_basic() {
        let a = WindowEntry {
            app_id: "a".into(),
            launch_cmd: "a".into(),
            workspace: "1".into(),
            monitor: None,
            floating: false,
            fullscreen: false,
            position: Some((10, 20)),
            size: Some((100, 200)),
            cwd: None,
            profile: None,
        };
        let refs = vec![&a];
        let r = bounding_rect(&refs).unwrap();
        assert_eq!(
            r,
            Rect {
                x: 10,
                y: 20,
                w: 100,
                h: 200
            }
        );
    }

    #[test]
    fn bounding_rect_missing_returns_none() {
        let a = WindowEntry {
            app_id: "a".into(),
            launch_cmd: "a".into(),
            workspace: "1".into(),
            monitor: None,
            floating: false,
            fullscreen: false,
            position: None,
            size: None,
            cwd: None,
            profile: None,
        };
        assert!(bounding_rect(&[&a]).is_none());
    }
}
