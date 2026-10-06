//! Keep subtitles and danmaku on the same timeline as a SponsorBlock cut,
//! and insert visible skip markers into both.
//!
//! The original subtitle/danmaku bytes are kept beside the playback file
//! (`*.sb-orig`) so a later danmaku refresh or subtitle rewrite can be
//! re-applied without shifting twice. Playback files are what Emby reads.
//!
//! Marker cues use the prefix `〔SB〕`. Danmaku lines already shifted use ASS
//! Effect `sb` (libass ignores unknown effects). Marker danmaku uses Name
//! `bsync-sb`, which the incremental cursor does not treat as a real comment.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::info;

use super::timeline::{remap_mark_interval, surviving_pieces};

const MARKER_PREFIX: &str = "〔SB〕";
const MARKER_NAME: &str = "bsync-sb";
const SHIFTED_EFFECT: &str = "sb";
const PLAN_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PlanMark {
    start: f64,
    end: f64,
    title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SponsorPlan {
    version: u32,
    removes: Vec<(f64, f64)>,
    marks: Vec<PlanMark>,
}

#[derive(Debug, Clone, PartialEq)]
struct Cue {
    start: f64,
    end: f64,
    text: String,
}

/// Persist the cut/mark plan next to the media file and rewrite sidecars once.
pub async fn save_and_apply(
    media_path: &Path,
    danmaku_path: &Path,
    subtitle_stem: &Path,
    removes: &[(f64, f64)],
    marks: &[(f64, f64, String)],
) -> Result<()> {
    let plan = SponsorPlan {
        version: PLAN_VERSION,
        removes: removes.to_vec(),
        marks: marks
            .iter()
            .filter(|(_, end, _)| end.is_finite())
            .map(|(start, end, title)| PlanMark {
                start: *start,
                end: *end,
                title: title.clone(),
            })
            .collect(),
    };
    let plan_path = plan_path_for_media(media_path);
    write_replace(&plan_path, &serde_json::to_string_pretty(&plan)?).await?;
    apply_plan(media_path, danmaku_path, subtitle_stem, &plan).await
}

/// After a danmaku rewrite/append, shift any new original-timeline lines.
pub async fn reapply_danmaku_after_write(danmaku_path: &Path) -> Result<bool> {
    let Some(plan_path) = plan_path_near_danmaku(danmaku_path) else {
        return Ok(false);
    };
    if !plan_path.exists() || !danmaku_path.exists() {
        return Ok(false);
    }
    let plan = read_plan(&plan_path).await?;
    apply_danmaku(danmaku_path, &plan).await?;
    Ok(true)
}

/// After subtitles are written again, rebuild them from the saved original.
pub async fn reapply_subtitles_after_write(subtitle_stem: &Path) -> Result<bool> {
    let plan_path = plan_path_for_media(subtitle_stem);
    if !plan_path.exists() {
        return Ok(false);
    }
    let plan = read_plan(&plan_path).await?;
    apply_subtitles(subtitle_stem, &plan).await?;
    Ok(true)
}

async fn apply_plan(media_path: &Path, danmaku_path: &Path, subtitle_stem: &Path, plan: &SponsorPlan) -> Result<()> {
    if danmaku_path.exists() {
        apply_danmaku(danmaku_path, plan).await?;
    }
    apply_subtitles(subtitle_stem, plan).await?;
    info!(
        "SponsorBlock sidecars updated for {} ({} remove ranges, {} marks)",
        media_path.display(),
        plan.removes.len(),
        plan.marks.len()
    );
    Ok(())
}

fn plan_path_for_media(media_path: &Path) -> PathBuf {
    let stem = media_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("media");
    let parent = media_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{stem}.sponsorblock.json"))
}

fn plan_path_near_danmaku(danmaku_path: &Path) -> Option<PathBuf> {
    let name = danmaku_path.file_name()?.to_str()?;
    let base = name.strip_suffix(".zh-CN.default.ass")?;
    Some(danmaku_path.with_file_name(format!("{base}.sponsorblock.json")))
}

async fn read_plan(path: &Path) -> Result<SponsorPlan> {
    let text = fs::read_to_string(path)
        .await
        .with_context(|| format!("read sponsor plan {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse sponsor plan {}", path.display()))
}

async fn apply_subtitles(subtitle_stem: &Path, plan: &SponsorPlan) -> Result<()> {
    let Some(parent) = subtitle_stem.parent() else {
        return Ok(());
    };
    if !parent.exists() {
        return Ok(());
    }
    let stem = subtitle_stem
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("media");
    let prefix = format!("{stem}.");
    let marker_name = format!("{stem}.sponsorblock.zh.srt");

    let mut dir = fs::read_dir(parent).await?;
    let mut subtitle_files = Vec::new();
    while let Some(entry) = dir.next_entry().await? {
        let path = entry.path();
        if !entry.file_type().await?.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with(&prefix) || name.contains(".sponsorblock.") {
            continue;
        }
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if ext.eq_ignore_ascii_case("srt") {
            subtitle_files.push(path);
        }
    }

    for path in subtitle_files {
        rewrite_subtitle_file(&path, plan).await?;
    }

    let marker_path = parent.join(marker_name);
    if plan.marks.is_empty() {
        if marker_path.exists() {
            let _ = fs::remove_file(&marker_path).await;
        }
    } else {
        let body = render_srt(&marker_cues(&plan.marks, &plan.removes));
        write_replace(&marker_path, &body).await?;
    }
    Ok(())
}

async fn rewrite_subtitle_file(path: &Path, plan: &SponsorPlan) -> Result<()> {
    let current = fs::read_to_string(path)
        .await
        .with_context(|| format!("read subtitle {}", path.display()))?;
    let orig_path = sidecar_orig_path(path);
    let source = if orig_path.exists() {
        let orig = fs::read_to_string(&orig_path).await?;
        let rendered = transform_srt(&orig, &plan.removes, &plan.marks);
        if current == orig || current == rendered {
            orig
        } else {
            write_replace(&orig_path, &current).await?;
            current.clone()
        }
    } else {
        write_replace(&orig_path, &current).await?;
        current.clone()
    };
    let rendered = transform_srt(&source, &plan.removes, &plan.marks);
    if rendered != current {
        write_replace(path, &rendered).await?;
    }
    Ok(())
}

async fn apply_danmaku(danmaku_path: &Path, plan: &SponsorPlan) -> Result<()> {
    let visible = fs::read_to_string(danmaku_path)
        .await
        .with_context(|| format!("read danmaku {}", danmaku_path.display()))?;
    let orig_path = sidecar_orig_path(danmaku_path);
    let original = if !orig_path.exists() {
        if ass_has_shifted_dialogue(&visible) {
            tracing::warn!(
                "SponsorBlock: danmaku {} is already shifted but the original copy is missing; refreshing markers only",
                danmaku_path.display()
            );
            strip_marker_dialogues(&visible)
        } else {
            let original = strip_marker_dialogues(&visible);
            write_replace(&orig_path, &original).await?;
            original
        }
    } else if ass_has_shifted_dialogue(&visible) {
        let mut original = fs::read_to_string(&orig_path).await?;
        original = append_new_original_dialogues(&original, &visible);
        write_replace(&orig_path, &original).await?;
        original
    } else {
        let original = strip_marker_dialogues(&visible);
        write_replace(&orig_path, &original).await?;
        original
    };
    let rendered = render_ass(&original, &plan.removes, &plan.marks);
    if rendered != visible {
        write_replace(danmaku_path, &rendered).await?;
    }
    Ok(())
}

fn sidecar_orig_path(path: &Path) -> PathBuf {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("sidecar");
    path.with_file_name(format!("{name}.sb-orig"))
}

async fn write_replace(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).await?;
        }
    }
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("sidecar");
    let tmp = path.with_file_name(format!(".{file_name}.sb-tmp"));
    fs::write(&tmp, body)
        .await
        .with_context(|| format!("write temp sidecar {}", tmp.display()))?;
    if fs::rename(&tmp, path).await.is_err() {
        if path.exists() {
            fs::remove_file(path)
                .await
                .with_context(|| format!("replace sidecar {}", path.display()))?;
        }
        fs::rename(&tmp, path)
            .await
            .with_context(|| format!("rename sidecar onto {}", path.display()))?;
    }
    Ok(())
}

