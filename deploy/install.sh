#!/usr/bin/env bash
# Vigilant Vole installer for Linux with systemd (Debian, Ubuntu, and others).
#
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)"
#   bash -c "$(curl -fsSL .../deploy/install.sh)" -- update
#   bash -c "$(curl -fsSL .../deploy/install.sh)" -- uninstall [--purge]
#
# Commands:
#   install            (default) install or repair; safe to run again
#   update             replace the binary with the latest (or --version) release,
#                      keep data and settings, restart
#   uninstall          remove the program and the services; keep data and settings
#   uninstall --purge  also delete /var/lib/vigilant-vole, /etc/vigilant-vole and the user
#
# Options:
#   --version vX.Y.Z   install this release instead of the latest
#   --port N           HTTP port (default 3001; written to the env file)
#   --agent            install a remote probe agent (vigilant-vole-agent.service)
#                      instead of the server; needs --server and a token
#   --server URL       agent: URL of your Vigilant Vole server
#   --token T          agent: agent token (or set VV_AGENT_TOKEN, which stays out of `ps`)
#   --name NAME        agent: display name (default: hostname)
#   --yes              don't ask before deleting data (uninstall --purge)
#
# Layout:
#   /opt/vigilant-vole/vigilant-vole        binary (+ /usr/local/bin/vigilant-vole)
#   /var/lib/vigilant-vole                  data: database, session key, backups
#   /etc/vigilant-vole/vigilant-vole.env    server settings (kept on update)
#   /etc/vigilant-vole/agent.env            agent settings (mode 0600)
#   vigilant-vole.service / vigilant-vole-agent.service
#
# Development only: VV_INSTALL_BASE_URL=http://host:port downloads
# <base>/<tag>/<asset> instead of GitHub releases (and <base>/latest holds the
# tag to use as "latest"). Not for production use.
set -euo pipefail

readonly REPO="Ferdinand99/Vigilant-Vole"
readonly APP_DIR="/opt/vigilant-vole"
readonly BIN="$APP_DIR/vigilant-vole"
readonly BIN_LINK="/usr/local/bin/vigilant-vole"
readonly DATA_DIR="/var/lib/vigilant-vole"
readonly CONF_DIR="/etc/vigilant-vole"
readonly ENV_FILE="$CONF_DIR/vigilant-vole.env"
readonly AGENT_ENV_FILE="$CONF_DIR/agent.env"
readonly UNIT_DIR="/etc/systemd/system"
readonly SERVICE="vigilant-vole"
readonly AGENT_SERVICE="vigilant-vole-agent"
readonly SVC_USER="vigilant-vole"
readonly DEFAULT_PORT="3001"

command_name="install"
want_version=""
port=""
agent_mode=0
agent_server=""
agent_token="${VV_AGENT_TOKEN:-}"
agent_name=""
purge=0
assume_yes=0
tmp_dir=""

if [[ -t 1 ]]; then
  c_bold=$'\e[1m' c_green=$'\e[32m' c_yellow=$'\e[33m' c_red=$'\e[31m' c_reset=$'\e[0m'
else
  c_bold="" c_green="" c_yellow="" c_red="" c_reset=""
fi

info() { printf '%s==>%s %s\n' "$c_bold" "$c_reset" "$*"; }
ok() { printf '%s ok%s %s\n' "$c_green" "$c_reset" "$*"; }
warn() { printf '%swarning:%s %s\n' "$c_yellow" "$c_reset" "$*" >&2; }
die() {
  printf '%serror:%s %s\n' "$c_red" "$c_reset" "$*" >&2
  exit 1
}

cleanup() {
  if [[ -n "$tmp_dir" && -d "$tmp_dir" ]]; then
    rm -rf "$tmp_dir"
  fi
}
trap cleanup EXIT

