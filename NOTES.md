# Notes — 3.1.5.1

**Version string shown in the web UI:** `v3.1.5.1`  
Cargo package version is `3.1.5` because Cargo rejects a fourth numeric component. Upstream code baseline is **v3.1.5** (`a73c2fca`, 2026-10-14), including the v3.1.4 manga source. SponsorBlock can now be toggled per video source (`sponsorblock_enabled`, default on); the global `sponsor_block.enabled` stays the master switch. Previous published tag remains `v3.1.2-sponsorblock-mark`.  
This is a **community fork** modification. It is **not** an official upstream release.

## Feature

After a successful Bilibili page media merge, if `sponsor_block.enabled` is true, the downloader:

1. Fetches skip segments from BilibiliSponsorBlock (`/api/skipSegments?videoID=&cid=&categories=&actionTypes=`), with the union of **cut** + **mark** categories.
2. Filters `actionType == skip` matching configured categories and page `cid`.
3. **Cut categories** (`categories`): merges overlapping remove intervals, inverts to keep ranges, drops tiny keep gaps, **snaps each keep cut-in to the previous video keyframe** (matches ffmpeg `-ss` before `-i` + `-c copy`), cuts with stream-copy + concat (old chapters stripped via `-map_chapters -1`), then atomically replaces the merged file. Subtitle/danmaku/`sponsorblock.json` all use those **same snapped remove intervals** so A/V and sidecars share one timeline. After a successful cut, ffprobe measures the real duration for chapter END times, sibling `{stem}.nfo` `<runtime>`, and `page.duration`. `cut_mode = "accurate"` re-encodes (libx264/aac) for frame-exact cuts and falls back to keyframe copy on failure.
4. **Mark categories** (`mark_categories`): embeds Matroska/MP4 chapters (Chinese titles) so Emby/Jellyfin show timeline markers for **manual** next-chapter skip. The same label is injected into existing `.srt` files, a dedicated `{stem}.sponsorblock.zh.srt`, and a top danmaku line `〔SB〕{title}` (ASS Name `bsync-sb`). Emby Intro Skip does not read these chapter titles and will not auto-skip them.
5. The original-timeline plan is the hidden `.{stem}.sponsorblock.json`; it records which sidecars were already shifted, so a later danmaku refresh or subtitle rewrite is shifted once, not twice. No uncut original is kept: the cut file atomically replaces the download, and legacy `*.sb-orig` copies from older builds are used once for migration and then deleted. A sidecar rewrite failure is logged and does not fail the page.
6. Same category must not be in both lists — **cut wins** (server + UI mutual exclusion).
7. Fail-open by default (API/ffmpeg errors keep the media file). Disabled / no segments / chapter-split skip / would-remove-all do not touch sidecars.

YouTube / TikTok / Douyin paths are unchanged (not wired).

## Enable

Set config key `sponsor_block` (JSON object), for example:

```json
{
  "enabled": true,
  "server_address": "https://www.bsbsb.top",
  "mirror_server_addresses": ["https://www.bsbsb.xyz"],
  "categories": ["sponsor", "padding"],
  "mark_categories": ["intro", "outro"],
  "action_types": ["skip"],
  "cut_mode": "keyframe",
  "min_segment_seconds": 0.5,
  "min_keep_gap_seconds": 0.3,
  "api_timeout_ms": 10000,
  "fail_open": true,
  "heartbeat_interval_secs": 300
}
```

Default: `enabled: false`, `categories: ["sponsor","padding"]`, `mark_categories: []` (existing cut behavior unchanged).

## Heartbeat

Probes `GET {server}/api/status` (primary + mirrors). Does **not** block downloads.

- On startup when enabled: one log heartbeat
- Periodic every `heartbeat_interval_secs` (default 300; `0` disables periodic)
- API: authenticated `GET /api/sponsorblock/health`
- Web settings: status line + 「检测」button

## Attribution

- Base project: https://github.com/NeeYoonc/bili-sync-up (v3.1.3)
- API / segment semantics: https://github.com/hanydd/BilibiliSponsorBlock
- API wiki: https://github.com/hanydd/BilibiliSponsorBlock/wiki/API

## Caveats

