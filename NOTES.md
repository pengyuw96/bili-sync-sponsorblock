# Local notes — 3.1.0+sponsorblock (audit package)

**Version string for this audit package:** `3.1.0+sponsorblock`  
Upstream tag remains **v3.1.0** (`8019f983cd5978eab89ae25cabdbd46c4bbe35b8`).  
This is a **local** modification for user audit. It is **not** an official upstream release.

## Feature

After a successful Bilibili page media merge, if `sponsor_block.enabled` is true, the downloader:

1. Fetches skip segments from BilibiliSponsorBlock (`/api/skipSegments?videoID=&cid=&categories=&actionTypes=`).
2. Filters `actionType == skip` matching configured categories and page `cid`.
3. Merges overlapping remove intervals, inverts to keep ranges, drops tiny keep gaps.
4. Cuts with ffmpeg stream-copy + concat, then atomically replaces the merged file.
5. Fail-open by default (API/ffmpeg errors keep the uncut file).

YouTube / TikTok / Douyin paths are unchanged (not wired).

## Enable

Set config key `sponsor_block` (JSON object), for example:

```json
{
  "enabled": true,
  "server_address": "https://www.bsbsb.top",
  "mirror_server_addresses": ["https://www.bsbsb.xyz"],
  "categories": ["sponsor", "padding"],
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

Default: `enabled: false`.

## Heartbeat

Probes `GET {server}/api/status` (primary + mirrors). Does **not** block downloads.

- On startup when enabled: one log heartbeat
- Periodic every `heartbeat_interval_secs` (default 300; `0` disables periodic)
- API: authenticated `GET /api/sponsorblock/health`
- Web settings: status line + 「检测」button


## Attribution

- Base project: https://github.com/NeeYoonc/bili-sync-up (v3.1.0)
- API / segment semantics: https://github.com/hanydd/BilibiliSponsorBlock
- API wiki: https://github.com/hanydd/BilibiliSponsorBlock/wiki/API

## Caveats

See `/workspace/bili-sponsor-integration/AUDIT.md`.