usage() {
  sed -n '2,/^set -euo pipefail/{/^set -euo/d;s/^# \{0,1\}//;p}' "${BASH_SOURCE[0]}" 2>/dev/null ||
    echo "usage: install.sh [install|update|uninstall [--purge]] [--version vX.Y.Z] [--port N] [--agent --server URL --token T]"
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
    install | update | uninstall) command_name="$1" ;;
    --version)
      [[ $# -ge 2 ]] || die "--version needs a value, e.g. --version v0.3.0"
      want_version="$2"
      shift
      ;;
    --version=*) want_version="${1#*=}" ;;
    --port)
      [[ $# -ge 2 ]] || die "--port needs a value"
      port="$2"
      shift
      ;;
    --port=*) port="${1#*=}" ;;
    --agent) agent_mode=1 ;;
    --server)
      [[ $# -ge 2 ]] || die "--server needs a URL"
      agent_server="$2"
      shift
      ;;
    --server=*) agent_server="${1#*=}" ;;
    --token)
      [[ $# -ge 2 ]] || die "--token needs a value"
      agent_token="$2"
      shift
      ;;
    --token=*) agent_token="${1#*=}" ;;
    --name)
      [[ $# -ge 2 ]] || die "--name needs a value"
      agent_name="$2"
      shift
      ;;
    --name=*) agent_name="${1#*=}" ;;
    --purge) purge=1 ;;
    --yes | -y) assume_yes=1 ;;
    -h | --help)
      usage
      exit 0
      ;;
    --) ;;
    *) die "unknown argument: $1 (see --help)" ;;
    esac
    shift
  done

  if [[ -n "$want_version" ]]; then
    [[ "$want_version" == v* ]] || want_version="v$want_version"
    [[ "$want_version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]] ||
      die "--version must look like v1.2.3 (got $want_version)"
  fi
  if [[ -n "$port" ]]; then
    if ! [[ "$port" =~ ^[0-9]+$ ]] || ((port < 1 || port > 65535)); then
      die "--port must be 1-65535 (got $port)"
    fi
  fi
  if [[ "$purge" == 1 && "$command_name" != "uninstall" ]]; then
    die "--purge only goes with uninstall"
  fi
  if [[ "$agent_mode" == 1 && "$command_name" == "install" ]]; then
    [[ -n "$agent_server" ]] || die "--agent needs --server https://your-vigilant-vole-server"
    [[ "$agent_server" =~ ^https?:// ]] || die "--server must start with http:// or https://"
    [[ -n "$agent_token" ]] || die "--agent needs a token: set VV_AGENT_TOKEN=vva_... or pass --token"
    case "$agent_server$agent_token$agent_name" in
    *$'\n'* | *\"* | *\\*) die "--server, --token and --name must not contain quotes, backslashes or newlines" ;;
    esac
  fi
}

require_root() {
  [[ "$(id -u)" == 0 ]] || die "run this as root (e.g. with sudo)"
}

require_systemd() {
  command -v systemctl >/dev/null 2>&1 && [[ -d /run/systemd/system ]] ||
    die "systemd is not running here. This installer sets up a systemd service; on other systems run the binary yourself or use the Docker image (ghcr.io/ferdinand99/vigilant-vole)."
}

detect_target() {
  case "$(uname -m)" in
  x86_64 | amd64) echo "x86_64-unknown-linux-musl" ;;
  aarch64 | arm64) echo "aarch64-unknown-linux-musl" ;;
  *) die "unsupported CPU architecture $(uname -m): release builds exist for x86_64 and aarch64 only (build from source, or use the Docker image)" ;;
  esac
}

