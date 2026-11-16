# Operations

## Where everything lives

All state is in one data directory (default `/var/lib/sitewright`):

```text
config.toml              # settings (docs/configuration.md)
secrets.toml             # API keys, mode 0600
admin.sock               # the CLI talks to the daemon here
db/sitewright.db         # SQLite: sites, releases, runs, metrics, passkeys
sites/<name>/repo/       # each site's source, a git repository
sites/<name>/releases/   # immutable published copies
certs/, acme/            # certificates and the ACME account (standalone mode)
skills/                  # your own skills
exports/                 # generated archives, deleted after a day
backups/                 # database copies taken before each migration
```

## Everyday commands

```sh
sitewright doctor                        # check the installation
sitewright site list
sitewright site info <name>              # hostnames and release history
sitewright site rollback <name>          # back to the previous release
sitewright site import <name> <dir>      # publish a folder of static files
sitewright prompt <name> "..."           # ask the agent for a change
sitewright export <name> --format folder # or: context (a Docker build context)
sitewright admin invite                  # one-time passkey enrollment link
sitewright update                        # install the latest signed release
```

Commands talk to the running daemon over `admin.sock`, so run them as the daemon's user
or as root. If the data directory is not the default, pass `--data-dir` or set
`SITEWRIGHT_DATA_DIR`.

Logs: `journalctl -u sitewright -f` (JSON lines; set `RUST_LOG=debug` in the unit for more).

## Backup and restore

Everything worth keeping is the data directory.

**Back up** (safe while the daemon runs):

```sh
sudo -u sitewright sqlite3 /var/lib/sitewright/db/sitewright.db ".backup '/var/lib/sitewright/backups/manual.db'"
sudo tar -czf sitewright-backup.tar.gz -C /var/lib \
  --exclude='sitewright/db/sitewright.db*' --exclude='sitewright/admin.sock' --exclude='sitewright/exports' sitewright
```

The first command takes a consistent copy of the database; the archive then holds that
copy and every file. Without `sqlite3`, stop the daemon and archive the whole directory.

**Restore:**

```sh
sudo systemctl stop sitewright
sudo tar -xzf sitewright-backup.tar.gz -C /var/lib
sudo mv /var/lib/sitewright/backups/manual.db /var/lib/sitewright/db/sitewright.db
sudo chown -R sitewright:sitewright /var/lib/sitewright
sudo systemctl start sitewright
```

## Moving to a new server

1. Install Sitewright on the new server (do not run `init`).
2. Stop the old daemon and copy the data directory across, as in *Restore*.
3. Start the new daemon and run `sitewright doctor`.
4. Repoint DNS (the wildcard and the admin record) to the new server's address.
5. In standalone mode the certificates come along; they renew from the new server once
   DNS has moved. Passkeys keep working because the admin hostname is unchanged.

## Changing your domain

To move every site from one zone to another (say from `dev.example.com` to `example.com`),
or to rename the admin hostname:

1. Make the new names reach the server (DNS, and the proxy in front if there is one).
2. In `config.toml` set `domains.zones`, `domains.admin_host` and, if used,
   `domains.admin_url`; add `domains.reserved_names` for subdomains other services use.
   Restart the daemon.
3. Give each existing site its new address and make it the primary one:
   ```sh
   sitewright site hostname add <name> <name>.example.com --primary
   ```
   The old hostname stays attached until you remove it.
4. **Enroll a new passkey**: `sitewright admin invite`. Passkeys are bound to the admin
   hostname, so ones made for the old hostname stop working when it changes. Remove them
   in Settings afterwards.

New sites are created in the first zone listed.

## Updating

```sh
sudo sitewright update --check    # is there a newer release?
sudo sitewright update            # download, verify, install, restart
sudo sitewright update --rollback # go back to the previous version
```

What `update` does:

1. Asks GitHub for the latest release and compares versions.
2. Downloads the archive for this machine, its minisign signature and its checksum, and
   **verifies both** against the public key compiled into the running binary. Anything
   that does not verify is not installed.
3. Keeps the current binary as `sitewright.prev` beside the new one and swaps atomically.
4. Asks the daemon to exit; systemd (`Restart=always`) starts the new version.
5. On start, the new version copies the database to `backups/` before running any
   migration.

`--rollback` puts `sitewright.prev` back and, if the newer version migrated the database,
restores the copy taken before that migration (the newer database is kept beside it as
`sitewright.db.from-<version>`, never deleted). Anything created after the update is not
in the restored database; site files and git history are unaffected.

Automatic updates are off by default. To install patch and minor releases
automatically (never a new major version):

```toml
[updates]
auto = true
```

Settings shows when a newer version is available. Built-in skills update with the binary;
your own skills in `skills/` are never touched.

## Troubleshooting

| Symptom | Likely cause and fix |
| --- | --- |
| `cannot reach the daemon at .../admin.sock` | The daemon is not running, you are not its user (use `sudo`), or its data directory is elsewhere (`--data-dir`). |
| A site's address shows "404: there is no site at this address" | The hostname is not one of a published site's hostnames. `sitewright site info <name>`; check the zone in `config.toml`. |
| A site's address shows another server's page | DNS or the proxy sends the name somewhere else. **Settings → Domains → Check**. |
| Passkey sign-in says it needs HTTPS | The admin UI must be reached over `https://` on exactly the configured admin hostname (`domains.admin_url`). |
| Lost every passkey | `sudo sitewright admin invite` and enroll again. Shell access is the root of trust. |
| "the provider rejected the API key" | Fix the key in **Settings → Models** (or `secrets.toml`), then **Test**. |
| A run ends with status `limit` | It hit `agent.max_turns`, `agent.max_run_tokens` or `agent.daily_spend_cap_usd`; the message says which. |
| "site ... already has a run in progress" | One run per site at a time. Open the run from the site page, or cancel it. |
| Publishing is refused | The site failed validation (broken link, missing `index.html`, over the size limits). The message lists the problems. |
| No certificate in standalone mode | `sitewright doctor` shows the reason per hostname: usually DNS not pointing here yet, or port 80 blocked. |
| Cloudflare error 525 or 526 | The web server in front of Sitewright failed TLS for that hostname; see [dns-and-tls.md](dns-and-tls.md#cloudflare). |
| The daemon will not start after an upgrade | `journalctl -u sitewright -e`, then `sudo sitewright update --rollback`. |

## `sitewright doctor`

Checks, and prints `ok`, `warn` or `FAIL` for each: the configuration file, data
directory permissions, `secrets.toml`, that the daemon is running and matches this binary,
whether an update is available, that the admin UI is on HTTPS, enrolled passkeys,
configured models and their keys, today's spend against the cap, every site's status, and
in standalone mode every hostname's certificate. It exits non-zero only on failures.
