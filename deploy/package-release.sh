#!/usr/bin/env bash
# Packs a release binary into the tarball attached to GitHub releases.
#
#   deploy/package-release.sh <binary> <target-triple> <version> <out-dir>
#
#   deploy/package-release.sh ./vigilant-vole x86_64-unknown-linux-musl v0.3.0 dist
#     -> dist/vigilant-vole-x86_64-unknown-linux-musl.tar.gz
#
# The archive holds one directory, vigilant-vole-<target>/, with the binary,
# LICENSE and a short README. Asset names carry no version, so
# releases/latest/download/<asset> always points at the newest build.
# Used by .github/workflows/release-binaries.yml; run it locally the same way.
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: $0 <binary> <target-triple> <version> <out-dir>" >&2
  exit 64
fi

binary="$1"
target="$2"
version="${3#v}"
out_dir="$4"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
name="vigilant-vole-${target}"

[[ -f "$binary" ]] || { echo "binary not found: $binary" >&2; exit 66; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/$name" "$out_dir"

install -m 0755 "$binary" "$work/$name/vigilant-vole"
install -m 0644 "$repo_root/LICENSE" "$work/$name/LICENSE"
cat >"$work/$name/README.md" <<EOF
# Vigilant Vole ${version}

A lightweight, self-hosted uptime monitor: one static binary, one SQLite file.
This archive is the Linux build for \`${target}\` (statically linked). The
only thing it needs from the system is a CA certificate store (Debian/Ubuntu:
the \`ca-certificates\` package).

## Install as a systemd service (recommended)

    bash -c "\$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)"

The installer verifies this archive against SHA256SUMS, creates the
\`vigilant-vole\` user, /var/lib/vigilant-vole, /etc/vigilant-vole/vigilant-vole.env
and vigilant-vole.service. Update with \`... install.sh) update\`.

## Run by hand

    DATA_DIR=./data PORT=3001 ./vigilant-vole serve

Open http://localhost:3001 and create the admin account in the setup wizard.
Remote probe agent: \`VV_AGENT_TOKEN=vva_... ./vigilant-vole agent --server https://status.example.com\`.

Ping monitors need unprivileged ICMP (net.ipv4.ping_group_range) or CAP_NET_RAW.

Documentation: https://github.com/Ferdinand99/Vigilant-Vole#readme
Linux guide:   https://github.com/Ferdinand99/Vigilant-Vole/blob/main/docs/install-linux.md
License: MIT (see LICENSE)
EOF

# Stable metadata so the same binary gives the same archive.
mtime="${SOURCE_DATE_EPOCH:-$(date +%s)}"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@${mtime}" \
  -C "$work" -cf - "$name" | gzip -n -9 >"$out_dir/$name.tar.gz"

echo "$out_dir/$name.tar.gz"
