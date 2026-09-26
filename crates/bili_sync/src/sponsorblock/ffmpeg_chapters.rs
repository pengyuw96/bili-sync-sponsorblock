//! Embed Matroska/MP4 chapters via ffmpeg ffmetadata + stream-copy remux.
//!
//! For Emby/Jellyfin timeline markers (manual skip), not auto-seek.
//! Base: https://github.com/NeeYoonc/bili-sync-up (v3.1.2)
//! Segment semantics: https://github.com/hanydd/BilibiliSponsorBlock
//!
//! MP4/QuickTime chapters must be contiguous; we fill gaps with 「内容」 chapters
//! so mark titles land at the correct timestamps on both MP4 and Matroska.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::fs;
use tracing::{debug, info};

use crate::downloader::resolve_media_tool_path;

use super::ffmpeg_cut::atomic_replace_with_optional_backup;

/// One chapter to embed (start/end in seconds, Chinese title).
#[derive(Debug, Clone, PartialEq)]
pub struct ChapterMark {
    pub start: f64,
    pub end: f64,
    pub title: String,
}

/// Chinese labels matching the settings UI category names.
pub fn category_chapter_title(category: &str) -> &'static str {
    match category {
        "sponsor" => "赞助广告",
        "padding" => "无意义垫片",
        "selfpromo" => "自我推广",
        "intro" => "片头",
        "outro" => "片尾",
        "interaction" => "互动提醒",
        "preview" => "预告/回顾",
        "filler" => "过场填充",
        "music_offtopic" => "非音乐段落",
        "poi_highlight" => "高光标记",
        _ => "片段标记",
    }
}

const CONTENT_TITLE: &str = "内容";

/// Expand mark segments into a contiguous chapter list covering `[0, duration]`.
///
/// Required for MP4/QuickTime chapter semantics (no gaps). Matroska also accepts this.
pub fn expand_chapters_with_content(marks: &[ChapterMark], duration: f64) -> Vec<ChapterMark> {
    if !(duration.is_finite() && duration > 0.0) {
        return Vec::new();
    }
    let mut marks: Vec<ChapterMark> = marks
        .iter()
        .filter(|c| c.start.is_finite() && c.end.is_finite() && c.end > c.start)
        .map(|c| ChapterMark {
            start: c.start.clamp(0.0, duration),
            end: c.end.clamp(0.0, duration),
            title: c.title.clone(),
        })
        .filter(|c| c.end > c.start)
        .collect();
    marks.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut out = Vec::new();
    let mut cursor = 0.0_f64;
    for m in marks {
        if m.start > cursor + 1e-3 {
            out.push(ChapterMark {
                start: cursor,
                end: m.start,
                title: CONTENT_TITLE.to_string(),
            });
        }
        let start = m.start.max(cursor);
        if m.end > start + 1e-3 {
            out.push(ChapterMark {
                start,
                end: m.end,
                title: m.title,
            });
            cursor = m.end;
        }
    }
    if cursor < duration - 1e-3 {
        out.push(ChapterMark {
            start: cursor,
            end: duration,
            title: CONTENT_TITLE.to_string(),
        });
    }
    out
}

/// Build ffmetadata body with chapters (TIMEBASE=1/1000).
pub fn build_ffmetadata(chapters: &[ChapterMark]) -> String {
    let mut body = String::from(";FFMETADATA1\n");
    for ch in chapters {
        if !(ch.start.is_finite() && ch.end.is_finite()) || ch.end <= ch.start {
            continue;
        }
        let start_ms = (ch.start * 1000.0).round().max(0.0) as i64;
        let end_ms = (ch.end * 1000.0).round().max((start_ms + 1) as f64) as i64;
        let title = escape_ffmetadata_value(&ch.title);
        body.push_str("\n[CHAPTER]\n");
        body.push_str("TIMEBASE=1/1000\n");
        body.push_str(&format!("START={}\n", start_ms));
        body.push_str(&format!("END={}\n", end_ms));
        body.push_str(&format!("title={}\n", title));
    }
    body
}

fn escape_ffmetadata_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '=' | ';' | '#' => {
                out.push('\\');
                out.push(c);
            }
            '\n' | '\r' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// Remux `input` → `output` with chapters from `marks` (stream copy).
