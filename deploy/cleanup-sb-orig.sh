#!/bin/sh
# Remove uncut originals left by older SponsorBlock builds. NOT run automatically.
# Usage (inside container or on NAS host with the host path):
#   sh cleanup-sb-orig.sh /bili-sync          # dry run, lists files
#   sh cleanup-sb-orig.sh /bili-sync --delete # actually delete
# Matches: *.sb-orig* (any case, incl. video .SB-ORIG.mp4) (subtitle/danmaku copies) and *.sponsor-original.* (video copies,
# default original_suffix). Stop bili-sync first so nothing is mid-write.
ROOT="${1:-/bili-sync}"
MODE="$2"
[ -d "$ROOT" ] || { echo "not a directory: $ROOT" >&2; exit 1; }
find "$ROOT" -type f \( -iname '*.sb-orig*' -o -name '*.sponsor-original.*' -o -name '*.sponsor-original' \) -print > /tmp/sb-orig-list.txt
COUNT=$(wc -l < /tmp/sb-orig-list.txt)
cat /tmp/sb-orig-list.txt
echo "found $COUNT file(s) under $ROOT"
if [ "$MODE" = "--delete" ]; then
  tr '\n' '\0' < /tmp/sb-orig-list.txt | xargs -0 -r rm -f --
  echo "deleted $COUNT file(s). Refresh the Emby library afterwards."
else
  echo "dry run; re-run with --delete to remove"
fi