ensure_tools() {
  local missing=()
  command -v curl >/dev/null 2>&1 || missing+=(curl)
  command -v tar >/dev/null 2>&1 || missing+=(tar)
  command -v gzip >/dev/null 2>&1 || missing+=(gzip)
  command -v sha256sum >/dev/null 2>&1 || missing+=(coreutils)
  command -v useradd >/dev/null 2>&1 || missing+=(passwd)
  # The server refuses to start without a system CA store (HTTPS checks and
  # notifications verify against it).
  if [[ ! -s /etc/ssl/certs/ca-certificates.crt && ! -s /etc/pki/tls/certs/ca-bundle.crt && ! -s /etc/ssl/cert.pem ]]; then
    missing+=(ca-certificates)
  fi
  ((${#missing[@]} == 0)) && return 0
  if command -v apt-get >/dev/null 2>&1; then
    info "Installing ${missing[*]}"
    DEBIAN_FRONTEND=noninteractive apt-get update -qq >/dev/null
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${missing[@]}" >/dev/null
  else
    die "missing tools: ${missing[*]}. Install them with your package manager and run this again."
  fi
}

download() {
  curl -fsSL --retry 3 --retry-delay 2 --connect-timeout 15 -o "$2" "$1" ||
    die "download failed: $1"
}

# Prints the release tag to install.
resolve_version() {
  if [[ -n "$want_version" ]]; then
    echo "$want_version"
    return
  fi
  local tag=""
  if [[ -n "${VV_INSTALL_BASE_URL:-}" ]]; then
    tag="$(curl -fsSL "${VV_INSTALL_BASE_URL%/}/latest" | tr -d '[:space:]')" ||
      die "could not read ${VV_INSTALL_BASE_URL%/}/latest"
  else
    # The /releases/latest page redirects to /releases/tag/<tag>: no API, no rate limit.
    local url
    url="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")" ||
      die "could not reach github.com to look up the latest release"
    tag="${url##*/}"
  fi
  [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+ ]] || die "could not work out the latest release (got '$tag'); pass --version vX.Y.Z"
  echo "$tag"
}

release_url() {
  if [[ -n "${VV_INSTALL_BASE_URL:-}" ]]; then
    echo "${VV_INSTALL_BASE_URL%/}/$1/$2"
  else
    echo "https://github.com/$REPO/releases/download/$1/$2"
  fi
}

installed_version() {
  [[ -x "$BIN" ]] || return 0
  "$BIN" --version 2>/dev/null | awk '{print $2}'
}

# Downloads, verifies and unpacks the release into $tmp_dir; sets new_bin.
new_bin=""
fetch_release() {
  local tag="$1" target asset
  target="$(detect_target)"
  asset="vigilant-vole-${target}.tar.gz"
  tmp_dir="$(mktemp -d)"

  info "Downloading Vigilant Vole $tag ($target)"
  download "$(release_url "$tag" "$asset")" "$tmp_dir/$asset"
  download "$(release_url "$tag" SHA256SUMS)" "$tmp_dir/SHA256SUMS"

  local expected actual
  expected="$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp_dir/SHA256SUMS")"
  [[ -n "$expected" ]] || die "SHA256SUMS of $tag has no entry for $asset"
  actual="$(sha256sum "$tmp_dir/$asset" | awk '{print $1}')"
  [[ "$expected" == "$actual" ]] || die "checksum mismatch for $asset (expected $expected, got $actual)"
  ok "Checksum verified"

  tar -xzf "$tmp_dir/$asset" -C "$tmp_dir" --no-same-owner
  new_bin="$tmp_dir/vigilant-vole-${target}/vigilant-vole"
  [[ -f "$new_bin" ]] || die "$asset does not contain vigilant-vole-${target}/vigilant-vole"
  chmod 0755 "$new_bin"
  "$new_bin" --version >/dev/null 2>&1 || die "the downloaded binary does not run on this system"
}

# Puts a new binary in place atomically (same directory, then rename).
place_binary() {
  mkdir -p "$APP_DIR"
  install -m 0755 -o root -g root "$1" "$APP_DIR/.vigilant-vole.new"
  mv -f "$APP_DIR/.vigilant-vole.new" "$BIN"
  ln -sfn "$BIN" "$BIN_LINK"
}

ensure_user() {
  if ! getent group "$SVC_USER" >/dev/null; then
    groupadd --system "$SVC_USER"
  fi
  if ! id -u "$SVC_USER" >/dev/null 2>&1; then
    local shell
    shell="$(command -v nologin 2>/dev/null || echo /bin/false)"
    useradd --system --gid "$SVC_USER" --home-dir "$DATA_DIR" --no-create-home \
      --shell "$shell" --comment "Vigilant Vole" "$SVC_USER"
    ok "Created system user $SVC_USER"
  fi
}

