#!/bin/sh
# Container entrypoint.
#
# Started as root (the default), it:
#   1. gives the `vole` user the uid/gid from PUID/PGID (default 1000:1000;
#      Unraid uses 99:100),
#   2. adds `vole` to the group that owns a mounted Docker socket, so Docker
#      monitors and label discovery work without `--group-add`,
#   3. hands the data directory to `vole` (data written by older images, which
#      ran as root, stays writable),
# and then runs the app as `vole`.
#
# Started with an explicit non-root user (`--user`, `user:` in compose), it runs
# the binary directly and leaves everything alone.
set -e

app=/usr/local/bin/vigilant-vole

if [ "$(id -u)" != "0" ]; then
    exec "$app" "$@"
fi

uid="${PUID:-1000}"
gid="${PGID:-1000}"
case "$uid$gid" in
    *[!0-9]*) echo "PUID and PGID must be numeric (got PUID=$uid PGID=$gid)" >&2; exit 1 ;;
esac

if [ "$uid" != "$(id -u vole)" ] || [ "$gid" != "$(id -g vole)" ]; then
    sed -i "s/^vole:x:[0-9]*:[0-9]*:/vole:x:$uid:$gid:/" /etc/passwd
    sed -i "s/^vole:x:[0-9]*:/vole:x:$gid:/" /etc/group
fi

sock=/var/run/docker.sock
if [ -S "$sock" ]; then
    sock_gid="$(stat -c %g "$sock")"
    sock_group="$(awk -F: -v g="$sock_gid" '$3 == g { print $1; exit }' /etc/group)"
    if [ -z "$sock_group" ]; then
        sock_group=docker-host
        addgroup -g "$sock_gid" "$sock_group"
    fi
    addgroup vole "$sock_group" 2>/dev/null || true
fi

data_dir="${DATA_DIR:-/data}"
mkdir -p "$data_dir"
chown -R vole:vole "$data_dir" 2>/dev/null \
    || echo "warning: could not chown $data_dir; make it writable for uid $uid" >&2

exec su-exec vole "$app" "$@"
