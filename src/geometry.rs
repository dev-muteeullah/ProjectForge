//! Pure geometry shared by the on-screen Gantt painter and the SVG exporter
//! (no egui types, so it is unit-testable): row metrics and the orthogonal "elbow" router
//! for dependency arrows.

use crate::model::DepKind;

pub const ROW_H: f32 = 26.0;
pub const HEADER_H: f32 = 40.0;
pub const GAP: f32 = 8.0;

/// (leaves the predecessor at its FINISH?, enters the successor at its FINISH?)
pub fn link_ends(kind: DepKind) -> (bool, bool) {
    match kind {
        DepKind::FS => (true, false),
        DepKind::SS => (false, false),
        DepKind::FF => (true, true),
        DepKind::SF => (false, true),
    }
}

pub type Pt = (f32, f32);

/// Orthogonal polyline from `p` (pred bar edge, vertically centred) to `q` (succ bar edge).
/// The last segment always points at `q` horizontally, so the arrowhead direction is
/// `+1` (rightwards) when entering a START and `-1` (leftwards) when entering a FINISH.
pub fn route(p: Pt, from_finish: bool, q: Pt, to_finish: bool) -> Vec<Pt> {
    let sp = if from_finish { 1.0 } else { -1.0 };
    let sq = if to_finish { 1.0 } else { -1.0 };
    // x of the single vertical segment, if one fits: it must be GAP away from `p` on the exit
    // side and GAP away from `q` on the entry side.
    let mut lo = f32::NEG_INFINITY;
    let mut hi = f32::INFINITY;
    if sp > 0.0 {
        lo = lo.max(p.0 + GAP);
    } else {
        hi = hi.min(p.0 - GAP);
    }
    if sq > 0.0 {
        lo = lo.max(q.0 + GAP);
    } else {
        hi = hi.min(q.0 - GAP);
    }
    if lo <= hi {
        let xm = if lo.is_finite() { lo } else { hi };
        return vec![p, (xm, p.1), (xm, q.1), q];
    }
    // detour through the gap between rows
    let mid_y = if q.1 >= p.1 { q.1 - ROW_H / 2.0 } else { q.1 + ROW_H / 2.0 };
    let x1 = p.0 + sp * GAP;
    let x2 = q.0 + sq * GAP;
    vec![p, (x1, p.1), (x1, mid_y), (x2, mid_y), (x2, q.1), q]
}

/// Direction of the final segment: +1 right, -1 left.
pub fn arrow_dir(to_finish: bool) -> f32 {
    if to_finish {
        -1.0
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn orthogonal(pts: &[Pt]) -> bool {
        pts.windows(2).all(|w| (w[0].0 - w[1].0).abs() < 1e-3 || (w[0].1 - w[1].1).abs() < 1e-3)
    }

    #[test]
    fn fs_forward_is_a_simple_elbow() {
        let r = route((100.0, 13.0), true, (200.0, 39.0), false);
        assert_eq!(r.len(), 4);
        assert!(orthogonal(&r));
        assert_eq!(r.last().copied(), Some((200.0, 39.0)));
    }

    #[test]
    fn fs_backward_detours_between_rows() {
        let r = route((200.0, 13.0), true, (100.0, 39.0), false);
        assert_eq!(r.len(), 6);
        assert!(orthogonal(&r));
        // the horizontal run happens between the two rows
        assert!(r[2].1 > 13.0 && r[2].1 < 39.0);
    }

    #[test]
    fn all_kinds_are_orthogonal_in_all_arrangements() {
        for kind in DepKind::ALL {
            let (a, b) = link_ends(kind);
            for (px, qx) in [(100.0, 300.0), (300.0, 100.0), (100.0, 100.0), (100.0, 105.0)] {
                for (py, qy) in [(13.0, 39.0), (39.0, 13.0), (13.0, 13.0)] {
                    let r = route((px, py), a, (qx, qy), b);
                    assert!(orthogonal(&r), "{:?} {:?}", kind, r);
                    let n = r.len();
                    // final segment is horizontal into q
                    assert!((r[n - 2].1 - r[n - 1].1).abs() < 1e-3);
                    // and approaches from the correct side
                    let from_left = r[n - 2].0 < r[n - 1].0;
                    assert_eq!(from_left, arrow_dir(b) > 0.0, "{:?} {:?}", kind, r);
                }
            }
        }
    }
}