ensure_dirs() {
  install -d -m 0750 -o "$SVC_USER" -g "$SVC_USER" "$DATA_DIR"
  # Data restored or copied in as root stays readable for the service.
  chown -R "$SVC_USER:$SVC_USER" "$DATA_DIR"
  install -d -m 0750 -o root -g "$SVC_USER" "$CONF_DIR"
}

env_changed=0
write_env_file() {
  if [[ -f "$ENV_FILE" ]]; then
    if [[ -n "$port" && "$(env_port)" != "$port" ]]; then
      env_changed=1
      if grep -q '^PORT=' "$ENV_FILE"; then
        sed -i "s/^PORT=.*/PORT=$port/" "$ENV_FILE"
      else
        printf 'PORT=%s\n' "$port" >>"$ENV_FILE"
      fi
      ok "Set PORT=$port in $ENV_FILE"
    fi
    return
  fi
  cat >"$ENV_FILE" <<EOF
# Vigilant Vole server settings, read by vigilant-vole.service.
# After a change: systemctl restart vigilant-vole
# All options: https://github.com/Ferdinand99/Vigilant-Vole#-configuration
# The installer never overwrites this file (update keeps it as it is).

# Database, session key and automatic pre-migration backups.
# Moving it elsewhere also needs ReadWritePaths= (systemctl edit vigilant-vole).
DATA_DIR=$DATA_DIR

# HTTP port; the server listens on all interfaces.
PORT=${port:-$DEFAULT_PORT}

# Behind a reverse proxy (Caddy, nginx, Traefik) that is the only way in:
# trust X-Forwarded-* for secure cookies, client IPs and generated URLs.
#VV_TRUST_PROXY=1

# Require /setup?token=<value> for the first-run setup page, so nobody else
# can claim a fresh instance that is reachable from the network.
#VV_SETUP_TOKEN=

# Merge a YAML/JSON config document into the database on every start.
#VV_CONFIG_FILE=/etc/vigilant-vole/vigilant.yml

# Checks allowed to run at the same time (default 64).
#VV_MAX_CONCURRENT_CHECKS=64

# Docker monitors and label discovery (the vigilant-vole user must be able to
# use the socket: usermod -aG docker vigilant-vole).
#DOCKER_HOST=unix:///var/run/docker.sock

# Tailscale ping monitors: path of the tailscaled socket.
#VV_TAILSCALE_SOCKET=/var/run/tailscale/tailscaled.sock

# Log filter.
#RUST_LOG=info
EOF
  chown root:"$SVC_USER" "$ENV_FILE"
  chmod 0640 "$ENV_FILE"
  env_changed=1
  ok "Wrote $ENV_FILE"
}

write_agent_env_file() {
  local name_line="#VV_AGENT_NAME="
  [[ -n "$agent_name" ]] && name_line="VV_AGENT_NAME=\"$agent_name\""
  local old_umask before=""
  [[ -f "$AGENT_ENV_FILE" ]] && before="$(cat "$AGENT_ENV_FILE")"
  old_umask="$(umask)"
  umask 077
  cat >"$AGENT_ENV_FILE" <<EOF
# Vigilant Vole remote agent, read by vigilant-vole-agent.service.
# After a change: systemctl restart vigilant-vole-agent
VV_AGENT_SERVER="$agent_server"
VV_AGENT_TOKEN="$agent_token"
$name_line
#VV_AGENT_CONCURRENCY=16
#RUST_LOG=info
EOF
  umask "$old_umask"
  chown root:root "$AGENT_ENV_FILE"
  chmod 0600 "$AGENT_ENV_FILE"
  [[ "$(cat "$AGENT_ENV_FILE")" == "$before" ]] || env_changed=1
  ok "Wrote $AGENT_ENV_FILE (mode 0600)"
}

