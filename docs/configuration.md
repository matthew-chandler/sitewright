# Configuration

Sitewright reads one TOML file. Every key has a default, so an empty file is valid.
Secrets (API keys) never go in this file; they live in `secrets.toml` in the data directory.

## Where the file is found

The first of these that applies:

1. `--config <file>` or the `SITEWRIGHT_CONFIG` environment variable (the file must exist).
2. `config.toml` inside the directory given by `--data-dir` or `SITEWRIGHT_DATA_DIR`.
3. `/etc/sitewright/config.toml`
4. `/var/lib/sitewright/config.toml`

With no file at all, the built-in defaults are used (a local-development setup).

The **data directory** is the first of: `--data-dir` / `SITEWRIGHT_DATA_DIR`; the file's
`data_dir` key; `/var/lib/sitewright`.

Unknown keys are rejected, so a typo is an error rather than a silently ignored setting.

## Keys

### Top level

| Key | Default | Meaning |
| --- | --- | --- |
| `data_dir` | `/var/lib/sitewright` | Where all state lives (database, sites, certificates). |

### `[server]`

| Key | Default | Meaning |
| --- | --- | --- |
| `mode` | `"proxy"` | `"proxy"`: behind nginx, Caddy or Cloudflare, which terminate TLS. `"standalone"`: the daemon also serves HTTPS itself and gets certificates from Let's Encrypt; see `[tls]`. |
| `listen` | `"127.0.0.1:8080"` | Plain-HTTP listen address. In standalone mode use `"0.0.0.0:80"`: it answers ACME challenges and redirects to HTTPS. Overridden by `serve --listen` or `SITEWRIGHT_LISTEN`. |
| `trusted_proxies` | `[]` | CIDR ranges whose `X-Forwarded-For`, `X-Forwarded-Proto` and `CF-Connecting-IP` headers are believed. With nginx on the same machine: `["127.0.0.1/32"]`. Leave empty when clients connect directly. |

### `[domains]`

| Key | Default | Meaning |
| --- | --- | --- |
| `zones` | `["localhost"]` | DNS zones sites live under. A new site's hostname is `<name>.<first zone>`. |
| `admin_host` | `admin.<first zone>` (`localhost` for the `localhost` zone) | Hostname of the admin UI. Its first label is reserved and cannot be used as a site name. |
| `reserved_names` | `[]` | Extra site names that may not be used, besides the built-in `admin`, `www`, `api`, `mail` and the admin host's label. List the subdomains of the zone that other services already use. |
| `admin_url` | built from `public_scheme`, `admin_host` and `public_port` | Full origin of the admin UI, for example `https://admin.example.com`. Set it when the admin UI's scheme or port differs from the sites'. Passkeys are bound to this origin, so changing it later means enrolling again. |
| `public_scheme` | `"http"` | `"http"` or `"https"`: how visitors reach sites. Used to build links. |
| `public_port` | none | Public port when it is not the scheme's default, for example `8080` in local development. |

### `[sites]`

| Key | Default | Meaning |
| --- | --- | --- |
| `max_file_bytes` | `1048576` (1 MiB) | Largest single file in a site. Files the owner uploads (`uploads/`) are exempt. |
| `max_site_bytes` | `20971520` (20 MiB) | Largest total size of a site's files, not counting `uploads/`. |
| `keep_releases` | `10` | Release directories kept on disk per site. Older releases stay in git and can still be rolled back to. |
| `purge_after_days` | `7` | How long a deleted site stays restorable. |
| `allowed_extensions` | common web file types | File extensions a site may contain (lowercase, no dot). |

### `[llm]`

See [providers.md](providers.md) for the provider keys and examples.

| Key | Default | Meaning |
| --- | --- | --- |
| `default` | first provider | Id of the provider used when a run does not name one. |
| `[[llm.providers]]` | none | One table per provider. |
| `[[llm.prices]]` | none | `model`, `input_per_mtok`, `output_per_mtok`: US dollars per million tokens. |

### `[agent]`