- Emby has no SponsorBlock skip button. Cut removes the segment from the file. Mark is a chapter plus a visible `〔SB〕` cue; use next chapter or the chapter menu.
- Only `actionType == skip` is applied. YouTube / TikTok / Douyin stay unwired.
- Re-download cleanup deletes `.{stem}.sponsorblock.json` (and any legacy `*.sb-orig`) with the other page sidecars, so a stale plan cannot shift a new download.
- **Keyframe snap:** stream-copy cuts cannot start mid-GOP. Cut-ins move to the previous keyframe (may keep a short tail of the sponsor). A remove shorter than the surrounding GOP may collapse to a no-op after snap. Sidecars follow the snapped plan, not the raw API timestamps — this is what fixes subtitle/danmaku skew.

## Mutual exclusion with chapter split

If the video source has `split_chapters_after_download` enabled, **both cut and mark are skipped** (`skipped_chapters`) so SponsorBlock does not fight player-chapter split.

## Page acceptance: slot 6「视频剪切」

`PageStatus` is now **6** slots (VideoStatus remains 5):

0 视频封面 · 1 视频内容 · 2 视频信息/NFO · 3 视频弹幕 · 4 视频字幕 · **5 视频剪切 (SponsorBlock)**

Detail API exposes `sponsor_cut_result` (English codes):

| code | meaning |
|------|---------|
| `disabled` | `sponsor_block.enabled=false` (or audio-only / N/A) |
| `none` | API compared, no matching skip segments |
| `cut` | ffmpeg cut applied; subtitle and danmaku timelines rewritten |
| `marked` | chapters embedded (no cut); subtitle and danmaku markers injected |
| `cut_and_marked` | cut applied, then chapters embedded (remapped); sidecars follow the cut file |
| `skipped_chapters` | mutual exclusion with chapter split |
| `would_remove_all` | would cut ~100%, kept original |
| `failed_open` | error but fail_open kept original |
| `error` | hard error (`fail_open=false`) |

