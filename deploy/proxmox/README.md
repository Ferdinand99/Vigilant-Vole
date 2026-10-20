# Proxmox VE LXC script (community-scripts format)

These files follow the [community-scripts](https://community-scripts.org) layout so they can be
submitted to [community-scripts/DevScripts](https://github.com/community-scripts/DevScripts) unchanged.
Until then they run straight from this repository.

| File | Goes to (DevScripts) | Does |
|---|---|---|
| [`ct/vigilant-vole.sh`](ct/vigilant-vole.sh) | `ct/vigilant-vole.sh` | Runs on the Proxmox host: container wizard (Debian 13, 1 CPU, 512 MB, 2 GB, unprivileged, amd64 + arm64), and `update_script` for the `update` command inside the container |
| [`install/vigilant-vole-install.sh`](install/vigilant-vole-install.sh) | `install/vigilant-vole-install.sh` | Runs inside the new container: downloads the release binary, writes the env file and `vigilant-vole.service` |
| [`json/vigilant-vole.json`](json/vigilant-vole.json) | `json/vigilant-vole.json` | Website metadata (category *Monitoring & Analytics*, port 3001, no default credentials) |

Inside the container: binary `/opt/vigilant-vole/vigilant-vole`, data `/var/lib/vigilant-vole`, settings
`/etc/vigilant-vole/vigilant-vole.env`, service `vigilant-vole`. The same paths as
[`deploy/install.sh`](../install.sh), except that the service runs as root: community-scripts rejects
dedicated service users in LXC scripts (the container is the isolation boundary), and root in an
unprivileged container may open raw ICMP sockets, so ping monitors work with no extra setup.

`APP="Vigilant-Vole"` (with the hyphen) is deliberate: the engine derives the file names and the
hostname from it (`NSAPP` = lowercase, spaces removed), so it has to match `vigilant-vole.sh` and
`vigilant-vole-install.sh`. `APP="Vigilant Vole"` would look for `vigilantvole-install.sh`.

## Before you test

1. **Release binaries must exist on GitHub.** The scripts download
   `vigilant-vole-<x86_64|aarch64>-unknown-linux-musl.tar.gz` from the latest release with
   `fetch_and_deploy_gh_release`. For v0.3.0 run **Actions → Release binaries → Run workflow** with
   `tag` = `v0.3.0` once; later releases get them automatically.
2. **Push the branch** that holds these files. Every command below loads them from GitHub, from
   `feature/proxmox-install`; after merging, replace `refs/heads/feature/proxmox-install` with
   `refs/heads/main`.

## Test 1: the full flow on your Proxmox host (do this before opening the PR)

The engine loads the `install/` script from the same "scripts root" as the `ct/` script. That root
defaults to DevScripts; `COMMUNITY_SCRIPTS_URL` points it at this repository instead (documented in
[core/docs/loading.md](https://github.com/community-scripts/core/blob/main/docs/loading.md#running-against-a-fork)).
In the **Proxmox VE shell** (as root):

```bash
export COMMUNITY_SCRIPTS_URL="https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/refs/heads/feature/proxmox-install/deploy/proxmox"
dev_mode=keep,logs bash -c "$(curl -fsSL "$COMMUNITY_SCRIPTS_URL/ct/vigilant-vole.sh")"
```

`dev_mode=keep` leaves the container in place if the build fails, `logs` keeps the full logs in
`/var/log/community-scripts/`. Drop `dev_mode=…` for a run that looks like a normal install. Choose
**Default** settings first, then try **Advanced** once (other storage, static IP, privileged off/on).

Unattended, with a setup token (the variable is exported into the container by the ct script):

```bash
export var_setup_token="$(openssl rand -hex 16)"
echo "setup token: $var_setup_token"
bash -c "$(curl -fsSL "$COMMUNITY_SCRIPTS_URL/ct/vigilant-vole.sh")"
unset var_setup_token
```

Then check, with `<ctid>` the new container's ID:

```bash
pct exec <ctid> -- systemctl status vigilant-vole --no-pager
pct exec <ctid> -- curl -fsS http://127.0.0.1:3001/healthz          # -> ok
pct exec <ctid> -- cat /etc/vigilant-vole/vigilant-vole.env
pct exec <ctid> -- cat /root/.vigilant-vole                            # installed version
```

1. Open the URL the script printed (`http://<ip>:3001`), finish the setup wizard (with a token:
   `/setup?token=<token>`).
2. Add a **Ping** monitor for your gateway and an **HTTP** monitor for `https://example.com`; both should
   go green (the HTTP one proves the CA store is there).
3. **Update path**: pretend an older version is installed and run the container's `update` command (it
   loads the ct script from the URL baked in at install time, i.e. your branch):

   ```bash
   pct exec <ctid> -- bash -c 'echo 0.2.0 > /root/.vigilant-vole'
   pct enter <ctid>
   update            # inside the container; expect "Update available ... -> 0.3.0", stop, deploy, start
   exit
   ```

   Your monitors and history must still be there afterwards. Run `update` again: it should say
   "No update available".
4. **arm64** (only if you have an arm64 Proxmox host, e.g. PXVirt on a Pi 5 or Ampere): repeat the
   install there. If you can't, tick "arm64 not tested" in the PR (the CI build does run the aarch64
   binary on a native arm64 runner, but community-scripts wants `var_arm64=yes` verified on a real
   host; set it to `no` or comment the line out if you'd rather not claim it).
5. Clean up: `pct stop <ctid> && pct destroy <ctid>`.

## Test 2: only the install script, in a Debian 13 container you made

Useful for iterating on `install/vigilant-vole-install.sh` without the wizard. Create a plain
**Debian 13** CT (unprivileged, nesting on), open its console, and run:

```bash
apt update && apt install -y curl
export COMMUNITY_SCRIPTS_URL="https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/refs/heads/feature/proxmox-install/deploy/proxmox"
export app=vigilant-vole
export FUNCTIONS_FILE_PATH="$(curl -fsSL https://raw.githubusercontent.com/community-scripts/core/main/lxc/install.func)"
bash -c "$(curl -fsSL "$COMMUNITY_SCRIPTS_URL/install/vigilant-vole-install.sh")"
```

`FUNCTIONS_FILE_PATH` (the engine's `lxc/install.func`), `app` and `COMMUNITY_SCRIPTS_URL` are what the
host normally passes into the container; `customize` uses the last two to write the container's `update`
command. Everything else the script loads itself. This doesn't exercise the container wizard, so do Test 1
at least once.

## Opening the PR to DevScripts

**Check eligibility first.** The DevScripts pull-request template requires, for a new script, that the
application is **at least 6 months old**, **actively maintained**, has **600+ GitHub stars** and
publishes **official release tarballs**. Vigilant Vole has release tarballs once the binaries workflow has
run, but the repository was created in September 2026 and is far below 600 stars, so a PR opened now
would be closed. Until it qualifies, point users at the one-liner from
[docs/install-linux.md](../../docs/install-linux.md#proxmox-ve-lxc) (it runs these same files from this
repository), and open the PR when the numbers are there. Before opening it, compare the files with
DevScripts' current
[`AGENTS.md`](https://github.com/community-scripts/DevScripts/blob/main/AGENTS.md) template again,
since the conventions change often.

When it qualifies:

```bash
gh repo fork community-scripts/DevScripts --clone
cd DevScripts
git switch -c feat/vigilant-vole
VV=../Vigilant-Vole/deploy/proxmox            # path to your Vigilant Vole checkout
cp "$VV/ct/vigilant-vole.sh"              ct/vigilant-vole.sh
cp "$VV/install/vigilant-vole-install.sh" install/vigilant-vole-install.sh
cp "$VV/json/vigilant-vole.json"          json/vigilant-vole.json
# set "date_created" in json/vigilant-vole.json to today's date
git add ct/vigilant-vole.sh install/vigilant-vole-install.sh json/vigilant-vole.json
git commit -m "feat: add Vigilant Vole"
git push -u origin feat/vigilant-vole
gh pr create --repo community-scripts/DevScripts --base main --title "Vigilant Vole" --body-file -   # paste the template
```

Run the ct script from your fork once more before opening the PR (the PR also gets a test command from
their `pr-test-command` workflow):

```bash
export COMMUNITY_SCRIPTS_URL="https://raw.githubusercontent.com/Ferdinand99/DevScripts/refs/heads/feat/vigilant-vole"
bash -c "$(curl -fsSL "$COMMUNITY_SCRIPTS_URL/ct/vigilant-vole.sh")"
```

Fill in the [pull request template](https://github.com/community-scripts/DevScripts/blob/main/.github/pull_request_template.md):

- **Type of change**: 🆕 New script.
- **Prerequisites**: self-review, tested (say which Proxmox VE version), no breaking changes, no
  security risks (no credentials in the scripts; the setup token is optional and only read from
  `var_setup_token`).
- **arm64**: "arm64 supported" only if you ran Test 1 on an arm64 host, otherwise "arm64 not tested".
- **Tested on**: Proxmox VE (Incus only if you tried it).
- **Code & security review**: follows `CODE-AUDIT.md`/`CONTRIBUTING.md`; correct structure
  (`ct/`, `install/`, `json/`); no hardcoded credentials; **no Docker** (bare-metal binary); **no git pull**
  (`fetch_and_deploy_gh_release` in `prebuild` mode, `check_for_gh_release` for updates).
- **AI assistance**: these scripts were drafted with an AI assistant following DevScripts' `AGENTS.md`;
  say so, name the model, and confirm that you reviewed and tested them.
- **Description**, as `AGENTS.md` asks for the upstream research: "Upstream ships static musl binaries
  for x86_64/aarch64 on every GitHub release (built in CI from the Dockerfile's Alpine builder stage). No
  runtime, no database server: one binary, SQLite in `/var/lib/vigilant-vole`. Port 3001. Needs only
  `ca-certificates` (present in the Debian template). No default credentials: first-run setup wizard,
  optional `VV_SETUP_TOKEN`."
- **Application requirements**: tick the four boxes only when they're true.
- **Source**: https://github.com/Ferdinand99/Vigilant-Vole

Website metadata (name, description, logo, categories) lives in `json/vigilant-vole.json` in DevScripts
and is pushed to their database by their CI; after the script moves to ProxmoxVE, changes to it go through
the "Report Issue" link on the script's page instead.
