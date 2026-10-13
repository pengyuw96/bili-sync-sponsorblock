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


/// Largest keyframe at or before `t`. If none exist at/before `t`, returns `0.0`
/// (file start — same place ffmpeg input-seek falls back to).
pub fn previous_keyframe(t: f64, keyframes: &[f64]) -> f64 {
    if !t.is_finite() {
        return 0.0;
    }
    let t = t.max(0.0);
    if keyframes.is_empty() {
        return t;
    }
    match keyframes.binary_search_by(|k| k.partial_cmp(&t).unwrap_or(std::cmp::Ordering::Less)) {
        Ok(i) => keyframes[i],
        Err(0) => 0.0,
        Err(i) => keyframes[i - 1],
    }
}

/// Smallest keyframe at or after `t`. If none, returns `t` unchanged.
pub fn next_keyframe(t: f64, keyframes: &[f64]) -> f64 {
    if !t.is_finite() {
        return 0.0;
    }
    let t = t.max(0.0);
    if keyframes.is_empty() {
        return t;
    }
    match keyframes.binary_search_by(|k| k.partial_cmp(&t).unwrap_or(std::cmp::Ordering::Less)) {
        Ok(i) => keyframes[i],
        Err(i) if i < keyframes.len() => keyframes[i],
        Err(_) => t,
    }
}

/// Keyframe closest to `t` (ties prefer the earlier one). Empty → `t`.
pub fn nearest_keyframe(t: f64, keyframes: &[f64]) -> f64 {
    if !t.is_finite() {
        return 0.0;
    }
    let t = t.max(0.0);
    if keyframes.is_empty() {
        return t;
    }
    let prev = previous_keyframe(t, keyframes);
    if keyframes.last().is_some_and(|last| *last < t) {
        // no keyframe at/after t
        return prev;
    }
    let next = next_keyframe(t, keyframes);
    if (next - t) < (t - prev) {
        next
    } else {
        prev
    }
}

