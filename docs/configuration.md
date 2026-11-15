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
| `admin_url` | built from `public_scheme`, `admin_host` and `public_port` | Full origin of the admin UI, for example `https://admin.example.com`. Set it when the admin UI's scheme or port differs from the sites'. Passkeys are bound to this origin, so changing it later means enrolling again. |
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
