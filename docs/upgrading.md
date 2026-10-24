# Upgrading, backups and rolling back

Vigilant Vole keeps everything in one SQLite database (`vigilant-vole.db` in the data directory). A new
version brings its schema changes as **migrations** that run by themselves when it starts, after it has
written a copy of the database to `backups/`. Upgrading is "pull the new version, restart"; rolling back
is "restore that copy, start the old version".

- [Upgrade](#upgrade): [Docker](#docker) · [Linux installer](#linux-installer-systemd) · [Proxmox LXC](#proxmox-ve-lxc) · [Unraid](#unraid)
- [What happens on start](#what-happens-on-start)
- [Roll back](#roll-back)
- [Downgrades](#downgrades)
- [Backup and restore drill](#backup-and-restore-drill)
- [How upgrades are tested](#how-upgrades-are-tested) (and what to do when releasing)

## Upgrade

You can skip versions: every migration since your version runs in order. Read the release notes first;
breaking changes are listed there.

### Docker

With the published image (`image: ghcr.io/ferdinand99/vigilant-vole:<tag>`):

```bash
docker compose pull
docker compose up -d
docker compose logs -f vigilant-vole   # "applied database migration" lines, then "listening on"
```

With the repository's `docker-compose.yml`, which builds from source: `git pull && docker compose up -d --build`.

Pin a version tag (`ghcr.io/ferdinand99/vigilant-vole:0.5.0`) rather than `latest` if you want to choose
when upgrades happen; that also makes rolling back a one-line change.

### Linux installer (systemd)

```bash
bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)" -- update
# a specific release:
bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)" -- update --version v0.5.0
```

`update` replaces the binary, keeps `/etc/vigilant-vole/vigilant-vole.env` and the data in
`/var/lib/vigilant-vole`, restarts the service and waits until it is healthy. Logs:
`journalctl -u vigilant-vole -e`.

### Proxmox VE (LXC)

Run `update` in the container's console. It stops the service, installs the latest release binary and
starts it again. Take a snapshot first (`pct snapshot <ctid> before-update` on the host, or
**Snapshots → Take Snapshot**): it is the quickest way back.

### Unraid

**Docker → Check for Updates → apply update** (or **Force Update** on the container). To stay on a
version, set the repository to `ghcr.io/ferdinand99/vigilant-vole:<version>` in the container settings.

## What happens on start

1. A restore uploaded under **Settings → Backup** is swapped in, if one is pending (see below).
2. If the database is from an **older** version, a consistent copy is written to
   `backups/vigilant-vole-before-<first new migration>-<unix time>.db` (in Docker
   `./data/backups/`, on Linux/Proxmox `/var/lib/vigilant-vole/backups/`, on Unraid
   `/mnt/user/appdata/vigilant-vole/backups/`).
3. Each pending migration runs in its own transaction and is recorded in the `schema_migrations` table.
   Besides Vigilant Vole's own migrations (`0001_init.sql` …) the table holds the shared kit's
   (`kit/0001_core.sql` …); see [vole-kit.md](vole-kit.md#migrations).
4. Notification channel secrets still stored in plaintext (saved by 0.11.x or earlier, or restored
   from a backup) are sealed with the instance key, `session.key` in the data directory. The log says
   how many channels were sealed. A later start finds nothing left to seal.
5. The server starts.

If a migration fails, its transaction is rolled back: nothing of that migration is kept, the ones
before it stay applied, and the server **does not start**. The log says which migration failed, which
migration the database is at, and where the copy from step 2 is:

```
database migration failed: migration 0102_status_page_subscribers.sql failed: table status_page_subscribers already exists.
Nothing of the failed migration was kept; the database is at 0101_incident_updates.sql.
The copy made before upgrading is /data/backups/vigilant-vole-before-0100_maintenance_strategies-1792580000.db
```

Fixing the cause and starting again continues where it stopped. Or roll back (next section) and report
the error. Stop a container that keeps restarting after a failed migration: every attempt writes
another copy to `backups/`.

Backups are never deleted automatically. Once the new version runs fine, remove old copies from
`backups/` when you like (keep the newest one or two).

## Roll back

Restore the copy taken before the upgrade and start the previous version. Always do both: an older
version can't use a database a newer one has migrated (see [Downgrades](#downgrades)).

**Docker**

```bash
docker compose down
cd data
mkdir -p backups/rolled-back
mv vigilant-vole.db* backups/rolled-back/          # the database and its -wal/-shm files
cp backups/vigilant-vole-before-<…>.db vigilant-vole.db
cd ..
# set the old tag in docker-compose.yml, e.g. image: ghcr.io/ferdinand99/vigilant-vole:0.4.0
docker compose up -d
# (building from source instead: git checkout v0.4.0 && docker compose up -d --build)
```

Move the `-wal` and `-shm` files together with the database, never leave them next to a restored
file: SQLite would replay the old write-ahead log into it.

**Linux installer**

```bash
systemctl stop vigilant-vole
cd /var/lib/vigilant-vole
mkdir -p backups/rolled-back && mv vigilant-vole.db* backups/rolled-back/
cp backups/vigilant-vole-before-<…>.db vigilant-vole.db && chown vigilant-vole: vigilant-vole.db
bash -c "$(curl -fsSL https://raw.githubusercontent.com/Ferdinand99/Vigilant-Vole/main/deploy/install.sh)" -- update --version v0.4.0
```

**Proxmox**: roll the container back to the snapshot (`pct rollback <ctid> before-update`). Without a
snapshot, do as for Linux above, but put the old binary in place by hand
(`/opt/vigilant-vole/vigilant-vole` from the release's `vigilant-vole-<arch>-unknown-linux-musl.tar.gz`;
the service there runs as root, so skip the `chown`).

**Unraid**: stop the container, swap the files in `/mnt/user/appdata/vigilant-vole` as for Docker, set
the old tag, start it.

Anything recorded since the upgrade (heartbeats, changes you made) is in the moved-aside database, not in
the restored copy.

## Downgrades

Running an older version on a database a newer version has migrated is **not supported**.

- **From 0.5.1 on** the app refuses to start on such a database, before changing anything:

  ```
  database migration failed: the database was created by a newer Vigilant Vole (schema 0110_…);
  this version only knows schemas up to 0103_subscriber_tokens. Upgrade Vigilant Vole again, or
  restore the backup made before that upgrade from /data/backups
  ```

  Settings → Backup also refuses to restore a backup made by a newer version.
- **The release after 0.6.x** adds the migration `0104_adopt_kit_core.sql`. It changes no table: it
  records that the users, API token and settings tables now belong to the shared kit (vole-kit), by
  adding the row `kit/0001_core.sql` to `schema_migrations`. Upgrading from 0.6.x therefore takes the
  usual copy (`vigilant-vole-before-0104_adopt_kit_core-….db`). **0.6.x and earlier can't open a
  database migrated by it**: they don't know those two rows and refuse to start, as above (the message
  names `schema kit/0001_core`). That is expected. To go back to 0.6.x, restore that copy.
- **0.9.0** adds `0105_heartbeat_raw_status.sql`: a nullable `raw_status` column on `heartbeats` (the
  real check result during maintenance). Older rows keep `NULL`; nothing is rewritten. 0.8.x refuses a
  database migrated by it, as above; restore the copy made before the upgrade to go back.
- **0.12.0** records `kit/0004_sealed_secrets.sql` (no table changes) and then seals the notification
  channel secrets. The upgrade from 0.11.x takes the usual copy first
  (`vigilant-vole-before-kit-0004_sealed_secrets-….db`), with the secrets still in plaintext. 0.11.x
  refuses a database 0.12.0 has started on. It couldn't read the sealed secrets and would send them to
  the providers as they are. To go back, restore that copy.
- **0.5.0 and older** don't check. They start, and in a test of 0.4.0 on a 0.5.0 database the dashboard,
  status pages and API writes worked, but anything the newer version stores (incident updates,
  subscribers, new maintenance options) is ignored and can end up inconsistent. Don't rely on it:
  roll back as above.

## Backup and restore drill

Do this once on a new install, and after major upgrades. It takes five minutes and is automated in
`tests/backup_restore.rs`, but nothing replaces having done it on your own instance.

1. **Settings → Backup → Database backup**: download the snapshot. It holds everything, users, tokens
   and secrets included: store it like a password. Notification channel secrets, sealed with the
   instance key (`session.key`) in the live database, are opened in the snapshot, so it restores on
   any instance; the restore seals them again.
2. Note a few things to check later: the number of monitors (shown on the Backup page), a status page,
   an API token you use.
3. **Restore on next start**: upload the file you just downloaded. The page confirms how many monitors
   it holds and shows the restore as pending.
4. Restart (`docker compose restart`, `systemctl restart vigilant-vole`). The current database is
   moved to `backups/vigilant-vole-before-restore-<unix time>.db` and the upload takes its place.
5. Check what you noted: monitors, the status page, the API token, and sign in with 2FA if you use it.
6. **Config as code**: `GET /api/v1/config?format=yaml&secrets=1` (admin) exports the configuration
   without history. Import it into a scratch instance (`docker run --rm -p 3002:3001
   ghcr.io/ferdinand99/vigilant-vole`, finish setup, create an API token, `POST /api/v1/config` with
   the file as the body), then import the same file again with `?dry_run=1`: every item must say
   *unchanged*.

Keep backups somewhere else than the machine that runs Vigilant Vole. A copy of the data directory taken
while the app is **stopped** is a valid backup too. Copy it whole: `vigilant-vole.db` without its
`session.key` restores, but the notification channels' secrets are then unreadable (each channel's edit
page asks for them again, and its alerts fail until they're entered). The pre-migration copies in
`backups/` need the same `session.key`.

## How upgrades are tested

`tests/fixtures/upgrade/<version>/` holds a database saved by every release, made by
[`tools/upgrade-fixtures/generate.sh`](../tools/upgrade-fixtures/generate.sh). The script runs that
release's container image, completes setup and fills it through the release's own UI and API: users with
every role, two-factor authentication, API tokens, a monitor of every type the release supports (groups,
tags, push tokens, agent settings, HTTP auth and other secrets), a notification channel per provider,
status pages with sections, incidents, custom CSS, a custom domain and a password, maintenance windows,
remote agents and browsers, settings. It then lets the instance run for a minute against local targets
so there are heartbeats and events, and saves the database with a `manifest.json` (credentials, row
counts, a few exact values).

`cargo test --test upgrade` (a CI job) upgrades each one with the current code and checks that:

- every pending migration runs in order, `schema_migrations` lists all of them, and the pre-migration
  copy equals the release's database;
- the schema is exactly that of a fresh database (tables, columns, indexes, triggers);
- every row and column survives; users, roles, passwords, TOTP secrets and recovery codes, monitors with
  their secrets, channels, tags, status pages, incidents, maintenance, agents and settings read back
  through the app's own code;
- the server starts on it: health check, admin and 2FA sign-in, dashboard, status page by slug and by
  custom domain, the release's API tokens (scopes kept), a push to its push token, its agent token;
- upgrading twice changes nothing; a migration that fails half-way is rolled back and retried; a
  database from a newer version is refused.

`cargo test --test backup_restore` runs the drill above against the newest fixture, plus restoring a
backup from the oldest release.

### When releasing

After the release's image is on GHCR:

1. `tools/upgrade-fixtures/generate.sh v0.5.2` (needs Docker; takes about two minutes). The script
   bind-mounts local folders, so with a remote Docker context (`ssh://`) copy `tools/upgrade-fixtures/`
   to that host and run it there with `OUT_DIR` set, then copy the result back.
2. Add the version to `RELEASES` in `tests/upgrade.rs`.
3. `cargo test --test upgrade --test backup_restore`.
4. Commit `tests/fixtures/upgrade/v0.5.2/` (about 300 KB).

If the generator reports items the new release rejected, extend the seed data in the script (new
monitor types, fields or providers go into its `MONITORS` list or the channel values) so the fixture
covers them.