fn transform_srt(input: &str, removes: &[(f64, f64)], marks: &[PlanMark]) -> String {
    let mut cues = Vec::new();
    for cue in parse_srt(input) {
        if is_marker_text(&cue.text) {
            continue;
        }
        for (start, end) in surviving_pieces(cue.start, cue.end, removes) {
            cues.push(Cue {
                start,
                end,
                text: cue.text.clone(),
            });
        }
    }
    cues.extend(marker_cues(marks, removes));
    cues.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
    render_srt(&cues)
}

fn marker_cues(marks: &[PlanMark], removes: &[(f64, f64)]) -> Vec<Cue> {
    let mut cues = Vec::new();
    for mark in marks {
        let Some((start, end)) = remap_mark_interval(mark.start, mark.end, removes, f64::MAX) else {
            continue;
        };
        let Some((start, end)) = marker_span(start, end) else {
            continue;
        };
        cues.push(Cue {
            start,
            end,
            text: format!("{MARKER_PREFIX}{} · 下一章节可跳过", mark.title),
        });
    }
    cues
}

fn marker_span(start: f64, end: f64) -> Option<(f64, f64)> {
    let len = end - start;
    if len < 0.2 {
        return None;
    }
    Some((start, start + len.min(3.5)))
}

fn is_marker_text(text: &str) -> bool {
    text.trim_start().starts_with(MARKER_PREFIX)
}

