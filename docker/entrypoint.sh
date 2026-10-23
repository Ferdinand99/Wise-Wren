#!/bin/sh
# Container entrypoint.
#
# Started as root (the default), it:
#   1. gives the `wren` user the uid/gid from PUID/PGID (default 1000:1000;
#      Unraid uses 99:100),
#   2. hands the data directory to `wren`,
# and then runs the app as `wren`.
#
# Started with an explicit non-root user (`--user`, `user:` in compose), it runs
# the binary directly and leaves everything alone.
set -e

app=/usr/local/bin/wise-wren

if [ "$(id -u)" != "0" ]; then
    exec "$app" "$@"
fi

uid="${PUID:-1000}"
gid="${PGID:-1000}"
case "$uid$gid" in
    *[!0-9]*) echo "PUID and PGID must be numeric (got PUID=$uid PGID=$gid)" >&2; exit 1 ;;
esac

if [ "$uid" != "$(id -u wren)" ] || [ "$gid" != "$(id -g wren)" ]; then
    sed -i "s/^wren:x:[0-9]*:[0-9]*:/wren:x:$uid:$gid:/" /etc/passwd
    sed -i "s/^wren:x:[0-9]*:/wren:x:$gid:/" /etc/group
fi

data_dir="${WW_DATA_DIR:-/data}"
mkdir -p "$data_dir"
chown -R wren:wren "$data_dir" 2>/dev/null \
    || echo "warning: could not chown $data_dir; make it writable for uid $uid" >&2

exec su-exec wren "$app" "$@"
