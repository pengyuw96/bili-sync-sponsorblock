#!/bin/sh
# Rename "S01E27 - <title>.*" files in single-video folders to "<title>.*"
# (3Blue1Brown-style: every file shares the video stem), drop folder-level
# "-thumb.jpg"/"-fanart.jpg", move "{stem}.sponsorblock.json" to hidden
# ".{newstem}.sponsorblock.json" with rendered_subtitles keys renamed (prevents
# a second subtitle shift). Emits SQL for page.path to $SQL_OUT.
# Usage: sh migrate-episode-names.sh <host_dir> <host_root> <container_root> [--apply]
DIR="$1"; HROOT="$2"; CROOT="$3"; APPLY=0; [ "${4:-}" = "--apply" ] && APPLY=1
SQL_OUT="${SQL_OUT:-/tmp/episode-rename.sql}"; : > "$SQL_OUT"
q(){ printf "%s" "$1" | sed "s/'/''/g"; }
find "$DIR" -mindepth 1 -maxdepth 1 -type d | while IFS= read -r d; do
  n=$(find "$d" -maxdepth 1 -type f -name 'S[0-9][0-9]E[0-9]* - *.mp4' | wc -l)
  [ "$n" -eq 1 ] || { [ "$n" -gt 1 ] && echo "SKIP multi-mp4: $d"; continue; }
  find "$d" -maxdepth 1 -type f -name 'S[0-9][0-9]E[0-9]* - *' | while IFS= read -r f; do
    b=$(basename "$f"); nb=$(printf "%s" "$b" | sed 's/^S[0-9][0-9]E[0-9][0-9]* - //')
    case "$nb" in *.sponsorblock.json) nb=".$nb";; esac
    if [ -e "$d/$nb" ]; then echo "EXISTS $d/$nb (skip)"; continue; fi
    echo "MV $b -> $nb"
    if [ $APPLY -eq 1 ]; then
      mv -- "$f" "$d/$nb"
      case "$nb" in .*.sponsorblock.json) sed -i 's/"S[0-9][0-9]E[0-9][0-9]* - /"/g' "$d/$nb";; esac
    fi
    case "$b" in *.mp4)
      old="$CROOT${f#$HROOT}"; new="$CROOT${d#$HROOT}/$nb"
      echo "UPDATE page SET path='$(q "$new")' WHERE path='$(q "$old")';" >> "$SQL_OUT";;
    esac
  done
  for x in -thumb.jpg -fanart.jpg; do
    [ -f "$d/$x" ] && { echo "RM $d/$x"; [ $APPLY -eq 1 ] && rm -f -- "$d/$x"; }
  done
done
echo "sql statements: $(wc -l < "$SQL_OUT") -> $SQL_OUT"