# Shared sandboxing. The service user only gets CAP_NET_RAW (raw ICMP for ping
# monitors when unprivileged ICMP via net.ipv4.ping_group_range is off).
unit_hardening() {
  cat <<'EOF'
AmbientCapabilities=CAP_NET_RAW
CapabilityBoundingSet=CAP_NET_RAW
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
EOF
}

# Writes stdin to the unit file $1 when it differs. Prints 1 if it changed, else 0.
put_unit() {
  local dest="$UNIT_DIR/$1.service" new
  new="$(cat)"
  if [[ -f "$dest" ]] && [[ "$(cat "$dest")" == "$new" ]]; then
    echo 0
    return 0
  fi
  printf '%s\n' "$new" >"$dest.tmp"
  chmod 0644 "$dest.tmp"
  mv -f "$dest.tmp" "$dest"
  echo 1
}

render_server_unit() {
  {
    cat <<EOF
# Managed by deploy/install.sh; it is rewritten on install and update.
# Local changes: systemctl edit vigilant-vole
[Unit]
Description=Vigilant Vole uptime monitor
Documentation=https://github.com/Ferdinand99/Vigilant-Vole
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$SVC_USER
Group=$SVC_USER
EnvironmentFile=$ENV_FILE
WorkingDirectory=$DATA_DIR
ExecStart=$BIN serve
Restart=on-failure
RestartSec=5
TimeoutStopSec=30
ReadWritePaths=$DATA_DIR
EOF
    unit_hardening
    cat <<'EOF'

[Install]
WantedBy=multi-user.target
EOF
  }
}

render_agent_unit() {
  {
    cat <<EOF
# Managed by deploy/install.sh; it is rewritten on install and update.
# Local changes: systemctl edit vigilant-vole-agent
[Unit]
Description=Vigilant Vole remote probe agent
Documentation=https://github.com/Ferdinand99/Vigilant-Vole/blob/main/docs/remote-agents.md
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$SVC_USER
Group=$SVC_USER
EnvironmentFile=$AGENT_ENV_FILE
ExecStart=$BIN agent
Restart=on-failure
RestartSec=10
TimeoutStopSec=30
EOF
    unit_hardening
    cat <<'EOF'

[Install]
WantedBy=multi-user.target
EOF
  }
}

unit_exists() {
  [[ -f "$UNIT_DIR/$1.service" ]]
}

env_port() {
  local p=""
  [[ -f "$ENV_FILE" ]] && p="$(sed -n 's/^PORT=["]\{0,1\}\([0-9]*\)["]\{0,1\}[[:space:]]*$/\1/p' "$ENV_FILE" | tail -n 1)"
  echo "${p:-$DEFAULT_PORT}"
}

wait_healthy() {
  local p i
  p="$(env_port)"
  info "Waiting for http://127.0.0.1:$p/healthz"
  for ((i = 0; i < 60; i++)); do
    if curl -fsS --max-time 2 "http://127.0.0.1:$p/healthz" >/dev/null 2>&1; then
      ok "Vigilant Vole is up"
      return 0
    fi
    systemctl is-failed --quiet "$SERVICE" && break
    sleep 1
  done
  journalctl -u "$SERVICE" -n 30 --no-pager >&2 || true
  die "Vigilant Vole did not answer on port $p. Check: journalctl -u $SERVICE"
}

wait_agent() {
  sleep 3
  if systemctl is-active --quiet "$AGENT_SERVICE"; then
    ok "Agent is running (logs: journalctl -u $AGENT_SERVICE -f)"
  else
    journalctl -u "$AGENT_SERVICE" -n 30 --no-pager >&2 || true
    die "the agent did not start. Check $AGENT_ENV_FILE and: journalctl -u $AGENT_SERVICE"
  fi
}

