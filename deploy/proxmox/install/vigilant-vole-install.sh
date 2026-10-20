#!/usr/bin/env bash

# Copyright (c) 2021-2026 community-scripts ORG
# Author: Ferdinand99 (Ferdinand99)
# License: MIT | https://github.com/community-scripts/DevScripts/raw/main/LICENSE
# Source: https://github.com/Ferdinand99/Vigilant-Vole

source /dev/stdin <<<"$FUNCTIONS_FILE_PATH"
color
verb_ip6
catch_errors
setting_up_container
network_check
update_os

fetch_and_deploy_gh_release "vigilant-vole" "Ferdinand99/Vigilant-Vole" "prebuild" "latest" "/opt/vigilant-vole" "vigilant-vole-$(arch_resolve "x86_64" "aarch64")-unknown-linux-musl.tar.gz"

msg_info "Configuring Vigilant Vole"
mkdir -p /var/lib/vigilant-vole /etc/vigilant-vole
ln -sf /opt/vigilant-vole/vigilant-vole /usr/local/bin/vigilant-vole
cat <<EOF >/etc/vigilant-vole/vigilant-vole.env
# All options: https://github.com/Ferdinand99/Vigilant-Vole#-configuration
DATA_DIR=/var/lib/vigilant-vole
PORT=3001
# Require /setup?token=<value> for the first-run setup page (empty = off).
VV_SETUP_TOKEN=${var_setup_token:-}
# Behind a reverse proxy that is the only way in:
#VV_TRUST_PROXY=1
#VV_CONFIG_FILE=/etc/vigilant-vole/vigilant.yml
#RUST_LOG=info
EOF
chmod 600 /etc/vigilant-vole/vigilant-vole.env
msg_ok "Configured Vigilant Vole"

msg_info "Creating Service"
cat <<EOF >/etc/systemd/system/vigilant-vole.service
[Unit]
Description=Vigilant Vole uptime monitor
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=root
EnvironmentFile=/etc/vigilant-vole/vigilant-vole.env
WorkingDirectory=/var/lib/vigilant-vole
ExecStart=/opt/vigilant-vole/vigilant-vole serve
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
systemctl enable -q --now vigilant-vole
msg_ok "Created Service"

motd_ssh
customize
cleanup_lxc
