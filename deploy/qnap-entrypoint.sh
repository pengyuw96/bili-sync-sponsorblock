#!/bin/sh
# QNAP entrypoint: start as root only long enough to make the container's own
# copy of the iGPU render node group-accessible, then drop to PUID:PGID with su-exec.
# The node under /dev inside the container is created by docker --device; the host node is untouched.
set -e
PUID="${PUID:-2601}"
PGID="${PGID:-1113}"
APP="${BILI_SYNC_APP:-/app/bili-sync-rs}"
if [ "$(id -u)" = "0" ]; then
  for n in /dev/dri/renderD*; do
    [ -e "$n" ] || continue
    chgrp "$PGID" "$n" && chmod 660 "$n" || echo "entrypoint: could not adjust $n (continuing without iGPU)" >&2
  done
  umask 002
  exec su-exec "$PUID:$PGID" "$APP" "$@"
fi
umask 002
exec "$APP" "$@"