host_address() {
  local a=""
  a="$(hostname -I 2>/dev/null | awk '{print $1}')" || true
  if [[ -z "$a" ]] && command -v ip >/dev/null 2>&1; then
    a="$(ip -o -4 addr show scope global 2>/dev/null | awk '{sub(/\/.*/, "", $4); print $4; exit}')" || true
  fi
  echo "${a:-localhost}"
}

print_url() {
  local p host token
  p="$(env_port)"
  host="$(host_address)"
  token="$(sed -n 's/^VV_SETUP_TOKEN=["]\{0,1\}\([^"]*\)["]\{0,1\}[[:space:]]*$/\1/p' "$ENV_FILE" 2>/dev/null | tail -n 1)"
  echo
  printf '%sVigilant Vole is running:%s http://%s:%s\n' "$c_bold" "$c_reset" "$host" "$p"
  if [[ -n "$token" ]]; then
    echo "First visit: http://$host:$p/setup?token=<VV_SETUP_TOKEN from $ENV_FILE>"
  else
    echo "Open it and create the admin account in the setup wizard right away."
    echo "(Until then anyone who can reach it can. Set VV_SETUP_TOKEN in $ENV_FILE to lock that down.)"
  fi
  echo "Settings: $ENV_FILE   Data: $DATA_DIR   Logs: journalctl -u $SERVICE -f"
  echo "Update:   bash -c \"\$(curl -fsSL https://raw.githubusercontent.com/$REPO/main/deploy/install.sh)\" -- update"
}

