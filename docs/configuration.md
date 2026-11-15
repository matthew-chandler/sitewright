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
| `mode` | `"proxy"` | `"proxy"`: behind nginx, Caddy or Cloudflare, which terminate TLS. `"standalone"`: the daemon binds 80/443 and gets its own certificates (arrives in M6). |
| `listen` | `"127.0.0.1:8080"` | Plain-HTTP listen address. Overridden by `serve --listen` or `SITEWRIGHT_LISTEN`. |
| `trusted_proxies` | `[]` | CIDR ranges whose `X-Forwarded-For`, `X-Forwarded-Proto` and `CF-Connecting-IP` headers are believed. With nginx on the same machine: `["127.0.0.1/32"]`. Leave empty when clients connect directly. |

### `[domains]`

| Key | Default | Meaning |
| --- | --- | --- |
| `zones` | `["localhost"]` | DNS zones sites live under. A new site's hostname is `<name>.<first zone>`. |
| `admin_host` | `admin.<first zone>` (`localhost` for the `localhost` zone) | Hostname of the admin UI. Its first label is reserved and cannot be used as a site name. |
| `public_scheme` | `"http"` | `"http"` or `"https"`: how visitors reach sites. Used to build links. |
| `public_port` | none | Public port when it is not the scheme's default, for example `8080` in local development. |

### `[sites]`

| Key | Default | Meaning |
| --- | --- | --- |
| `max_file_bytes` | `1048576` (1 MiB) | Largest single file in a site. |
| `max_site_bytes` | `20971520` (20 MiB) | Largest total size of a site's files. |
| `keep_releases` | `10` | Release directories kept on disk per site. Older releases stay in git and can still be rolled back to. |
| `purge_after_days` | `7` | How long a deleted site stays restorable. |
| `allowed_extensions` | common web file types | File extensions a site may contain (lowercase, no dot). |

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