///
/// `duration_secs` is used to fill MP4-safe contiguous content chapters.
pub async fn embed_chapters_with_ffmpeg(
    input: &Path,
    output: &Path,
    marks: &[ChapterMark],
    duration_secs: f64,
) -> Result<()> {
    if marks.is_empty() {
        bail!("no chapters to embed");
    }
    if !input.exists() {
        bail!("media file missing: {}", input.display());
    }

    let chapters = expand_chapters_with_content(marks, duration_secs);
    if chapters.is_empty() {
        bail!("no chapters after expand");
    }

    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).await?;

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let temp_dir = parent.join(format!(
        ".bili-sync-sponsor-chapters-{}-{}",
        std::process::id(),
        ts
    ));
    fs::create_dir_all(&temp_dir).await?;

    let meta_path = temp_dir.join("ffmetadata.txt");
    let meta_body = build_ffmetadata(&chapters);
    if let Err(e) = fs::write(&meta_path, meta_body.as_bytes()).await {
        let _ = fs::remove_dir_all(&temp_dir).await;
        return Err(e).context("write ffmetadata");
    }

    let input_str = input.to_string_lossy().to_string();
    let meta_str = meta_path.to_string_lossy().to_string();
    let output_str = output.to_string_lossy().to_string();

    debug!(
        "ffmpeg embed {} mark(s) → {} chapter(s) → {}",
        marks.len(),
        chapters.len(),
        output.display()
    );

    let cmd = tokio::process::Command::new(resolve_media_tool_path("ffmpeg"))
        .args([
            "-y",
            "-i",
            &input_str,
            "-i",
            &meta_str,
            "-map",
            "0",
            "-map_metadata",
            "0",
            "-map_chapters",
            "1",
            "-c",
            "copy",
            &output_str,
        ])
        .output()
        .await;

    let cmd = match cmd {
        Ok(o) => o,
        Err(e) => {
            let _ = fs::remove_dir_all(&temp_dir).await;
            return Err(e).context("spawn ffmpeg chapter embed");
        }
    };

    if !cmd.status.success() {
        let stderr = String::from_utf8_lossy(&cmd.stderr);
        let _ = fs::remove_dir_all(&temp_dir).await;
        bail!("ffmpeg chapter embed error: {}", stderr.trim());
    }

    if !output.exists() {
        let _ = fs::remove_dir_all(&temp_dir).await;
        bail!(
            "ffmpeg chapter embed produced no file: {}",
            output.display()
        );
    }

    let _ = fs::remove_dir_all(&temp_dir).await;
    info!(
        "SponsorBlock chapters embedded ({} marks → {} chapters) → {}",
        marks.len(),
        chapters.len(),
        output.display()
    );
    Ok(())
}

/// Embed chapters into `target` via temp remux + atomic replace (no original backup).
pub async fn embed_chapters_in_place(
    target: &Path,
    marks: &[ChapterMark],
    duration_secs: f64,
) -> Result<()> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let ext = target
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("mp4");
    let tmp_out = parent.join(format!(
        ".bili-sync-sponsor-mark-{}.{}",
        std::process::id(),
        ext
    ));
    if let Err(e) = embed_chapters_with_ffmpeg(target, &tmp_out, marks, duration_secs).await {
        let _ = fs::remove_file(&tmp_out).await;
        return Err(e);
    }
    atomic_replace_with_optional_backup(target, &tmp_out, false, "").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmetadata_contains_chapters() {
        let body = build_ffmetadata(&[
            ChapterMark {
                start: 10.5,
                end: 20.0,
                title: "赞助广告".into(),
            },
            ChapterMark {
                start: 90.0,
                end: 100.0,
                title: "片尾".into(),
            },
        ]);
        assert!(body.starts_with(";FFMETADATA1"));
        assert!(body.contains("[CHAPTER]"));
        assert!(body.contains("START=10500"));
        assert!(body.contains("END=20000"));
        assert!(body.contains("title=赞助广告"));
        assert!(body.contains("title=片尾"));
    }

    #[test]
    fn category_titles_match_ui() {
        assert_eq!(category_chapter_title("sponsor"), "赞助广告");
        assert_eq!(category_chapter_title("intro"), "片头");
        assert_eq!(category_chapter_title("outro"), "片尾");
    }

    #[test]
    fn expand_fills_gaps_for_mp4() {
        let marks = vec![
            ChapterMark {
                start: 2.0,
                end: 4.0,
                title: "赞助广告".into(),
            },
            ChapterMark {
                start: 7.0,
                end: 9.0,
                title: "片尾".into(),
            },
        ];
        let chapters = expand_chapters_with_content(&marks, 10.0);
        assert_eq!(chapters.len(), 5);
        assert_eq!(chapters[0].title, "内容");
        assert!((chapters[0].start - 0.0).abs() < 1e-6);
        assert!((chapters[0].end - 2.0).abs() < 1e-6);
        assert_eq!(chapters[1].title, "赞助广告");
        assert_eq!(chapters[2].title, "内容");
        assert_eq!(chapters[3].title, "片尾");
        assert_eq!(chapters[4].title, "内容");
        assert!((chapters[4].end - 10.0).abs() < 1e-6);
    }
}