# Installs the requested release if it is not the one already in place.
# Sets binary_changed=1 when the binary was replaced.
binary_changed=0
install_binary() {
  local tag current svc
  tag="$(resolve_version)"
  current="$(installed_version)"
  if [[ -n "$current" && "v$current" == "$tag" ]]; then
    ok "Vigilant Vole $tag is already installed"
    ln -sfn "$BIN" "$BIN_LINK"
    return 0
  fi
  fetch_release "$tag"
  local active=()
  if [[ -n "$current" ]]; then
    for svc in "$SERVICE" "$AGENT_SERVICE"; do
      if systemctl is-active --quiet "$svc" 2>/dev/null; then
        active+=("$svc")
      fi
    done
    if ((${#active[@]} > 0)); then
      info "Stopping ${active[*]}"
      systemctl stop "${active[@]}"
    fi
  fi
  place_binary "$new_bin"
  binary_changed=1
  if [[ -n "$current" ]]; then
    ok "Updated Vigilant Vole $current -> ${tag#v}"
  else
    ok "Installed Vigilant Vole ${tag#v} to $BIN"
  fi
}

# Starts service $1, or restarts it when the binary, its unit ($2 = 1) or the
# settings changed.
apply_service() {
  if [[ "$binary_changed" == 1 || "${2:-0}" == 1 || "$env_changed" == 1 ]]; then
    systemctl restart "$1"
  else
    systemctl start "$1"
  fi
}

cmd_install() {
  require_root
  require_systemd
  detect_target >/dev/null
  ensure_tools
  install_binary
  ensure_user

  if [[ "$agent_mode" == 1 ]]; then
    install -d -m 0750 -o root -g "$SVC_USER" "$CONF_DIR"
    write_agent_env_file
    local agent_unit_changed
    agent_unit_changed="$(put_unit "$AGENT_SERVICE" < <(render_agent_unit))"
    systemctl daemon-reload
    systemctl enable --quiet "$AGENT_SERVICE"
    apply_service "$AGENT_SERVICE" "$agent_unit_changed"
    wait_agent
    return
  fi

  ensure_dirs
  write_env_file
  local server_unit_changed
  server_unit_changed="$(put_unit "$SERVICE" < <(render_server_unit))"
  systemctl daemon-reload
  systemctl enable --quiet "$SERVICE"
  apply_service "$SERVICE" "$server_unit_changed"
  if unit_exists "$AGENT_SERVICE" && [[ "$binary_changed" == 1 ]]; then
    systemctl restart "$AGENT_SERVICE" || true
  fi
  wait_healthy
  print_url
}

cmd_update() {
  require_root
  require_systemd
  local have_server=0 have_agent=0
  unit_exists "$SERVICE" && have_server=1
  unit_exists "$AGENT_SERVICE" && have_agent=1
  if [[ ! -x "$BIN" ]] || ((have_server == 0 && have_agent == 0)); then
    die "Vigilant Vole is not installed here (no $BIN with a service). Run the installer without 'update' first."
  fi
  ensure_tools
  install_binary

  # Newer installers may bring unit changes; the settings files stay as they are.
  local server_unit_changed=0 agent_unit_changed=0
  if ((have_server == 1)); then
    server_unit_changed="$(put_unit "$SERVICE" < <(render_server_unit))"
  fi
  if ((have_agent == 1)); then
    agent_unit_changed="$(put_unit "$AGENT_SERVICE" < <(render_agent_unit))"
  fi
  systemctl daemon-reload
  if ((have_server == 1)); then
    ensure_dirs
    # Database migrations run on start; they back up the database to
    # $DATA_DIR/backups first.
    apply_service "$SERVICE" "$server_unit_changed"
    wait_healthy
  fi
  if ((have_agent == 1)); then
    apply_service "$AGENT_SERVICE" "$agent_unit_changed"
    wait_agent
  fi
  ok "Vigilant Vole $(installed_version) is running"
}

cmd_uninstall() {
  require_root
  # Ask before touching anything, so a declined purge changes nothing.
  if [[ "$purge" == 1 && "$assume_yes" != 1 ]]; then
    if [[ -t 0 ]]; then
      local answer=""
      read -r -p "Delete $DATA_DIR (database, backups) and $CONF_DIR for good? [y/N] " answer
      [[ "$answer" =~ ^[Yy]([Ee][Ss])?$ ]] || die "nothing removed (answer y to confirm, or run uninstall without --purge)"
    else
      die "uninstall --purge deletes all monitoring data; add --yes to confirm when not running interactively"
    fi
  fi

  local svc
  for svc in "$SERVICE" "$AGENT_SERVICE"; do
    if unit_exists "$svc"; then
      info "Removing $svc.service"
      systemctl disable --now --quiet "$svc" 2>/dev/null || true
      rm -f "$UNIT_DIR/$svc.service"
    fi
  done
  command -v systemctl >/dev/null 2>&1 && systemctl daemon-reload 2>/dev/null || true
  if [[ -L "$BIN_LINK" || -f "$BIN_LINK" ]]; then
    rm -f "$BIN_LINK"
  fi
  rm -rf "$APP_DIR"
  ok "Removed the program and its services"

  if [[ "$purge" != 1 ]]; then
    echo "Kept your data in $DATA_DIR and settings in $CONF_DIR."
    echo "Delete them too with: uninstall --purge"
    return
  fi

  rm -rf "$DATA_DIR" "$CONF_DIR" "$UNIT_DIR/$SERVICE.service.d" "$UNIT_DIR/$AGENT_SERVICE.service.d"
  if id -u "$SVC_USER" >/dev/null 2>&1; then
    userdel "$SVC_USER" 2>/dev/null || warn "could not remove user $SVC_USER"
  fi
  if getent group "$SVC_USER" >/dev/null; then
    groupdel "$SVC_USER" 2>/dev/null || true
  fi
  ok "Deleted data, settings and the $SVC_USER user"
}

main() {
  parse_args "$@"
  if [[ -n "${VV_INSTALL_BASE_URL:-}" && "$command_name" != "uninstall" ]]; then
    warn "VV_INSTALL_BASE_URL is set: downloading from ${VV_INSTALL_BASE_URL} (development only)"
  fi
  case "$command_name" in
  install) cmd_install ;;
  update) cmd_update ;;
  uninstall) cmd_uninstall ;;
  esac
}

# `bash -c "$(curl ...)" update` puts the first word in $0, not $1: take it back.
case "$0" in
install | update | uninstall | --*) set -- "$0" "$@" ;;
esac
main "$@"
