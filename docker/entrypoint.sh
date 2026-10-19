#!/bin/sh
# Container entrypoint.
#
# Started as root (the default), it hands the data directory to the unprivileged
# `vole` user and then drops to it. That keeps upgrades painless: data folders
# written by older images (which ran as root) or created on the host would
# otherwise be read-only for the app.
#
# Started with an explicit non-root user (`--user 99:100`, `user:` in compose),
# it runs the binary directly and leaves ownership alone.
set -e

if [ "$(id -u)" = "0" ]; then
    data_dir="${DATA_DIR:-/data}"
    mkdir -p "$data_dir"
    chown -R vole:vole "$data_dir" 2>/dev/null || echo "warning: could not chown $data_dir; make it writable for uid 1000" >&2
    exec su-exec vole:vole /usr/local/bin/vigilant-vole "$@"
fi

exec /usr/local/bin/vigilant-vole "$@"
