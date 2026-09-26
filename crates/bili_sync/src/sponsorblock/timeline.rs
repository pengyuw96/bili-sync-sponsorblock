//! Merge remove intervals and invert to keep ranges for ffmpeg cutting.
//!
//! Inspired by BilibiliSponsorBlock segment semantics:
//! https://github.com/hanydd/BilibiliSponsorBlock/wiki/API

/// Merge overlapping / adjacent remove intervals (gap ≤ epsilon).
pub fn merge_remove_intervals(intervals: &[(f64, f64)], epsilon: f64) -> Vec<(f64, f64)> {
    let mut items: Vec<(f64, f64)> = intervals
        .iter()
        .copied()
        .filter(|(s, e)| e > s && s.is_finite() && e.is_finite())
        .collect();
    if items.is_empty() {
        return Vec::new();
    }
    items.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut merged = Vec::with_capacity(items.len());
    let mut cur = items[0];
    for next in items.into_iter().skip(1) {
        if next.0 <= cur.1 + epsilon {
            cur.1 = cur.1.max(next.1);
        } else {
            merged.push(cur);
            cur = next;
        }
    }
    merged.push(cur);
    merged
}

/// Invert remove intervals against `[0, duration]` → keep intervals.
pub fn invert_to_keep(removes: &[(f64, f64)], duration: f64) -> Vec<(f64, f64)> {
    if !(duration.is_finite() && duration > 0.0) {
        return Vec::new();
    }
    let removes = merge_remove_intervals(removes, 1e-6);
    let mut keeps = Vec::new();
    let mut cursor = 0.0_f64;
    for (start, end) in removes {
        let start = start.clamp(0.0, duration);
        let end = end.clamp(0.0, duration);
        if start > cursor {
            keeps.push((cursor, start));
        }
        cursor = cursor.max(end);
    }
    if cursor < duration {
        keeps.push((cursor, duration));
    }
    keeps
}

/// Drop keep gaps shorter than `min_keep_gap` (absorb into remove).
pub fn absorb_short_keeps(keeps: Vec<(f64, f64)>, min_keep_gap: f64) -> Vec<(f64, f64)> {
    if min_keep_gap <= 0.0 {
        return keeps;
    }
    keeps
        .into_iter()
        .filter(|(s, e)| (e - s) >= min_keep_gap)
        .collect()
}