/// Merge overlapping / adjacent keep intervals (gap ≤ epsilon).
fn merge_keep_intervals(intervals: &[(f64, f64)], epsilon: f64) -> Vec<(f64, f64)> {
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

/// Snap keep ranges so cut-ins match ffmpeg stream-copy seek.
///
/// # Snap rule (must stay in sync with `ffmpeg_cut::extract_part`)
///
/// Stream copy can only start a part on a keyframe, so every cut-in is moved
/// to a keyframe and the media, chapters, subtitles and danmaku all use that
/// same snapped timeline.
///
/// - **cut-in** (`keep.start`): [`nearest_keyframe`]. The old rule always took
///   the previous keyframe, so every cut kept up to one whole GOP of the
///   sponsor (2–5 s on Bilibili encodes) and the removed total came up short.
///   Nearest bounds the error to half a GOP in either direction. The extract
///   then seeks to exactly that keyframe (`-ss` with µs precision).
/// - **cut-out** (`keep.end`): unchanged; `-t` stops at the last packet before
///   the end, which stream copy can do on any frame.
///
/// Overlapping/abutting keeps after snap are merged. Keyframes must already be
/// relative to the container start (see `probe_video_keyframe_times`).
/// Empty `keyframes` returns `keeps` unchanged (probe failure fail-open).
pub fn snap_keep_ranges_to_keyframes(
    keeps: &[(f64, f64)],
    keyframes: &[f64],
) -> Vec<(f64, f64)> {
    if keyframes.is_empty() {
        return keeps.to_vec();
    }
    let mut snapped = Vec::with_capacity(keeps.len());
    for &(start, end) in keeps {
        if !(start.is_finite() && end.is_finite()) || end <= start {
            continue;
        }
        let s = if start <= 1e-6 { 0.0 } else { nearest_keyframe(start, keyframes) };
        if end - s < 1e-3 {
            continue;
        }
        snapped.push((s, end));
    }
    merge_keep_intervals(&snapped, 1e-3)
}

/// Snap remove intervals by snapping complementary keeps, then re-deriving removes.
///
/// Prefer [`snap_keep_ranges_to_keyframes`] when you already have keep ranges from
/// [`compute_keep_ranges`] (those include short-gap absorption).
pub fn snap_intervals_to_keyframes(
    removes: &[(f64, f64)],
    duration: f64,
    keyframes: &[f64],
) -> Vec<(f64, f64)> {
    if keyframes.is_empty() {
        return merge_remove_intervals(removes, 1e-6);
    }
    let keeps = invert_to_keep(removes, duration);
    let snapped_keeps = snap_keep_ranges_to_keyframes(&keeps, keyframes);
    removes_from_keeps(&snapped_keeps, duration)
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

/// Removed regions implied by the keep ranges ffmpeg actually used.
///
/// `compute_keep_ranges` may absorb short gaps, so the real cut is the complement
/// of `keeps`, not the raw SponsorBlock intervals.
pub fn removes_from_keeps(keeps: &[(f64, f64)], duration: f64) -> Vec<(f64, f64)> {
    if !(duration.is_finite() && duration > 0.0) {
        return Vec::new();
    }
    let mut keeps: Vec<(f64, f64)> = keeps
        .iter()
        .copied()
        .filter(|(s, e)| e > s && s.is_finite() && e.is_finite())
        .map(|(s, e)| (s.clamp(0.0, duration), e.clamp(0.0, duration)))
        .filter(|(s, e)| e > s)
        .collect();
    keeps.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut removes = Vec::new();
    let mut cursor = 0.0_f64;
    for (start, end) in keeps {
        if start > cursor + 1e-3 {
            removes.push((cursor, start));
        }
        cursor = cursor.max(end);
    }
    if duration > cursor + 1e-3 {
        removes.push((cursor, duration));
    }
    removes
}

/// Pieces of `[start, end]` that survive `removes`, mapped onto the post-cut timeline.
///
/// A cue that spans a cut is split. A cue fully inside a cut disappears.
/// Empty `removes` returns the original interval.
pub fn surviving_pieces(start: f64, end: f64, removes: &[(f64, f64)]) -> Vec<(f64, f64)> {
    if !(start.is_finite() && end.is_finite()) || end - start < 0.05 {
        return Vec::new();
    }
    let removes = merge_remove_intervals(removes, 1e-3);
    if removes.is_empty() {
        return vec![(start, end)];
    }

    let mut pieces = vec![(start, end)];
    for &(rs, re) in &removes {
        let mut next = Vec::new();
        for (s, e) in pieces {
            if e <= rs + 1e-3 || s >= re - 1e-3 {
                next.push((s, e));
                continue;
            }
            if s < rs - 1e-3 {
                next.push((s, rs.min(e)));
            }
            if e > re + 1e-3 {
                next.push((re.max(s), e));
            }
        }
        pieces = next;
    }

    pieces
        .into_iter()
        .filter(|(s, e)| e - s >= 0.05)
        .filter_map(|(s, e)| {
            let rs = remap_timestamp_after_removes(s, &removes);
            let re = remap_timestamp_after_removes(e, &removes);
            (re - rs >= 0.05).then_some((rs, re))
        })
        .collect()
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

    #[test]
    fn removes_from_keeps_matches_absorbed_gap() {
        let removes = removes_from_keeps(&[(0.0, 10.0), (30.0, 100.0)], 100.0);
        assert_eq!(removes.len(), 1);
        assert!((removes[0].0 - 10.0).abs() < 1e-6);
        assert!((removes[0].1 - 30.0).abs() < 1e-6);
    }

    #[test]
    fn surviving_pieces_splits_cue_around_cut() {
        let pieces = surviving_pieces(5.0, 40.0, &[(10.0, 20.0)]);
        assert_eq!(pieces.len(), 2);
        assert!((pieces[0].0 - 5.0).abs() < 1e-6);
        assert!((pieces[0].1 - 10.0).abs() < 1e-6);
        assert!((pieces[1].0 - 10.0).abs() < 1e-6);
        assert!((pieces[1].1 - 30.0).abs() < 1e-6);
    }

    #[test]
    fn surviving_pieces_drops_cue_inside_cut() {
        assert!(surviving_pieces(12.0, 18.0, &[(10.0, 20.0)]).is_empty());
    }

    #[test]
    fn previous_keyframe_floors() {
        let kfs = [0.0, 10.0, 20.0, 30.0];
        assert!((previous_keyframe(20.934, &kfs) - 20.0).abs() < 1e-9);
        assert!((previous_keyframe(20.0, &kfs) - 20.0).abs() < 1e-9);
        assert!((previous_keyframe(5.0, &kfs) - 0.0).abs() < 1e-9);
        assert!((previous_keyframe(0.5, &[]) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn next_keyframe_ceils() {
        let kfs = [0.0, 10.0, 20.0, 30.0];
        assert!((next_keyframe(20.934, &kfs) - 30.0).abs() < 1e-9);
        assert!((next_keyframe(20.0, &kfs) - 20.0).abs() < 1e-9);
        assert!((next_keyframe(35.0, &kfs) - 35.0).abs() < 1e-9);
    }

    #[test]
    fn snap_keep_cut_in_to_previous_keyframe() {
        // Matches ffmpeg -ss before -i: intro cut at 20.934 → media starts at KF 20.0
        let keeps = snap_keep_ranges_to_keyframes(&[(20.934, 120.0)], &[0.0, 10.0, 20.0, 30.0, 120.0]);
        assert_eq!(keeps.len(), 1);
        assert!((keeps[0].0 - 20.0).abs() < 1e-9);
        assert!((keeps[0].1 - 120.0).abs() < 1e-9);
    }

    #[test]
    fn snap_keep_cut_out_unchanged() {
        let keeps = snap_keep_ranges_to_keyframes(&[(0.0, 50.5), (60.0, 100.0)], &[0.0, 10.0, 20.0, 48.0, 58.0, 100.0]);
        assert_eq!(keeps.len(), 2);
        assert!((keeps[0].0 - 0.0).abs() < 1e-9);
        assert!((keeps[0].1 - 50.5).abs() < 1e-9); // end not snapped
        assert!((keeps[1].0 - 58.0).abs() < 1e-9); // start floored to prev KF
        assert!((keeps[1].1 - 100.0).abs() < 1e-9);
    }

    #[test]
    fn snap_merges_when_remove_fits_inside_gop() {
        // Remove (50, 52) but nearest keyframe to 52 is 48 → both keeps snap into one
        let keeps = snap_keep_ranges_to_keyframes(&[(0.0, 50.0), (52.0, 100.0)], &[0.0, 48.0, 57.0, 100.0]);
        assert_eq!(keeps.len(), 1);
        assert!((keeps[0].0 - 0.0).abs() < 1e-9);
        assert!((keeps[0].1 - 100.0).abs() < 1e-9);
        let removes = removes_from_keeps(&keeps, 100.0);
        assert!(removes.is_empty());
    }

    #[test]
    fn snap_intervals_to_keyframes_via_removes() {
        let removes = snap_intervals_to_keyframes(&[(0.0, 20.934)], 120.0, &[0.0, 10.0, 20.0, 30.0]);
        assert_eq!(removes.len(), 1);
        assert!((removes[0].0 - 0.0).abs() < 1e-9);
        assert!((removes[0].1 - 20.0).abs() < 1e-9);
    }

    #[test]
    fn nearest_keyframe_picks_closest() {
        let kfs = [0.0, 10.0, 20.0, 30.0];
        assert!((nearest_keyframe(28.9, &kfs) - 30.0).abs() < 1e-9);
        assert!((nearest_keyframe(21.0, &kfs) - 20.0).abs() < 1e-9);
        assert!((nearest_keyframe(25.0, &kfs) - 20.0).abs() < 1e-9); // tie → earlier
        assert!((nearest_keyframe(99.0, &kfs) - 30.0).abs() < 1e-9);
    }

    #[test]
    fn snap_forward_removes_whole_sponsor() {
        // Sponsor 300.2..412.7, GOP 5 s: old "previous KF" rule kept 410..412.7
        // of the sponsor (2.7 s). Nearest → 415, so the removed total is within
        // half a GOP of the SponsorBlock length.
        let kfs: Vec<f64> = (0..=200).map(|i| i as f64 * 5.0).collect();
        let keep = compute_keep_ranges(&[(300.2, 412.7)], 1000.0, 0.5, 0.3).unwrap();
        let snapped = snap_keep_ranges_to_keyframes(&keep, &kfs);
        let removed: f64 = removes_from_keeps(&snapped, 1000.0).iter().map(|(s, e)| e - s).sum();
        assert!((removed - (412.7 - 300.2)).abs() <= 2.5 + 1e-9, "removed={removed}");
        assert!((snapped[1].0 - 415.0).abs() < 1e-9);
    }

    #[test]
    fn snap_noop_without_keyframes() {
        let keeps = [(20.934, 120.0)];
        assert_eq!(snap_keep_ranges_to_keyframes(&keeps, &[]), keeps.to_vec());
    }
}
