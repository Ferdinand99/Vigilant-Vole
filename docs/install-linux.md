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

It first asks what this machine should run:

```
What should this machine run?
  1) Server        (web UI, database)
  2) Remote agent  (runs checks for another Vigilant Vole server)
Choose 1 or 2 [1]:
```

Press Enter for the server. For a [remote agent](#remote-agent) it then asks for the server URL, the
agent token (input hidden) and a name. `--server` or `--agent` (or `VV_INSTALL_MODE=server|agent`)
answer the question up front, and without a terminal (`curl … | bash`, Ansible, cloud-init) it installs
the server without asking, as earlier versions did.

The script ([`deploy/install.sh`](../deploy/install.sh)):

1. checks for systemd and a supported CPU (x86_64 or aarch64) and installs `curl`/`ca-certificates`
   with apt if they're missing,
2. downloads the latest release for your architecture and verifies it against `SHA256SUMS`,
3. installs the binary to `/opt/vigilant-vole/vigilant-vole` (linked as `/usr/local/bin/vigilant-vole`),
4. creates the system user `vigilant-vole`, the data directory `/var/lib/vigilant-vole` (mode 0750) and
   the settings file `/etc/vigilant-vole/vigilant-vole.env`,
5. installs and starts `vigilant-vole.service`, waits for `/healthz` and prints the URL.

(For an agent, steps 4 and 5 write `/etc/vigilant-vole/agent.env` and start `vigilant-vole-agent.service`
instead; see [Remote agent](#remote-agent).)

Open `http://<server>:3001` and create the admin account in the setup wizard **right away**. The wizard
needs a setup token: set `VV_SETUP_TOKEN` in the env file and open `/setup?token=<value>`, or leave it
unset and read the one the app made up from the log (`journalctl -u vigilant-vole | grep "setup token"`).
`VV_SETUP_OPEN=1` skips the token (anyone who can reach the page first creates the admin).

Options (put them after `--`):

```bash
bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)" -- --server --port 8080
bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)" -- --server --version v0.3.0
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

It first asks whether the container runs the **server** (the default, picked after 10 seconds) or a
**remote agent**. An agent container gets 1 CPU, 256 MB RAM and a 1 GB disk unless you set other values
(`var_ram`, `var_disk`, or in Advanced settings), and during the install it asks for the server URL, the
agent token (input hidden) and a name. It then runs only `vigilant-vole-agent.service` (no web UI) and
shows up on your server under **Settings → Remote agents**.

The answers can be given up front, which is what an unattended run (`mode=default`) needs:

| Variable | Meaning |
|---|---|
| `var_mode` | `server` (default) or `agent` |
| `var_setup_token` | server: the setup token for `/setup?token=<value>` (unset: the app makes one up and logs it) |
| `var_agent_server` | agent, required: URL of your Vigilant Vole server |
| `var_agent_token` | agent, required: the `vva_…` token from Settings → Remote agents |
| `var_agent_name` | agent: display name (default: the container's hostname) |

```bash
export COMMUNITY_SCRIPTS_URL="https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/proxmox"
export var_mode=agent var_agent_server=https://status.example.com var_agent_name=Proxmox
read -rsp "Agent token: " var_agent_token && export var_agent_token && echo
mode=default bash -c "$(curl -fsSL "$COMMUNITY_SCRIPTS_URL/ct/vigilant-vole.sh")"
unset var_agent_token
```

An unattended agent run without `var_agent_server` and `var_agent_token` stops before creating the
container. The token ends up only in `/etc/vigilant-vole/agent.env` (mode 0600) inside the container.

It uses the [community-scripts engine](https://github.com/community-scripts/core) (container wizard,
default/advanced settings, the `update` command inside the container), with the scripts loaded from this
repository. When the script has been accepted by community-scripts, the official one-liner will be on their
site. Details and how the script is tested: [deploy/proxmox/README.md](../deploy/proxmox/README.md).

Inside the container:

| What | Where |
|---|---|
| Binary | `/opt/vigilant-vole/vigilant-vole` |
| Data | `/var/lib/vigilant-vole` |
| Settings | `/etc/vigilant-vole/vigilant-vole.env` (agent: `/etc/vigilant-vole/agent.env`, mode 0600) |
| Service | `vigilant-vole.service`, or `vigilant-vole-agent.service` for an agent (runs as root, which is the community-scripts convention: the container is the isolation boundary) |
| Update | run `update` in the container console (it updates whichever service the container has) |

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

`update` finds the installed services (server, agent or both), stops them, swaps the binary, and starts
them again; data and settings stay. Add
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

A backup contains everything (users, tokens, notification secrets): store it like a password. Keep
`session.key` with the database: notification channel secrets and 2FA secrets are sealed with it, and
without it they have to be entered again (people with 2FA log in with a recovery code and set it up again).

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

The same script installs a [remote probe agent](remote-agents.md) instead of the server. Create the agent
on your server under **Settings → Remote agents → New agent** (the `vva_…` token is shown once), then on
the other host run the installer and pick **2) Remote agent**:

```
Choose 1 or 2 [1]: 2
Server URL (e.g. https://status.example.com): https://status.example.com
Agent token (vva_..., input hidden):
Agent name [frankfurt-01]: Frankfurt
 ok Reached https://status.example.com, which accepts the agent token
 ...
 ok Agent is running and connected to its server
```

It checks that `<server>/healthz` answers and that the server accepts the token, and warns (but still
installs) when it doesn't: the agent keeps retrying, and a wrong token can be fixed in the env file. When
it's done the agent shows up on the server under Settings → Remote agents; tick **Check from remote
agents too** on the monitors it should run.

Without questions, e.g. from automation (flags and `VV_AGENT_*` variables mix freely; `--server URL`
alone implies `--agent`):

```bash
export VV_AGENT_TOKEN=vva_xxxxxxxxxxxxxxxx
bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)" -- \
  --agent --server https://status.example.com --name Frankfurt

# or entirely from the environment
VV_INSTALL_MODE=agent VV_AGENT_SERVER=https://status.example.com VV_AGENT_TOKEN=vva_xxxxxxxxxxxxxxxx \
  VV_AGENT_NAME=Frankfurt bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)"
```

The token is stored in `/etc/vigilant-vole/agent.env` (mode 0600) and the agent runs as
`vigilant-vole-agent.service`. `--token vva_…` works too, but the environment variable keeps it out of the
process list. To change the server, token or name later, edit that file and
`systemctl restart vigilant-vole-agent`, or run the installer again with the new values. A server and an
agent can live on the same machine; `update` updates both.

## Uninstalling

```bash
bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)" -- uninstall
```

removes the program and the services and keeps `/var/lib/vigilant-vole` and `/etc/vigilant-vole`.
`uninstall --purge` deletes those and the `vigilant-vole` user as well (it asks first; add `--yes` when
running without a terminal).

With both a server and an agent on the machine, `uninstall` asks which one to remove (the server, the
agent, or both and the program). `uninstall --server` or `uninstall --agent` choose without asking and
leave the other one, the program and the user in place; with `--purge` only that one's data or settings
go (`/var/lib/vigilant-vole` and `vigilant-vole.env`, or `agent.env`). Without a terminal, or with
`--yes`, plain `uninstall` removes both.

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
