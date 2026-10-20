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

# The ct script stops unattended agent runs without a server URL or token, so
# these questions only come up when someone is at the terminal. They go into a
# systemd EnvironmentFile in quotes: no quotes, backslashes, $ or backticks.
if [[ "${var_mode:-server}" == "agent" ]]; then
  bad_chars='["\\$`[:cntrl:][:space:]]'
  asked=0
  while [[ ! "${var_agent_server:-}" =~ ^https?://[^/]+ || "${var_agent_server}" =~ $bad_chars ]]; do
    if [[ -n "${var_agent_server:-}" ]]; then
      msg_warn "The server URL (var_agent_server) must start with http:// or https://, without spaces or quotes"
    fi
    asked=1
    var_agent_server=""
    if ! read -r -p "${TAB3}Vigilant Vole server URL (e.g. https://status.example.com): " var_agent_server; then
      msg_error "No terminal to ask on. Set var_agent_server and var_agent_token."
      exit 1
    fi
  done
  while [[ "${var_agent_token:-}" != vva_?* || "${var_agent_token}" =~ $bad_chars ]]; do
    if [[ -n "${var_agent_token:-}" ]]; then
      msg_warn "Agent tokens start with vva_ (Settings → Remote agents on your server)"
    fi
    asked=1
    var_agent_token=""
    if ! read -r -s -p "${TAB3}Agent token (vva_..., from Settings → Remote agents; hidden): " var_agent_token; then
      echo
      msg_error "No terminal to ask on. Set var_agent_server and var_agent_token."
      exit 1
    fi
    echo
  done
  if [[ -z "${var_agent_name:-}" && "$asked" == 1 ]]; then
    read -r -p "${TAB3}Agent name (Enter = $(hostname)): " var_agent_name || true
  fi
  bad_name='["\\$`[:cntrl:]]'
  if [[ "${var_agent_name:-}" =~ $bad_name ]]; then
    msg_error "var_agent_name must not contain quotes, backslashes, \$ or backticks"
    exit 1
  fi
fi

fetch_and_deploy_gh_release "vigilant-vole" "Ferdinand99/Vigilant-Vole" "prebuild" "latest" "/opt/vigilant-vole" "vigilant-vole-$(arch_resolve "x86_64" "aarch64")-unknown-linux-musl.tar.gz"
ln -sf /opt/vigilant-vole/vigilant-vole /usr/local/bin/vigilant-vole

if [[ "${var_mode:-server}" == "agent" ]]; then
  msg_info "Configuring Vigilant Vole Agent"
  # The token goes to curl on stdin (-K -), not on its command line.
  agent_state=""
  if ! curl -fsS --max-time 5 -o /dev/null "${var_agent_server%/}/healthz" 2>/dev/null; then
    agent_state="unreachable"
  else
    code="$(printf 'header = "Authorization: Bearer %s"
' "$var_agent_token" |
      curl -sS --max-time 10 -o /dev/null -w '%{http_code}' -K - "${var_agent_server%/}/api/agent/v1/monitors" 2>/dev/null || true)"
    case "$code" in
    401 | 403) agent_state="refused" ;;
    esac
  fi
  mkdir -p /etc/vigilant-vole
  name_line="#VV_AGENT_NAME="
  if [[ -n "${var_agent_name:-}" ]]; then
    name_line="VV_AGENT_NAME=\"${var_agent_name}\""
  fi
  (
    umask 077
    cat <<EOF >/etc/vigilant-vole/agent.env
# Vigilant Vole remote agent, read by vigilant-vole-agent.service.
# After a change: systemctl restart vigilant-vole-agent
VV_AGENT_SERVER="${var_agent_server}"
VV_AGENT_TOKEN="${var_agent_token}"
${name_line}
#VV_AGENT_CONCURRENCY=16
#RUST_LOG=info
EOF
  )
  chmod 600 /etc/vigilant-vole/agent.env
  unset var_agent_token
  msg_ok "Configured Vigilant Vole Agent"

  msg_info "Creating Service"
  cat <<EOF >/etc/systemd/system/vigilant-vole-agent.service
[Unit]
Description=Vigilant Vole remote probe agent
Documentation=https://github.com/Ferdinand99/Vigilant-Vole/blob/main/docs/remote-agents.md
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=root
EnvironmentFile=/etc/vigilant-vole/agent.env
ExecStart=/opt/vigilant-vole/vigilant-vole agent
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
EOF
  systemctl enable -q --now vigilant-vole-agent
  msg_ok "Created Service"

  if [[ -z "$agent_state" ]]; then
    msg_info "Waiting for the agent to reach its server"
    for _ in {1..15}; do
      sleep 1
      invocation="$(systemctl show -p InvocationID --value vigilant-vole-agent 2>/dev/null || true)"
      agent_log="$(journalctl --no-pager -o cat "_SYSTEMD_INVOCATION_ID=${invocation}" 2>/dev/null || true)"
      if [[ "$agent_log" == *"connected to server"* ]]; then
        agent_state="connected"
        break
      fi
    done
  fi
  case "$agent_state" in
  connected) msg_ok "Agent connected to ${var_agent_server%/}" ;;
  refused) msg_warn "${var_agent_server%/} rejected the agent token (mistyped, rotated, deleted or disabled agent); put the right one in /etc/vigilant-vole/agent.env and run: systemctl restart vigilant-vole-agent" ;;
  unreachable) msg_warn "Could not reach ${var_agent_server%/} from this container; the agent keeps retrying (journalctl -u vigilant-vole-agent -f)" ;;
  *) msg_warn "The agent has not reached ${var_agent_server%/} yet and keeps retrying (journalctl -u vigilant-vole-agent -f)" ;;
  esac
else
  msg_info "Configuring Vigilant Vole"
  mkdir -p /var/lib/vigilant-vole /etc/vigilant-vole
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
fi

motd_ssh
customize
cleanup_lxc
