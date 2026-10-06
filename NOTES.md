# Notes — 3.1.4.1

**Version string shown in the web UI:** `v3.1.4.1`  
Cargo package version is `3.1.4` because Cargo rejects a fourth numeric component. Upstream code baseline is still **v3.1.3** (`f5a91a975ca311a065864a613860bb61d963c090`, published 2026-10-03). Upstream v3.1.4 / v3.1.4.1 (manga source and the duplicate-source patch) are not merged. Previous published tag remains `v3.1.2-sponsorblock-mark`.  
This is a **community fork** modification. It is **not** an official upstream release.

## Feature

After a successful Bilibili page media merge, if `sponsor_block.enabled` is true, the downloader:

1. Fetches skip segments from BilibiliSponsorBlock (`/api/skipSegments?videoID=&cid=&categories=&actionTypes=`), with the union of **cut** + **mark** categories.
2. Filters `actionType == skip` matching configured categories and page `cid`.
3. **Cut categories** (`categories`): merges overlapping remove intervals, inverts to keep ranges, drops tiny keep gaps, cuts with ffmpeg stream-copy + concat, then atomically replaces the merged file. Subtitle cues and danmaku dialogues are rewritten onto that post-cut timeline (short absorbed gaps included).
4. **Mark categories** (`mark_categories`): embeds Matroska/MP4 chapters (Chinese titles) so Emby/Jellyfin show timeline markers for **manual** next-chapter skip. The same label is injected into existing `.srt` files, a dedicated `{stem}.sponsorblock.zh.srt`, and a top danmaku line `〔SB〕{title}` (ASS Name `bsync-sb`). Emby Intro Skip does not read these chapter titles and will not auto-skip them.
5. The original-timeline plan is `{stem}.sponsorblock.json`. Playback originals are kept as `*.sb-orig` so a later danmaku refresh or subtitle rewrite is shifted once, not twice. A sidecar rewrite failure is logged and does not fail the page.
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
  "keep_original": false,
  "original_suffix": ".sponsor-original",
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
- Re-download cleanup deletes `*.sb-orig` and `{stem}.sponsorblock.json` with the other page sidecars, so a stale plan cannot shift a new download.

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

Mark segments are expanded into a **contiguous** chapter list (gaps filled with 「内容」) so MP4/QuickTime chapter semantics stay valid; Matroska accepts the same layout. Emby/Jellyfin then show named segments (赞助广告 / 片头 / …) on the timeline for manual jump. Chapter times use the complement of the keep ranges ffmpeg actually wrote, so they match the file after short gaps are absorbed.

Subtitle marker text is `〔SB〕{title} · 下一章节可跳过`. Danmaku marker text is the shorter `〔SB〕{title}` on style Top. Shifted danmaku dialogues set Effect to `sb`; the Name field stays `bsync-dm|source_id|sent_at` so the incremental cursor still parses.

## Verify chapters

After a mark (or cut_and_marked) download:

```bash
ffprobe -show_chapters -print_format json /path/to/media.mp4
# or
ffmpeg -i /path/to/media.mp4 2>&1 | grep -A2 Chapter
```