fn parse_srt(input: &str) -> Vec<Cue> {
    let text = input.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = text.lines().collect();
    let mut cues = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some((start, end)) = parse_arrow_line(lines[i]) {
            i += 1;
            let mut body = Vec::new();
            while i < lines.len() && !lines[i].is_empty() {
                if parse_arrow_line(lines[i]).is_some() {
                    break;
                }
                body.push(lines[i]);
                i += 1;
            }
            if end > start {
                cues.push(Cue {
                    start,
                    end,
                    text: body.join("\n"),
                });
            }
        } else {
            i += 1;
        }
    }
    cues
}

fn parse_arrow_line(line: &str) -> Option<(f64, f64)> {
    let (left, right) = line.split_once("-->")?;
    let start = parse_clock(left)?;
    let end_token = right.split_whitespace().next().unwrap_or(right);
    let end = parse_clock(end_token)?;
    Some((start, end))
}

fn parse_clock(raw: &str) -> Option<f64> {
    let raw = raw.trim();
    let mut parts = raw.split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.replace(',', ".").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

fn render_srt(cues: &[Cue]) -> String {
    let mut out = String::new();
    for (index, cue) in cues.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n",
            index + 1,
            format_srt_time(cue.start),
            format_srt_time(cue.end),
            cue.text
        ));
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn format_srt_time(t: f64) -> String {
    let total_ms = (t.max(0.0) * 1000.0).round() as u64;
    let ms = total_ms % 1000;
    let total_s = total_ms / 1000;
    let s = total_s % 60;
    let total_m = total_s / 60;
    let m = total_m % 60;
    let h = total_m / 60;
    format!("{h:02}:{m:02}:{s:02},{ms:03}")
}

fn format_ass_time(t: f64) -> String {
    let t = t.max(0.0);
    let secs = t.floor() as u32;
    let hour = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let left = t - f64::from(hour * 3600) - f64::from(minutes * 60);
    format!("{hour}:{minutes:02}:{left:05.2}")
}