| Key | Default | Meaning |
| --- | --- | --- |
| `max_turns` | `40` | Most model calls in one run. |
| `max_run_tokens` | `1000000` | Most tokens (input plus output, summed over all calls) in one run. |
| `daily_spend_cap_usd` | `5.0` | Most US dollars spent across all runs per UTC day. Needs a price for the model. |
| `max_concurrent_runs` | `2` | Most runs in progress at once. There is always at most one per site. |

Hitting a limit ends the run cleanly with status `limit` and a message naming the limit.

### `[metrics]`

See [metrics.md](metrics.md) for what is collected.

| Key | Default | Meaning |
| --- | --- | --- |
| `uptime_interval_secs` | `60` | Seconds between uptime checks of each site. `0` turns them off. |
| `slow_ms` | `2000` | A check slower than this marks the site as degraded. |
| `uptime_resolve_local` | `false` | Check sites through the daemon's own listener instead of public DNS. For local development, where `*.localhost` does not resolve for the daemon. |
| `retention_days` | `30` | Days raw requests, events and checks are kept. Daily rollups are kept forever. |

### `[backends]`

Site backends: agent-written JavaScript that answers a site's `/api/` requests. See
[backends.md](backends.md).

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Turn backends off entirely. Sites that have one can then not be published. |
| `request_timeout_secs` | `10` | Longest a handler may run. |
| `memory_mb` | `64` | JavaScript memory per site. |
| `max_db_mb` | `100` | Largest a site's database may grow. |
| `idle_secs` | `300` | A site's worker stops after this long without requests. |
| `max_workers` | `8` | Most worker processes alive at once. |
| `jobs` | `true` | Let backends run background jobs and scheduled tasks. |
| `job_timeout_secs` | `30` | Longest a job or scheduled task may run, not counting time spent waiting for the database, outside requests or models. |

### `[tls]`

Used in standalone mode. See [dns-and-tls.md](dns-and-tls.md#standalone-mode).

| Key | Default | Meaning |
| --- | --- | --- |
| `https_listen` | `"0.0.0.0:443"` | HTTPS listen address. |
| `acme_directory` | Let's Encrypt production | ACME directory URL. For testing: `https://acme-staging-v02.api.letsencrypt.org/directory`. |
| `acme_contact` | none | Email address for the ACME account (expiry notices). |
| `acme_root_cert` | none | PEM file with an extra root to trust for the ACME server itself. Only for test servers such as Pebble. |
| `redirect_http` | `true` | Redirect plain HTTP to HTTPS. ACME challenges and `/.well-known/` are always answered. |
| `include_admin_host` | `true` | Also get a certificate for the admin hostname. |
| `precheck` | `true` | Before requesting a certificate, check that the hostname reaches this server over HTTP. |
| `scan_interval_secs` | `30` | Seconds between scans for hostnames that need a certificate. |

### `[updates]`

See [operations.md](operations.md#updating).

| Key | Default | Meaning |
| --- | --- | --- |
| `check` | `true` | Look for a newer release once a day and show it in Settings. |
| `auto` | `false` | Install patch and minor releases automatically. A new major version is never installed automatically. |

## Secrets

`secrets.toml` in the data directory holds API keys:

```toml
[keys]
azure = "..."
```

It must be mode 0600; the daemon refuses a file other users can read. Its values are never
logged, never shown, and never included in errors.

## Examples

Local development (`dev/local.toml`):

```toml
data_dir = "dev/data"

[server]
listen = "127.0.0.1:8080"

[domains]
zones = ["localhost"]
public_port = 8080
```

Behind nginx on the same machine:

```toml
[server]
listen = "127.0.0.1:18080"
trusted_proxies = ["127.0.0.1/32"]

[domains]
zones = ["example.com"]
admin_host = "admin.example.com"
public_scheme = "https"
```

## Environment variables

| Variable | Meaning |
| --- | --- |
| `SITEWRIGHT_CONFIG` | Same as `--config`. |
| `SITEWRIGHT_DATA_DIR` | Same as `--data-dir`. |
| `SITEWRIGHT_LISTEN` | Same as `serve --listen`. |
| `SITEWRIGHT_LOG_FORMAT` | `json` for structured logs; anything else for human-readable logs. |
| `RUST_LOG` | Log filter, for example `info` or `sw_sites=debug`. |
