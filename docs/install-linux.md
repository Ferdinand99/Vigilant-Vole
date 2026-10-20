# Installing on Linux (systemd) and Proxmox VE

Vigilant Vole is one static binary, so it runs without Docker on any 64-bit Linux with systemd:
Debian, Ubuntu, a Proxmox LXC container, a VM, a Raspberry Pi 4/5 with a 64-bit OS. Every release
ships static musl builds for **x86_64** and **aarch64** (`vigilant-vole-<arch>-unknown-linux-musl.tar.gz`)
plus `SHA256SUMS`.

- [Linux with the install script](#linux-with-the-install-script)
- [Proxmox VE LXC](#proxmox-ve-lxc)
- [Files and settings](#files-and-settings)
- [Updating](#updating)
- [Backups](#backups)
- [Behind a reverse proxy](#behind-a-reverse-proxy)
- [Ping, Docker and Tailscale monitors](#ping-docker-and-tailscale-monitors)
- [Remote agent](#remote-agent)
- [Uninstalling](#uninstalling)
- [Doing it by hand](#doing-it-by-hand)

## Linux with the install script

As root (or prefix with `sudo`):

```bash
bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)"
```

The script ([`deploy/install.sh`](../deploy/install.sh)):

1. checks for systemd and a supported CPU (x86_64 or aarch64) and installs `curl`/`ca-certificates`
   with apt if they're missing,
2. downloads the latest release for your architecture and verifies it against `SHA256SUMS`,
3. installs the binary to `/opt/vigilant-vole/vigilant-vole` (linked as `/usr/local/bin/vigilant-vole`),
4. creates the system user `vigilant-vole`, the data directory `/var/lib/vigilant-vole` (mode 0750) and
   the settings file `/etc/vigilant-vole/vigilant-vole.env`,
5. installs and starts `vigilant-vole.service`, waits for `/healthz` and prints the URL.

Open `http://<server>:3001` and create the admin account in the setup wizard **right away** (until then,
anyone who can reach the page can). To lock the wizard first, set `VV_SETUP_TOKEN` in the env file and
open `/setup?token=<value>`.

Options (put them after `--`):

```bash
bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)" -- --port 8080
bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)" -- --version v0.3.0
```

Running it again is safe: it repairs what's missing, keeps your settings and data, and only restarts
the service when something changed. Prefer to read it first? Download it, look, then run
`sudo bash install.sh`.

## Proxmox VE LXC

A [community-scripts](https://community-scripts.org)-style script creates a Debian 13 container
(1 CPU, 512 MB RAM, 2 GB disk, unprivileged) and installs Vigilant Vole in it. Run it in the
**Proxmox VE shell**:

```bash
export COMMUNITY_SCRIPTS_URL="https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/proxmox"
bash -c "$(curl -fsSL "$COMMUNITY_SCRIPTS_URL/ct/vigilant-vole.sh")"
```

It uses the [community-scripts engine](https://github.com/community-scripts/core) (container wizard,
default/advanced settings, the `update` command inside the container), with the scripts loaded from this
repository. When the script has been accepted by community-scripts, the official one-liner will be on their
site. Details and how the script is tested: [deploy/proxmox/README.md](../deploy/proxmox/README.md).

Inside the container:

| What | Where |
|---|---|
| Binary | `/opt/vigilant-vole/vigilant-vole` |
| Data | `/var/lib/vigilant-vole` |
| Settings | `/etc/vigilant-vole/vigilant-vole.env` |
| Service | `vigilant-vole.service` (runs as root, which is the community-scripts convention: the container is the isolation boundary) |
| Update | run `update` in the container console |

Ping monitors work in an unprivileged container: the service runs as the container's root, which may
open raw ICMP sockets in the container's own network namespace, and Debian 13 also allows unprivileged
ICMP (`net.ipv4.ping_group_range`).

You can also run [`deploy/install.sh`](#linux-with-the-install-script) inside any Debian or Ubuntu LXC
you made yourself. Its unit uses systemd sandboxing (`ProtectSystem`, `PrivateTmp`), which needs
**Options → Features → Nesting** on the container (the default for new unprivileged containers). Without
it the service fails with `status=226/NAMESPACE`.

## Files and settings

| Path | What |
|---|---|
| `/opt/vigilant-vole/vigilant-vole` | the program (`/usr/local/bin/vigilant-vole` links to it) |
| `/var/lib/vigilant-vole/` | database (`vigilant-vole.db`), session key, `backups/` |
| `/etc/vigilant-vole/vigilant-vole.env` | server settings (`DATA_DIR`, `PORT`, `VV_*`) |
| `/etc/vigilant-vole/agent.env` | agent settings, mode 0600 (agent installs only) |
| `/etc/systemd/system/vigilant-vole.service` | the service (rewritten by the installer) |

The env file takes the same variables as the Docker image (see the
[configuration table](../README.md#-configuration)); `PUID`/`PGID` don't apply. After editing it:

```bash
sudo systemctl restart vigilant-vole
journalctl -u vigilant-vole -f      # logs
```

The installer never overwrites the env file. It does rewrite the unit file, so make lasting unit changes
with a drop-in: `sudo systemctl edit vigilant-vole`. For example, a `DATA_DIR` outside
`/var/lib/vigilant-vole` needs:

```ini
[Service]
ReadWritePaths=/srv/vigilant-vole
```

The service runs as `vigilant-vole` with `NoNewPrivileges`, `ProtectSystem=strict` (only the data
directory is writable), `ProtectHome`, `PrivateTmp` and a single capability, `CAP_NET_RAW`.

## Updating

```bash
bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)" -- update
```

`update` stops the service, swaps the binary, and starts it again; data and settings stay. Add
`--version vX.Y.Z` for a specific release. Database migrations run on start, and the database is copied
to `/var/lib/vigilant-vole/backups/` before any migration is applied. In a Proxmox container made with
the LXC script, run `update` in the container instead.

## Backups

- **Settings → Backup → Database backup** downloads a consistent snapshot while monitoring keeps running.
- Or copy the data directory while the service is stopped:

  ```bash
  sudo systemctl stop vigilant-vole
  sudo tar -czf vigilant-vole-$(date +%F).tar.gz -C /var/lib vigilant-vole
  sudo systemctl start vigilant-vole
  ```

  Keep `/etc/vigilant-vole/vigilant-vole.env` too if you changed it. On Proxmox, a container backup
  (`vzdump`) covers everything.

A backup contains everything (users, tokens, notification secrets): store it like a password.

## Behind a reverse proxy

Run Caddy, nginx or Traefik in front for HTTPS and set, in the env file:

```bash
VV_TRUST_PROXY=1
```

so session cookies get `Secure`, the login rate limiter sees the real client IPs, and generated URLs use
the public host. Only set it when the proxy is the **only** way in: firewall port 3001, or bind the proxy
to the same host and block the port from outside. Also set the public base URL under Settings → General.
A minimal Caddyfile:

```
status.example.com {
    reverse_proxy 127.0.0.1:3001
}
```

## Ping, Docker and Tailscale monitors

- **Ping** works out of the box: the binary uses unprivileged ICMP sockets when
  `net.ipv4.ping_group_range` allows them (the default on current Debian and Ubuntu) and raw sockets
  otherwise, which the unit allows through `CAP_NET_RAW`.
- **Docker** monitors and label discovery need access to the Docker socket:
  `sudo usermod -aG docker vigilant-vole && sudo systemctl restart vigilant-vole`. That gives the app
  control over Docker; a read-only [socket proxy](docker-discovery.md#permissions) is safer.
- **Tailscale ping** reads `/var/run/tailscale/tailscaled.sock` (change it with `VV_TAILSCALE_SOCKET`);
  see [tailscale-ping.md](tailscale-ping.md).

## Remote agent

The same script installs a [remote probe agent](remote-agents.md) instead of the server: create the agent
under **Settings → Remote agents**, then on the other host:

```bash
export VV_AGENT_TOKEN=vva_xxxxxxxxxxxxxxxx
bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)" -- \
  --agent --server https://status.example.com --name Frankfurt
```

The token is stored in `/etc/vigilant-vole/agent.env` (mode 0600) and the agent runs as
`vigilant-vole-agent.service`. `--token vva_…` works too, but the environment variable keeps it out of the
process list. `update` updates the agent the same way.

## Uninstalling

```bash
bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)" -- uninstall
```

removes the program and the services and keeps `/var/lib/vigilant-vole` and `/etc/vigilant-vole`.
`uninstall --purge` deletes those and the `vigilant-vole` user as well (it asks first; add `--yes` when
running without a terminal).

## Doing it by hand

```bash
arch=$(uname -m)    # x86_64 or aarch64
curl -fLO "https://github.com/Ferdinand99/Vigilant-Vole/releases/latest/download/vigilant-vole-${arch}-unknown-linux-musl.tar.gz"
curl -fLO "https://github.com/Ferdinand99/Vigilant-Vole/releases/latest/download/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf "vigilant-vole-${arch}-unknown-linux-musl.tar.gz"
DATA_DIR=./data ./vigilant-vole-${arch}-unknown-linux-musl/vigilant-vole serve
```

The binary needs nothing from the system except a CA certificate store (`ca-certificates` on
Debian/Ubuntu); it won't start without one.

For development, `deploy/install.sh` honours `VV_INSTALL_BASE_URL=http://host:port`: it then downloads
`<base>/<tag>/<asset>` (and reads the latest tag from `<base>/latest`) instead of GitHub releases. Don't
use it for real installs.
