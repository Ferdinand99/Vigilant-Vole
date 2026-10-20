#!/usr/bin/env bash
_CS_DEFAULT_URL="https://raw.githubusercontent.com/community-scripts/DevScripts/main"
_cs_boot="${COMMUNITY_SCRIPTS_CORE_DIR:-$(dirname "${BASH_SOURCE[0]}")/../../core}/core/build.func"
source "$_cs_boot" 2>/dev/null || source <(curl -fsSL "${COMMUNITY_SCRIPTS_CORE_URL:-https://raw.githubusercontent.com/community-scripts/core/main}/core/build.func")

# Copyright (c) 2021-2026 community-scripts ORG
# Author: Ferdinand99 (Ferdinand99)
# License: MIT | https://github.com/community-scripts/DevScripts/raw/main/LICENSE
# Source: https://github.com/Ferdinand99/Vigilant-Vole

APP="Vigilant-Vole"
var_tags="${var_tags:-monitoring;uptime}"
var_cpu="${var_cpu:-1}"
var_os="${var_os:-debian}"
var_version="${var_version:-13}"
var_arm64="${var_arm64:-yes}"
var_unprivileged="${var_unprivileged:-1}"

# server: web UI and database. agent: a remote probe that runs checks for another
# Vigilant Vole server, so it gets smaller defaults. Inside an existing container
# (the update command) the installed service decides.
if [[ -z "${var_mode:-}" ]]; then
  if command -v pveversion >/dev/null 2>&1; then
    if ! is_unattended; then
      var_mode=$(msg_menu "Install the Vigilant Vole server or a remote agent?" \
        "server" "Server (web UI, database)" \
        "agent" "Remote agent (runs checks for another Vigilant Vole server)")
    fi
  elif [[ -f /etc/systemd/system/vigilant-vole-agent.service && ! -f /etc/systemd/system/vigilant-vole.service ]]; then
    var_mode="agent"
  fi
fi
var_mode="${var_mode:-server}"
if [[ "$var_mode" == "agent" ]]; then
  var_ram="${var_ram:-256}"
  var_disk="${var_disk:-1}"
else
  var_ram="${var_ram:-512}"
  var_disk="${var_disk:-2}"
fi

export var_mode
export var_setup_token="${var_setup_token:-}"
export var_agent_server="${var_agent_server:-}"
export var_agent_token="${var_agent_token:-}"
export var_agent_name="${var_agent_name:-}"

header_info "$APP"
variables
color
catch_errors

function update_script() {
  header_info
  check_container_storage
  check_container_resources

  if [[ ! -f /opt/vigilant-vole/vigilant-vole ]]; then
    msg_error "No ${APP} Installation Found!"
    exit
  fi

  local services=() svc
  for svc in vigilant-vole vigilant-vole-agent; do
    if [[ -f "/etc/systemd/system/${svc}.service" ]]; then
      services+=("$svc")
    fi
  done
  if [[ ${#services[@]} -eq 0 ]]; then
    msg_error "No ${APP} service found (vigilant-vole or vigilant-vole-agent)!"
    exit
  fi

  if check_for_gh_release "vigilant-vole" "Ferdinand99/Vigilant-Vole"; then
    msg_info "Stopping Services"
    systemctl stop "${services[@]}"
    msg_ok "Stopped Services"

    CLEAN_INSTALL=1 fetch_and_deploy_gh_release "vigilant-vole" "Ferdinand99/Vigilant-Vole" "prebuild" "latest" "/opt/vigilant-vole" "vigilant-vole-$(arch_resolve "x86_64" "aarch64")-unknown-linux-musl.tar.gz"

    msg_info "Starting Services"
    systemctl start "${services[@]}"
    msg_ok "Started Services"
    msg_ok "Updated successfully!"
  fi
  exit
}

if [[ ! "$var_mode" =~ ^(server|agent)$ ]]; then
  msg_error "var_mode must be server or agent (got ${var_mode})."
  exit 1
fi
if [[ "$var_mode" == "agent" ]] && command -v pveversion >/dev/null 2>&1 && is_unattended; then
  if [[ -z "${var_agent_server:-}" || -z "${var_agent_token:-}" ]]; then
    msg_error "var_agent_server and var_agent_token are required for unattended agent installs."
    exit 1
  fi
fi

start
build_container
description

msg_ok "Completed successfully!\n"
if [[ "$var_mode" == "agent" ]]; then
  echo -e "${CREATING}${GN}${APP} remote agent has been successfully initialized!${CL}"
  echo -e "${INFO}${YW}The agent is running. See it on your Vigilant Vole server under Settings → Remote agents:${CL}"
  if [[ -n "${var_agent_server:-}" ]]; then
    echo -e "${GATEWAY}${BGN}${var_agent_server%/}/settings/agents${CL}"
  fi
  echo -e "${INFO}${YW}Then tick \"Check from remote agents too\" on the monitors it should run.${CL}"
else
  echo -e "${CREATING}${GN}${APP} setup has been successfully initialized!${CL}"
  echo -e "${INFO}${YW}Access it using the following URL:${CL}"
  echo -e "${GATEWAY}${BGN}http://${IP}:3001${CL}"
fi
