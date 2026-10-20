# Tailscale ping monitors

A **Tailscale ping** monitor asks the Tailscale daemon (`tailscaled`) on the machine that runs the check to ping a
peer in your tailnet, like `tailscale ping -c 1 nas`. It answers a different question than an ICMP ping of the
peer's `100.x` address: whether the peer is reachable *at the Tailscale layer*, and which way the packets went —
a direct UDP path or a DERP relay.

Each check:

1. reads tailscaled's status (`/localapi/v0/status`): is Tailscale logged in and running here, and which peer does
   the target name;
2. asks tailscaled to ping that peer's Tailscale IP (`/localapi/v0/ping`), waiting at most the monitor's timeout;
3. reports **up** with the route and tailscaled's measured latency, or **down** with the reason.

Messages look like `pong from nas (100.101.102.103) via DERP(fra) in 23 ms`,
`pong from nas (100.101.102.103) direct via 203.0.113.7:41641 in 3 ms`,
`No pong from nas (100.101.102.103) within 10s; Tailscale shows it offline (last seen 2026-10-19 21:04 UTC)` or
`Tailscale on this host is logged out (NeedsLogin): log it in with tailscale up`. A relayed pong counts as up;
the route shows in the message and the monitor's history.

## Target

The peer's machine name as shown in the Tailscale admin console (`nas`), its MagicDNS name
(`nas.tail1234.ts.net`) or a Tailscale IP (`100.101.102.103`, IPv6 works too). An IP that belongs to no peer is
pinged as is, which covers addresses behind a subnet router.

The form's **Ping type** adds `?type=…` to the target:

| Type | Target | What it tests |
|---|---|---|
| Disco (default) | `nas` | Tailscale's own path probe, without either host's IP stack (what `tailscale ping` does) |
| TSMP | `nas?type=tsmp` | Through WireGuard to Tailscale on the peer |
| ICMP | `nas?type=icmp` | Through WireGuard and the peer's IP stack (catches a peer whose firewall drops traffic) |

## Giving the container the socket

The check talks to tailscaled over its local socket, so the socket of the host (or of a Tailscale container) has
to be visible inside the Vigilant Vole container. Vigilant Vole looks for it in this order:

1. `VV_TAILSCALE_SOCKET`, when set;
2. `/var/run/tailscale/tailscaled.sock` (Linux packages, the Unraid plugin);
3. `/tmp/tailscaled.sock` (the official `tailscale/tailscale` container);
4. `/var/packages/Tailscale/var/tailscaled.sock` (Synology);
5. `/tmp/tailscale/tailscaled.sock` (QNAP);
6. `/var/run/tailscaled.socket` (the open-source `tailscaled` on macOS).

Without one, the monitor is down with `No tailscaled socket found: …`.

### Docker Compose, Tailscale on the host

```yaml
services:
  vigilant-vole:
    volumes:
      - ./data:/data
      - /var/run/tailscale/tailscaled.sock:/var/run/tailscale/tailscaled.sock
```

A mounted socket *file* goes stale when tailscaled restarts and creates a new one (the monitor then reports that
it can't reach tailscaled until the container restarts). Mounting the folder avoids that:
`- /var/run/tailscale:/var/run/tailscale`.

### Tailscale in a sidecar container

The official image puts its socket where `TS_SOCKET` says (`/tmp/tailscaled.sock` by default). Share it through
a volume and point Vigilant Vole at it:

```yaml
services:
  tailscale:
    image: tailscale/tailscale:stable
    hostname: vigilant-vole
    environment:
      - TS_AUTHKEY=tskey-auth-…
      - TS_STATE_DIR=/var/lib/tailscale
      - TS_SOCKET=/var/run/tailscale/tailscaled.sock
    volumes:
      - ./tailscale-state:/var/lib/tailscale
      - tailscale-socket:/var/run/tailscale
  vigilant-vole:
    volumes:
      - ./data:/data
      - tailscale-socket:/var/run/tailscale
volumes:
  tailscale-socket:
```

### Unraid

With the Tailscale plugin, tailscaled runs on the host: fill in the template's **Tailscale socket (optional)**
path with `/var/run/tailscale/tailscaled.sock`.

### Synology, QNAP, other paths

Mount the package's socket to one of the paths above, or anywhere plus `VV_TAILSCALE_SOCKET=/that/path`.

### Remote agents

[Remote agents](remote-agents.md) (0.3.0 or newer) run Tailscale ping monitors with the socket of *their* host,
found the same way — handy for checking a peer from inside another network.

### Windows

On Windows the check opens tailscaled's named pipe (`\\.\pipe\ProtectedPrefix\Administrators\Tailscale\tailscaled`)
instead; this path is untested.

## Security

The socket is tailscaled's control interface. On Linux anyone who can open it may read the tailnet's status
(peers, their addresses and operating systems) and ping; changing settings, logging out or switching exit nodes
needs root or the user named with `tailscale up --operator=…`.

- Vigilant Vole runs as the unprivileged `vole` user, so tailscaled gives it the read side only. It never calls
  anything but `status` and `ping`.
- Running the container as root (`user: "0"`), or making its user Tailscale's operator, hands it full control of
  Tailscale on that host. Don't.
- Anyone who can create monitors can see whether names resolve in your tailnet and how its peers are routed
  (that is the point of the monitor); give the Editor role accordingly.
