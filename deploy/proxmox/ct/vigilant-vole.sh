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
var_ram="${var_ram:-512}"
var_disk="${var_disk:-2}"
var_os="${var_os:-debian}"
var_version="${var_version:-13}"
var_arm64="${var_arm64:-yes}"
var_unprivileged="${var_unprivileged:-1}"

export var_setup_token="${var_setup_token:-}"

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

  if check_for_gh_release "vigilant-vole" "Ferdinand99/Vigilant-Vole"; then
    msg_info "Stopping Vigilant Vole"
    systemctl stop vigilant-vole
    msg_ok "Stopped Vigilant Vole"

    CLEAN_INSTALL=1 fetch_and_deploy_gh_release "vigilant-vole" "Ferdinand99/Vigilant-Vole" "prebuild" "latest" "/opt/vigilant-vole" "vigilant-vole-$(arch_resolve "x86_64" "aarch64")-unknown-linux-musl.tar.gz"

    msg_info "Starting Vigilant Vole"
    systemctl start vigilant-vole
    msg_ok "Started Vigilant Vole"
    msg_ok "Updated successfully!"
  fi
  exit
}

start
build_container
description

msg_ok "Completed successfully!\n"
echo -e "${CREATING}${GN}${APP} setup has been successfully initialized!${CL}"
echo -e "${INFO}${YW}Access it using the following URL:${CL}"
echo -e "${GATEWAY}${BGN}http://${IP}:3001${CL}"