Migration `m20260926_000001_add_page_sponsor_cut_result` adds the column and clears completed bit31 on pages whose slot5==0 (and reopens parent videos) so existing libraries re-run only the new cut slot. Cite: [BilibiliSponsorBlock](https://github.com/hanydd/BilibiliSponsorBlock).

## Chapter embedding notes

Mark segments are expanded into a **contiguous** chapter list (gaps filled with 「内容」) so MP4/QuickTime chapter semantics stay valid; Matroska accepts the same layout. Emby/Jellyfin then show named segments (赞助广告 / 片头 / …) on the timeline for manual jump. Chapter times use the complement of the **keyframe-snapped** keep ranges ffmpeg actually wrote, and the chapter list END is the ffprobe-measured post-cut duration when available.

Subtitle marker text is `〔SB〕{title} · 下一章节可跳过`. Danmaku marker text is the shorter `〔SB〕{title}` on style Top. Shifted danmaku dialogues set Effect to `sb`; the Name field stays `bsync-dm|source_id|sent_at` so the incremental cursor still parses.

## Verify chapters

After a mark (or cut_and_marked) download:

```bash
ffprobe -show_chapters -print_format json /path/to/media.mp4
# or
ffmpeg -i /path/to/media.mp4 2>&1 | grep -A2 Chapter
```


## 2026-10-12: no uncut originals, accurate cut points, one media path

- Removed `sponsor_block.keep_original` / `original_suffix` (old config keys are ignored on load).
  A second `*.sponsor-original.mp4` / `*.sb-orig` next to the cut file made Emby show an
  empty detail page (0 items) and no poster. Only the cut video + its NFO/thumb/subs remain.
  Sidecars no longer need `*.sb-orig`: danmaku lines carry Effect `sb` once shifted; SRTs are
  tracked by hash in `{stem}.sponsorblock.json`. Legacy `.sb-orig` files are consumed once and deleted.
  Cleanup of existing files: `deploy/cleanup-sb-orig.sh` (dry run by default).
- ~2 s offset root causes: (1) `-ss` was printed with 3 decimals, so a keyframe at e.g. 20.0213
  became 20.021 and the copy seek fell back one whole GOP; (2) keyframe pts were absolute while
  input `-ss` is relative to container start_time; (3) cut-in always snapped to the *previous*
  keyframe, keeping up to one GOP of every sponsor; (4) the cut timeline used the API's whole-second
  duration. Fixes: µs `-ss`, keyframes normalised by start_time, nearest-keyframe snap (≤ half GOP),
  ffprobe source duration.
- New `sponsor_block.cut_mode`: `keyframe` (default, stream copy, fast) or `accurate`
  (libx264 crf18 veryfast + aac re-encode, frame-exact, CPU heavy; falls back to keyframe on failure).
- Canonical in-container media path is `/bili-sync` (UI placeholders + compose). NAS compose
  `deploy/docker-compose.nas.yml` mounts only `/bili-sync`; NAS DB paths were migrated from
  `/media/bili-sync` with a backup. Creating a source under an unmounted root now returns a clear hint.

## 2026-10-12: one stem per single-page video (Emby covers)

- Single-page submissions that belong to a UGC season no longer get `S01E27 - title`
  names unless `collection_folder_mode = up_seasonal`; every file in the video folder
  shares the video stem (`title.mp4`, `title-thumb.jpg`, `title-fanart.jpg`, `title.nfo`, subs).
- Folder-level art is skipped when the prefix is empty (previously `-thumb.jpg` / `-fanart.jpg`).
  The earlier poster.jpg change was reverted.
- SponsorBlock plan is now hidden: `.{stem}.sponsorblock.json` (legacy visible name is renamed on access).
- `deploy/migrate-episode-names.sh` renames existing folders and emits page.path SQL.

## Smart cut (beta) — HEVC / iGPU

- **Feasible** for H.264 and **closed-GOP** H.265 Main 8-bit via MPEG-TS concat of re-encoded boundary slivers + stream-copied mid. Open-GOP H.265 mid-copy leaves dangling POCs; decode check fails → automatic stable fallback.
- **Encoder order (H.265):** `hevc_qsv` → `hevc_vaapi` → `libx265` veryfast. Runtime probe; no `/dev/dri` → software.
- **NAS container (not applied yet):** add device `/dev/dri` (e.g. `- /dev/dri:/dev/dri`), ensure the process user can open `renderD*` (often group `render` / `video`). Image ffmpeg must list `hevc_qsv` or `hevc_vaapi` (Alpine 3.22 stock ffmpeg 6.1.2 links libx265 + libvpl; confirm QSV inside the running container before relying on HW).
- **Do not bump to ffmpeg 8** for this feature alone: Alpine 3.22 pins 6.1.2; edge 8.x is a distro jump. Stream-copy/concat are I/O-bound; short re-encodes are encoder-bound (x265 / QSV), not demuxer-version-bound.


### Smart cut fixes (2026-10-15, NAS test round 1)
- Keyframe probe: `-skip_frame nokey -show_entries frame=pts_time -of csv` printed
  `0.000000,` (extra column) on ffmpeg 6.1.2 → first keyframe dropped (H.264) or the
  whole list empty (Bilibili HEVC). Now packet flags via `compact` key=value
  (`packet=pts_time,dts_time,flags`, K rows, dts fallback). Affects stable snap too.
- Joins: pieces now carry absolute timestamps (`-output_ts_offset`, mux delay 0, +1 s
  base) and are joined with the concat *protocol*; next offset = last piece PTS + 1
  frame. Copy pieces select frames by PTS window (`noise=drop=`) instead of
  `-frames:v`, drop open-GOP leading pictures, and get SPS/PPS via `dump_extra`.
- Open GOP (x264 open-gop non-IDR I, HEVC CRA, detected via `trace_headers` on
  keyframes + leading-picture scan) is never a copy start; if that pushes re-encode
  above 50 % of the keep, smart cut refuses → stable.

## run as 2601:1113 via root-only entrypoint
- `/app/entrypoint.sh` (deploy/qnap-entrypoint.sh): as root, `chgrp $PGID /dev/dri/renderD*` + `chmod 660` on the container's own
  device node (docker `--device` creates a separate node; host node stays 600 admin:administrators), `umask 002`, then
  `exec su-exec $PUID:$PGID /app/bili-sync-rs`. Files are created as 2601:1113 directly.
- Run with `--user 0:0 --cap-drop ALL --cap-add CHOWN --cap-add FOWNER --cap-add SETUID --cap-add SETGID --security-opt no-new-privileges`.
  After su-exec the app has no effective caps. The PUID/PGID ownership fixer is a no-op when not root.