/// Filter raw remove intervals by min length, then merge → keep → absorb short keeps.
///
/// Returns `None` when cutting would remove ~100% of the video (caller should fail-open).
/// Returns `Some(empty)` when there is nothing to cut (no-op).
/// Returns `Some(keeps)` when cutting should proceed.
pub fn compute_keep_ranges(
    remove_intervals: &[(f64, f64)],
    duration: f64,
    min_segment_seconds: f64,
    min_keep_gap_seconds: f64,
) -> Option<Vec<(f64, f64)>> {
    if !(duration.is_finite() && duration > 0.0) {
        return Some(Vec::new());
    }

    let filtered: Vec<(f64, f64)> = remove_intervals
        .iter()
        .copied()
        .filter(|(s, e)| {
            let len = e - s;
            len.is_finite() && len >= min_segment_seconds.max(0.0)
        })
        .collect();

    if filtered.is_empty() {
        return Some(Vec::new());
    }

    let removes = merge_remove_intervals(&filtered, 1e-3);
    let keeps = invert_to_keep(&removes, duration);
    let keeps = absorb_short_keeps(keeps, min_keep_gap_seconds);

    let keep_total: f64 = keeps.iter().map(|(s, e)| e - s).sum();
    // Would remove ~100% of video
    if keeps.is_empty() || keep_total <= duration * 0.01 {
        return None;
    }

    // Full video kept → no-op
    if keeps.len() == 1 && (keeps[0].0 - 0.0).abs() < 1e-3 && (keeps[0].1 - duration).abs() < 1e-3 {
        return Some(Vec::new());
    }

    Some(keeps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_overlapping_and_adjacent() {
        let merged = merge_remove_intervals(&[(1.0, 3.0), (2.5, 5.0), (5.0, 6.0), (10.0, 12.0)], 1e-6);
        assert_eq!(merged, vec![(1.0, 6.0), (10.0, 12.0)]);
    }

    #[test]
    fn invert_basic() {
        let keeps = invert_to_keep(&[(10.0, 20.0), (50.0, 60.0)], 100.0);
        assert_eq!(keeps, vec![(0.0, 10.0), (20.0, 50.0), (60.0, 100.0)]);
    }

    #[test]
    fn invert_start_and_end_removes() {
        let keeps = invert_to_keep(&[(0.0, 5.0), (90.0, 100.0)], 100.0);
        assert_eq!(keeps, vec![(5.0, 90.0)]);
    }

    #[test]
    fn absorb_short_keep_gap() {
        let keeps = invert_to_keep(&[(10.0, 20.0), (20.2, 30.0)], 100.0);
        // tiny keep between 20 and 20.2
        assert!(keeps.iter().any(|(s, e)| (*e - *s) < 0.3));
        let absorbed = absorb_short_keeps(keeps, 0.3);
        assert_eq!(absorbed, vec![(0.0, 10.0), (30.0, 100.0)]);
    }

    #[test]
    fn compute_would_remove_all_returns_none() {
        let result = compute_keep_ranges(&[(0.0, 100.0)], 100.0, 0.5, 0.3);
        assert!(result.is_none());
    }

    #[test]
    fn compute_noop_when_empty_removes() {
        let result = compute_keep_ranges(&[], 100.0, 0.5, 0.3);
        assert_eq!(result, Some(vec![]));
    }

    #[test]
    fn compute_typical_sponsor_cut() {
        // Sample inspired by live API: intro [0, 20.934]
        let result = compute_keep_ranges(&[(0.0, 20.934)], 120.0, 0.5, 0.3).unwrap();
        assert_eq!(result.len(), 1);
        assert!((result[0].0 - 20.934).abs() < 1e-6);
        assert!((result[0].1 - 120.0).abs() < 1e-6);
    }

    #[test]
    fn min_segment_filters_short_removes() {
        let result = compute_keep_ranges(&[(10.0, 10.2)], 100.0, 0.5, 0.3).unwrap();
        assert!(result.is_empty());
    }
}

/// Subtract duration of all removed intervals that end at or before `t`.
/// If `t` falls inside a remove interval, clamp to the interval start then subtract
/// prior removes (so in-remove timestamps collapse to the cut seam).
pub fn remap_timestamp_after_removes(t: f64, removes: &[(f64, f64)]) -> f64 {
    if !t.is_finite() {
        return t;
    }
    let removes = merge_remove_intervals(removes, 1e-6);
    let mut offset = 0.0_f64;
    for (s, e) in &removes {
        if *e <= t {
            offset += e - s;
        } else if *s < t {
            // Inside a removed region: collapse to the cut seam (start of remove),
            // after subtracting only prior fully-removed intervals.
            return (*s - offset).max(0.0);
        } else {
            break;
        }
    }
    (t - offset).max(0.0)
}

/// Remap a mark `[start, end]` after cuts. Returns `None` if the mark vanishes
/// (fully inside a cut, or zero/negative length after remap).
pub fn remap_mark_interval(
    start: f64,
    end: f64,
    removes: &[(f64, f64)],
    new_duration: f64,
) -> Option<(f64, f64)> {
    if !(start.is_finite() && end.is_finite()) || end <= start {
        return None;
    }
    // Drop marks fully covered by a single remove interval.
    for (s, e) in merge_remove_intervals(removes, 1e-6) {
        if start >= s && end <= e {
            return None;
        }
    }
    let mut rs = remap_timestamp_after_removes(start, removes);
    let mut re = remap_timestamp_after_removes(end, removes);
    if new_duration.is_finite() && new_duration > 0.0 {
        rs = rs.clamp(0.0, new_duration);
        re = re.clamp(0.0, new_duration);
    }
    if re - rs < 1e-3 {
        return None;
    }
    Some((rs, re))
}

/// Approximate post-cut duration = original − merged remove length (clamped).
pub fn duration_after_removes(duration: f64, removes: &[(f64, f64)]) -> f64 {
    if !(duration.is_finite() && duration > 0.0) {
        return 0.0;
    }
    let removed: f64 = merge_remove_intervals(removes, 1e-6)
        .iter()
        .map(|(s, e)| {
            let s = s.clamp(0.0, duration);
            let e = e.clamp(0.0, duration);
            (e - s).max(0.0)
        })
        .sum();
    (duration - removed).max(0.0)
}

#[cfg(test)]
mod remap_tests {
    use super::*;

    #[test]
    fn remap_after_single_intro_cut() {
        let removes = [(0.0, 20.0)];
        assert!((remap_timestamp_after_removes(0.0, &removes) - 0.0).abs() < 1e-6);
        assert!((remap_timestamp_after_removes(20.0, &removes) - 0.0).abs() < 1e-6);
        assert!((remap_timestamp_after_removes(50.0, &removes) - 30.0).abs() < 1e-6);
    }

    #[test]
    fn remap_after_two_cuts() {
        let removes = [(10.0, 20.0), (50.0, 60.0)];
        // Mark starting at 70 → subtract 10+10 = 20 → 50
        assert!((remap_timestamp_after_removes(70.0, &removes) - 50.0).abs() < 1e-6);
        // Mark at 30 (between cuts) → subtract only first 10 → 20
        assert!((remap_timestamp_after_removes(30.0, &removes) - 20.0).abs() < 1e-6);
        // Inside second cut → collapse to seam at 50 → remapped 40
        assert!((remap_timestamp_after_removes(55.0, &removes) - 40.0).abs() < 1e-6);
    }

    #[test]
    fn remap_mark_drops_fully_cut_segment() {
        let removes = [(10.0, 30.0)];
        assert_eq!(remap_mark_interval(12.0, 25.0, &removes, 100.0), None);
    }

    #[test]
    fn remap_mark_typical_after_cut() {
        let removes = [(0.0, 20.934)];
        let (s, e) = remap_mark_interval(90.0, 100.0, &removes, 99.066).unwrap();
        assert!((s - 69.066).abs() < 1e-3);
        assert!((e - 79.066).abs() < 1e-3);
    }

    #[test]
    fn remap_noop_without_removes() {
        let (s, e) = remap_mark_interval(10.0, 20.0, &[], 100.0).unwrap();
        assert!((s - 10.0).abs() < 1e-9);
        assert!((e - 20.0).abs() < 1e-9);
    }

    #[test]
    fn duration_after_removes_basic() {
        assert!((duration_after_removes(100.0, &[(10.0, 20.0), (50.0, 60.0)]) - 80.0).abs() < 1e-6);
    }
}