fn render_ass(original: &str, removes: &[(f64, f64)], marks: &[PlanMark]) -> String {
    let text = original.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Dialogue:") {
            if let Some(fields) = split_ass_fields(rest) {
                if is_marker_dialogue(&fields) {
                    continue;
                }
                for (start, end) in surviving_pieces(parse_clock(&fields[1]).unwrap_or(0.0), parse_clock(&fields[2]).unwrap_or(0.0), removes)
                {
                    let mut shifted = fields.clone();
                    shifted[1] = format_ass_time(start);
                    shifted[2] = format_ass_time(end);
                    shifted[8] = SHIFTED_EFFECT.to_string();
                    out.push_str(&format_dialogue(&shifted));
                    out.push('\n');
                }
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    for cue in marker_cues(marks, removes) {
        out.push_str(&format!(
            "Dialogue: 5,{},{},Top,{MARKER_NAME},0,0,0,,{MARKER_PREFIX}{}\n",
            format_ass_time(cue.start),
            format_ass_time(cue.end),
            cue.text.trim_start_matches(MARKER_PREFIX).trim_end_matches(" · 下一章节可跳过")
        ));
    }
    out
}

fn strip_marker_dialogues(input: &str) -> String {
    let text = input.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Dialogue:") {
            if let Some(fields) = split_ass_fields(rest) {
                if is_marker_dialogue(&fields) {
                    continue;
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn ass_has_shifted_dialogue(input: &str) -> bool {
    input.lines().any(|line| {
        line.strip_prefix("Dialogue:")
            .and_then(split_ass_fields)
            .is_some_and(|fields| fields.get(8).is_some_and(|effect| effect.trim() == SHIFTED_EFFECT))
    })
}

fn append_new_original_dialogues(original: &str, visible: &str) -> String {
    let mut known = dialogue_identities(original);
    let mut extra = Vec::new();
    for line in visible.lines() {
        let Some(rest) = line.strip_prefix("Dialogue:") else {
            continue;
        };
        let Some(fields) = split_ass_fields(rest) else {
            continue;
        };
        if is_marker_dialogue(&fields) || fields.get(8).is_some_and(|e| e.trim() == SHIFTED_EFFECT) {
            continue;
        }
        let identity = dialogue_identity(&fields);
        if known.insert(identity) {
            extra.push(format_dialogue(&fields));
        }
    }
    if extra.is_empty() {
        return original.replace("\r\n", "\n").replace('\r', "\n");
    }
    let mut out = original.replace("\r\n", "\n").replace('\r', "\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for line in extra {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn dialogue_identities(input: &str) -> std::collections::HashSet<String> {
    let mut ids = std::collections::HashSet::new();
    for line in input.lines() {
        if let Some(fields) = line.strip_prefix("Dialogue:").and_then(split_ass_fields) {
            if !is_marker_dialogue(&fields) {
                ids.insert(dialogue_identity(&fields));
            }
        }
    }
    ids
}

fn dialogue_identity(fields: &[String]) -> String {
    let name = fields.get(4).map(|s| s.trim()).unwrap_or("");
    if !name.is_empty() {
        return name.to_string();
    }
    format!(
        "{}|{}|{}",
        fields.get(1).map(String::as_str).unwrap_or(""),
        fields.get(2).map(String::as_str).unwrap_or(""),
        fields.get(9).map(String::as_str).unwrap_or("")
    )
}

fn is_marker_dialogue(fields: &[String]) -> bool {
    fields.get(4).is_some_and(|name| name.trim() == MARKER_NAME)
        || fields.get(9).is_some_and(|text| is_marker_text(text))
}

fn split_ass_fields(rest: &str) -> Option<Vec<String>> {
    let rest = rest.trim_start();
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut commas = 0;
    for ch in rest.chars() {
        if ch == ',' && commas < 9 {
            fields.push(std::mem::take(&mut current));
            commas += 1;
        } else {
            current.push(ch);
        }
    }
    fields.push(current);
    if fields.len() < 10 {
        return None;
    }
    Some(fields)
}

fn format_dialogue(fields: &[String]) -> String {
    format!(
        "Dialogue: {},{},{},{},{},{},{},{},{},{}",
        fields[0].trim(),
        fields[1],
        fields[2],
        fields[3],
        fields[4],
        fields[5],
        fields[6],
        fields[7],
        fields[8],
        fields[9]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(start: f64, end: f64, title: &str) -> PlanMark {
        PlanMark {
            start,
            end,
            title: title.to_string(),
        }
    }

    #[test]
    fn srt_shifts_and_drops_cut_cue() {
        let input = "\
1
00:00:01,000 --> 00:00:04,000
片头里的字

2
00:00:12,000 --> 00:00:15,000
广告词

3
00:00:25,000 --> 00:00:28,000
正文
";
        let out = transform_srt(input, &[(10.0, 20.0)], &[mark(30.0, 40.0, "片尾")]);
        assert!(!out.contains("广告词"));
        assert!(out.contains("片头里的字"));
        assert!(out.contains("00:00:15,000 --> 00:00:18,000"));
        assert!(out.contains("正文"));
        assert!(out.contains("〔SB〕片尾 · 下一章节可跳过"));
    }

    #[test]
    fn srt_marker_only_keeps_original_times() {
        let input = "\
1
00:00:02,000 --> 00:00:03,000
你好
";
        let out = transform_srt(input, &[], &[mark(2.0, 8.0, "片头")]);
        assert!(out.contains("00:00:02,000 --> 00:00:03,000"));
        assert!(out.contains("你好"));
        assert!(out.contains("〔SB〕片头 · 下一章节可跳过"));
    }

    #[test]
    fn ass_dialogue_is_shifted_once_and_marked() {
        let original = "\
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 2,0:00:12.00,0:00:14.00,Float,bsync-dm|1|10,0,0,0,,广告弹幕
Dialogue: 2,0:00:25.00,0:00:27.00,Float,bsync-dm|2|11,0,0,0,,正文弹幕
";
        let rendered = render_ass(original, &[(10.0, 20.0)], &[mark(0.0, 5.0, "片头")]);
        assert!(!rendered.contains("广告弹幕"));
        assert!(rendered.contains("Dialogue: 2,0:00:15.00,0:00:17.00,Float,bsync-dm|2|11,0,0,0,sb,正文弹幕"));
        assert!(rendered.contains("Dialogue: 5,0:00:00.00,0:00:03.50,Top,bsync-sb,0,0,0,,〔SB〕片头"));
        assert_eq!(rendered.matches("正文弹幕").count(), 1);
    }

    #[test]
    fn append_keeps_new_original_line_only() {
        let original = "\
Dialogue: 2,0:00:25.00,0:00:27.00,Float,bsync-dm|2|11,0,0,0,,正文弹幕
";
        let visible = "\
Dialogue: 2,0:00:05.00,0:00:07.00,Float,bsync-dm|2|11,0,0,0,sb,正文弹幕
Dialogue: 2,0:00:30.00,0:00:32.00,Float,bsync-dm|3|12,0,0,0,,新弹幕
Dialogue: 5,0:00:00.00,0:00:03.50,Top,bsync-sb,0,0,0,,〔SB〕片头
";
        let merged = append_new_original_dialogues(original, visible);
        assert!(merged.contains("bsync-dm|2|11"));
        assert!(merged.contains("新弹幕"));
        assert!(!merged.contains("bsync-sb"));
        assert_eq!(merged.matches("新弹幕").count(), 1);
        let merged_again = append_new_original_dialogues(&merged, visible);
        assert_eq!(merged_again.matches("新弹幕").count(), 1);
    }
}
