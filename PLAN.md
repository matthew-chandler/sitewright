# Prompt-to-site host — v1 implementation plan

Nov 11, 2026 · @Matthew

This plan is the full brief for an autonomous coding agent to build v1 of **Sitewright**, a self-hosted, open-source tool that turns a prompt from your phone into a live website on its own subdomain. Build it end to end, milestone by milestone, with minimal input from the owner.

## How to use this plan

You are the coding agent. Read the whole plan before writing code. It states intent and decisions, not exact API signatures: check every crate's current version and docs on crates.io and docs.rs before using it, because APIs change.

- **Work autonomously.** When the plan is silent, ambiguous or wrong, make the call yourself, record it as an ADR in `docs/adr/`, and keep going. Only ask Matthew for the items in *User inputs and decision policy*.
- **One milestone at a time (M0 to M6).** Each ends green: `cargo fmt`, `clippy -D warnings`, all tests passing, docs updated, `docs/STATUS.md` updated, PR merged.
- **Never block.** If a user input is missing, stub the feature behind config, write the exact request under "Needs from Matthew" in `docs/STATUS.md`, and continue with other work.
- **Keep `docs/STATUS.md` current:** done, in progress, next, blockers, and what to test by hand.
- **Git hygiene:** a branch per milestone, Conventional Commits, PR via `gh`, squash-merge when CI is green. Never commit secrets. Never force-push `main`.
- **On the VPS, act only as the `devtest` user.** Never attempt `sudo`, and never touch anything outside that user's home, its systemd user units, or the paths the bootstrap script grants. Matthew's existing production sites must never be affected.
- **Prefer boring, well-maintained crates.** If a chosen crate is unmaintained or unfit, swap it and write an ADR.

### Repository setup (first task of M0)

Matthew starts Claude Code in `~/projects/sitewright`, which contains only `PLAN.md` (this plan) and possibly `.env`. That directory becomes the project repo; never create a second checkout elsewhere.

1. Decide the final project name: check crates.io and GitHub for conflicts, and keep "sitewright" if it's free. Keep the directory name as is even if the project name changes.
2. Before the first commit, create `.gitignore` covering `.env`, `target/`, local data directories and any secrets files. Run `git status` and confirm `.env` is not listed before every commit.
3. `git init -b main`, then commit `PLAN.md` (kept at the repo root) together with the initial scaffold.
4. `gh` is already authenticated, so create the public GitHub repo yourself: `gh repo create <name> --public --source=. --remote=origin --push`, with a one-line description and topics such as `rust`, `ai-agents`, `self-hosted`, `static-sites`.
5. Protect `main` so merges require green CI (`gh api` on branch protection). If that fails, note it in `docs/STATUS.md` and move on.

## Project context and goals

The goal is to describe a website from a phone and have it live at `<name>.<domain>` minutes later, on a server you own, with any LLM provider and any domain registrar.

**Owner:** Matthew Chandler, a software engineer. The project is open source, a learning vehicle (Rust with AI, agent tools, agent skills) and a resume piece. Code quality, tests, docs and a clean commit history matter as much as features.

**Existing setup to respect:**

- A Racknerd VPS (x86\_64 Linux; confirm the distro and architecture with `uname -a` and `/etc/os-release` over SSH) runs nginx. It already hosts Matthew's sites on `machandler.com`.
- DNS for `machandler.com` is on Cloudflare. Existing sites are proxied (orange cloud) and Cloudflare handles public TLS.
- Development happens in WSL (Ubuntu) on Matthew's PC. The agent can SSH to the VPS as a restricted `devtest` user for testing.

**Owner preferences:**

- Frontend stack is the agent's choice; frameworks are fine. Keep the footprint light and keep the single-binary deliverable (embed built assets).
- Markdown for all docs.
- Primary LLM for development: the Azure OpenAI gpt-5-mini deployment (500K TPM) through its OpenAI-compatible v1 endpoint, with credentials in .env at the repo root. Anthropic and Gemini must work too.

**Design principles:**

- **One static binary.** No runtime dependencies beyond a Linux kernel; Docker is never required.
- **Registrar- and provider-agnostic.** The user adds DNS records by hand at any host; the tool tells them exactly what to add and verifies it.
- **Deterministic where possible, agentic where useful.** Fixed operations are tools written in Rust. Judgment (design, editing, dashboards) is the agent's job, guided by skills.
- **Safe by default.** The agent cannot run shell commands or reach the network, and destructive actions need a human to confirm.

## v1 scope

v1 ships every part of the project whiteboard: the PWA dashboard, the Rust daemon (API and login, agent loop, site manager, web server and TLS, metrics, updater), three LLM provider adapters, and registrar-agnostic DNS guidance.

**In v1:**

- Static sites only (HTML, CSS, JS, images the agent writes), one per hostname, each git-versioned with one-click rollback.
- Serving in **proxy mode** (behind nginx, Caddy or Cloudflare) and **standalone mode** (own ports 80/443 with automatic Let's Encrypt certificates).
- An agent that creates, edits, validates, publishes and deletes sites through Rust tools, guided by built-in and user-supplied skills (agentskills.io format).
- LLM adapters: OpenAI-compatible (covers OpenAI, Azure OpenAI, OpenRouter, Groq, Ollama and more), Anthropic, Gemini.
- A mobile-first web UI, installable as a PWA, with passkey login, live agent progress, release history, settings and dashboards.
- Metrics: request logs, a privacy-friendly event beacon, uptime checks, a default dashboard, and an AI-designed dashboard per site.
- Signed GitHub releases and a `sitewright update` command with rollback.
- `export_site`: the published site as a folder archive, or as a Docker build context (Dockerfile plus files). No Docker needed on the server.
- A CLI covering every core operation, plus complete docs.

**Not in v1** (keep the code open to these, but don't build them):

- Allow using Claude Pro plan quota as an LLM to use
- Sites with backends, and running sites in containers.
- Publishing to other hosts (Cloudflare Pages, GitHub Pages), replication, Litestream, a Kubernetes operator.
- DNS provider APIs (keep a `DnsProvider` trait stub only).
- A native mobile app, multiple users or teams, macOS or Windows hosts.
- Agent web browsing or shell access, and image uploads into prompts.

## Architecture overview

One daemon process does everything: it serves the sites, hosts the admin UI and API, runs the agent, collects metrics and updates itself. The only outside services are the LLM APIs, Let's Encrypt (in standalone mode) and GitHub Releases.

&#91;embedded content: v1 architecture · one daemon, six parts\]

The agent (highlighted) is the only part that calls an LLM, and it can change sites only through the site manager's tools.

- **API + login:** passkey auth, sessions, the HTTP API behind the UI, and a Unix-socket admin API for the CLI.
- **Agent loop:** turns prompts into tool calls against the site manager and metrics; streams progress to the UI.
- **Site manager:** sites, hostnames, git repos, immutable releases, validation, publish and rollback, export.
- **Web server + TLS:** routes requests by `Host` to the current release; proxy mode or standalone mode with ACME.
- **Metrics:** request log, event beacon, uptime checks, rollups, default and AI dashboards.
- **Updater:** checks GitHub Releases, verifies signatures, swaps the binary, rolls back.

## Tech stack and key decisions

The stack is Rust stable (edition 2024) on tokio and axum, with SQLite for state and server-rendered HTML for the UI, all shipped as one static musl binary. Record each row below as part of ADR-0001; replace a choice only with a new ADR.

| Area | Choice | Why |
| --- | --- | --- |
| Language and build | Rust stable, edition 2024; `x86_64-` and `aarch64-unknown-linux-musl` static binaries via `cargo-zigbuild` | One file to install and update |
| Async and HTTP | `tokio`, `axum`, `tower-http` (static files, compression, tracing) | Mature, well documented |
| TLS and ACME | `rustls` plus `instant-acme` (HTTP-01), with a custom per-hostname certificate resolver | HTTP-01 works while nginx owns port 443 in dev; full control over which hostnames get certificates |
| Database | SQLite via `sqlx` (WAL mode, embedded migrations, offline query checking) | Zero setup; one file to back up |
| Site versioning | `git2` with vendored libgit2, network features off | Commits, diffs and rollback without a `git` binary |
| HTTP client | `reqwest` with rustls | LLM APIs, update checks, uptime checks |
| LLM access | Own `LlmProvider` trait with three adapters on `reqwest`; no LLM framework crate | Full control of tool calling and streaming; a core resume piece |
| UI | Agent's choice: server-rendered (e.g. `askama` + `htmx`) or a framework (e.g. Svelte) over the JSON API; built assets embedded with `rust-embed` | Either keeps one binary; record the choice in an ADR |
| Charts | Vega-Lite rendered by `vega-embed` (vendored) | A declarative spec the agent can write and the server can validate |
| Passkeys | `webauthn-rs` | Phishing-resistant login with no passwords |
| CLI and config | `clap`; TOML config via `serde`; separate `secrets.toml` | Plain-text config that's easy to review |
| Logging | `tracing` with JSON output in production | Structured logs for operations |
| DNS checks | `hickory-resolver` plus an HTTP token check | Works behind any proxy |
| Releases | `cargo-dist`; `minisign` signatures checked with `minisign-verify`; `self-replace` for the binary swap | Signed self-updates |
| Testing | `cargo-nextest`, `insta` snapshots, `wiremock` provider fixtures, Pebble for ACME | Fast, deterministic CI |
| Supply chain | `cargo-deny` (licenses, advisories, bans) | Clean dependency hygiene |
| License | MIT OR Apache-2.0 | Rust community default |

## Storage layout and data model

All state lives in one data directory, so moving to a new server means copying that directory and repointing DNS. Default `/var/lib/sitewright`; in dev, `~/sitewright-data`.

```text
<data>/
  config.toml            # non-secret settings (or /etc/sitewright/config.toml)
  secrets.toml           # API keys, mode 0600, never logged
  db/sitewright.db       # SQLite, WAL mode
  sites/<name>/repo/     # git working tree: the site's source
  sites/<name>/releases/<sha>/   # immutable published copies (keep last 10)
  certs/<hostname>/      # cert.pem, key.pem, meta.json (standalone mode)
  acme/account.json      # ACME account key
  skills/                # user-added skills (built-ins are embedded)
  exports/               # generated export archives, auto-cleaned
  backups/               # DB copy taken before each migration or update
```

Serving reads only from `releases/<current sha>/`. Publishing commits the repo, copies the tree to a new release directory, then switches the `current_release` pointer in one DB transaction, so a release goes live all at once and rollback is instant.

**Tables** (exact columns are the agent's call; keep migrations additive):

| Table | Holds |
| --- | --- |
| `sites` | name, title, status, `current_release`, timestamps, `deleted_at` (soft delete, purged after 7 days) |
| `site_hostnames` | one or more hostnames per site, one marked primary |
| `releases` | site, commit sha, message, author (agent or user), created\_at |
| `agent_runs` | site, prompt, status, provider, model, tokens in/out, cost, error, timestamps |
| `agent_events` | ordered event stream per run (messages, tool calls, results), replayable in the UI |
| `requests` | ts, site, method, path, status, bytes, duration, referrer host, UA class, visitor hash |
| `events` | beacon events: ts, site, name, props (JSON), visitor hash |
| `checks` | uptime and certificate checks: ts, site, kind, ok, latency, detail |
| `daily_rollups` | per site and day: views, visitors, errors, p95 latency, uptime % |
| `dashboards` | per site: validated AI dashboard spec (JSON) and version |
| `users`, `passkeys`, `sessions`, `api_tokens` | single admin user in v1; tokens stored hashed |
| `settings`, `providers`, `audit_log` | runtime settings, non-secret provider config, record of admin actions |

Raw `requests` and `events` rows are kept 30 days; rollups are kept forever. Raw IP addresses are never stored: the visitor hash is a hash of IP, user agent and site, salted with a key that rotates daily.

## Agent loop, tools and skills

The agent is a tool-calling loop written in Rust: **tools** are actions Rust code performs with hard guardrails, and **skills** are instructions the model loads only when a task needs them. The model never touches the filesystem, shell or network directly.

### The loop

1. Build the system prompt: role, house rules, the site's file list and recent releases, the **skills index** (each skill's name and description only), and the tool schemas.
2. Call the provider with streaming. Forward text and tool-call events to the run's event stream (SSE to the UI, stdout for the CLI) and persist them in `agent_events`.
3. Run each tool call, append the result, and loop until the model ends its turn, the user cancels, or a limit is hit.
4. Limits (configurable): 40 turns per run, a per-run token cap, a per-day spend cap across all runs. Hitting one ends the run cleanly with a clear message.
5. One run at a time per site (a per-site lock); runs on different sites may run in parallel (default max 2).

### Tools

| Tool | What it does | Guardrail |
| --- | --- | --- |
| `list_sites`, `site_info` | List sites; show hostnames, release history, status | Read-only |
| `create_site(name, title)` | Validate the name, reserve the hostname, init the git repo | Name must match `^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$`; reserved names refused (`admin`, `www`, `api`, `mail`, plus the admin host) |
| `list_files`, `read_file` | Browse the site's working tree | Paths confined to the repo |
| `write_file(path, content)`, `delete_file(path)` | Change the working tree | Canonicalized paths; no `..`, absolute paths, symlinks or `.git`; allowed extensions only; 1 MB per file, 20 MB per site |
| `validate_site` | Check that `index.html` exists, HTML parses, internal links and assets resolve, and page weight is within budget | Returns a list of problems for the agent to fix |
| `publish_site(message)` | Validate, commit, create the release, switch to it, run a health check; on failure, roll back automatically | Publishing a site that fails validation is refused |
| `rollback_site(release?)` | Switch to the previous or a named release | Safe, no confirmation needed |
| `delete_site` | Returns `needs_confirmation`; the site is deleted only after the user confirms in the UI or CLI by typing the site name | Soft delete, recoverable for 7 days |
| `check_domain(hostname)` | Resolve DNS, then fetch `http://<hostname>/.well-known/sitewright/<token>` to prove requests reach this server | Read-only; works behind Cloudflare |
| `metrics_schema`, `run_readonly_query(sql)` | Describe and query the metric views | Read-only connection, SQLite authorizer allowing SELECT on metric views only, 5 s timeout, 1,000 rows |
| `save_dashboard(spec)` | Store a dashboard spec | Validated against the JSON schema and by running every query |
| `export_site(format)` | Build a folder archive or a Docker build context | Deterministic: no model judgment involved |
| `load_skill(name)`, `read_skill_file(name, path)` | Load a skill's full instructions or its bundled files | Read-only |

The agent has no shell tool and no web-fetch tool in v1.

### Skills

Use the [Agent Skills](https://agentskills.io) format: a folder with `SKILL.md` (YAML frontmatter with `name` and `description`, then Markdown instructions), plus optional `references/` and `templates/`. Built-in skills live in the repo's `skills/` and are embedded in the binary; user skills go in `<data>/skills/` and override a built-in of the same name. Validate every built-in skill in CI with the spec's reference validator (`skills-ref validate`).

Built-in skills to write:

- **create-site**: plan the page structure, then build a fast, accessible, mobile-first site with no external CDNs by default. Include the analytics snippet, validate, then publish.
- **edit-site**: read the relevant files first, make the smallest change that meets the request, keep the existing style, validate, publish with a clear commit message.
- **delete-site**: explain what happens (soft delete, 7-day recovery, DNS left in place), then request confirmation.
- **setup-domain**: explain the DNS modes, use `check_domain`, and give the exact record to add for the user's situation.
- **design-dashboard**: inspect the site's files and the metrics schema, choose metrics that fit what the site actually does, then write SQL plus Vega-Lite panels and save them.
- **export-site**: explain the two formats and call `export_site`.

## LLM providers

Three adapters behind one `LlmProvider` trait cover nearly every major model; an API key alone is not enough, because each provider also needs its endpoint, model name, auth style and request format.

The trait exposes one streaming chat call that takes normalized messages and tool definitions and yields normalized events (text delta, tool call, usage, stop reason). Each adapter translates to and from its wire format.

| Adapter | Covers | Auth | Notes |
| --- | --- | --- | --- |
| `openai_compat` (Chat Completions) | OpenAI, **Azure OpenAI** (v1 endpoint), OpenRouter, Groq, DeepSeek, Ollama, vLLM, LM Studio | Bearer token, or `api-key` header for Azure (configurable) | Azure: base URL `https://<resource>.openai.azure.com/openai/v1/`, model = the deployment name. Implement and test this adapter first. |
| `anthropic` (Messages API) | Claude models | `x-api-key` plus `anthropic-version` header | Tool use via content blocks |
| `gemini` (generateContent) | Gemini models | API key | Function calling via `functionDeclarations` |

Example config:

```toml
[llm]
default = "azure"

[[llm.providers]]
id = "azure"
kind = "openai_compat"
base_url = "https://RESOURCE.openai.azure.com/openai/v1/"
model = "DEPLOYMENT_NAME"
auth = "api_key_header"   # or "bearer"
key_ref = "azure"         # looked up in secrets.toml

[[llm.providers]]
id = "claude"
kind = "anthropic"
model = "MODEL_ID"
key_ref = "anthropic"
```

- **Capabilities:** the agent needs reliable tool calling and streaming. Mark small local models as best effort in the docs; they often fail to build a whole site.
- **Resilience:** timeouts, retries with exponential backoff and jitter on 429 and 5xx (honor `Retry-After`), and clear surfaced errors for auth or quota failures.
- **Cost:** record token usage per run. Prices come from a user-editable table in config; ship no hard-coded prices.
- **Tests:** `wiremock` fixtures per adapter covering plain text, a tool-call round trip, streaming chunk boundaries and error codes. Live smoke tests run only when env vars such as `SW_TEST_AZURE_KEY` are set.
- **UI:** a "Test connection" button per provider that sends a tiny tool-call request and reports success, latency and errors.

## Domains, DNS and TLS

The user adds DNS records by hand at any DNS host, the tool verifies them, and TLS is either handled upstream (proxy mode) or by built-in Let's Encrypt (standalone mode). Certificates are only ever requested for hostnames that belong to a site.

### Hostnames

- Config lists one or more **zones** (for example `machandler.com`). A site's primary hostname is `<name>.<default zone>`; extra hostnames can be attached per site.
- The admin UI has its own configured hostname (default `admin.<zone>`). Its name is reserved.
- Requests for any hostname that isn't a site or the admin host get a plain 404 page and no certificate.

### DNS modes (documented in `docs/dns-and-tls.md`, explained by the setup-domain skill)

| Mode | Record the user adds | Trade-off |
| --- | --- | --- |
| Wildcard on the zone | `*.example.com A <server IP>` once | Zero per-site DNS work |
| Wildcard one level down | `*.sites.example.com A <server IP>` once | Leaves the main namespace untouched |
| Per-site records | `hikes.example.com A <server IP>` for each site | No wildcard at all |

The docs must explain two DNS rules. A specific record always wins over the wildcard at that name, so other subdomains can point elsewhere. And any record at a name (even a lone TXT or MX) stops the wildcard from answering there.

### Proxy mode (build first)

- The daemon listens on `127.0.0.1:<port>` over HTTP. An upstream proxy terminates TLS and forwards the `Host` header.
- A `trusted_proxies` setting controls whether `X-Forwarded-For`, `X-Forwarded-Proto` and `CF-Connecting-IP` are believed.
- Ship tested example configs for nginx and Caddy in `deploy/`.

### Standalone mode

- The daemon binds ports 80 and 443 itself (via `CAP_NET_BIND_SERVICE` in the systemd unit, never root). HTTP redirects to HTTPS except for ACME challenges.
- Certificates come from ACME HTTP-01 through `instant-acme`, issued when a site or hostname is created. Never issue on demand for an unknown name in the TLS handshake.
- Renew when a third of the certificate's lifetime remains, checked every 12 hours. Use ACME Renewal Information (ARI) if the crate supports it. Don't assume 90-day certificates.
- The directory URL is configurable: Let's Encrypt staging for dev and tests, production by default. In tests, use Pebble.
- Listen ports are configurable separately, so dev can serve HTTPS on 18443 while nginx forwards port-80 challenges.

### Cloudflare notes for the docs

- Proxied (orange cloud) records on Cloudflare's free certificate only cover first-level subdomains (`x.example.com`), not `x.sub.example.com`. Wildcards one level down should be DNS-only, or need a paid certificate.
- Behind Cloudflare's proxy, use proxy mode. Standalone mode expects DNS-only records so Let's Encrypt can reach the server directly.

## Metrics, uptime and AI-designed dashboards

All numbers come from deterministic code; the AI only decides which charts fit each site, by writing queries and chart specs that the server validates and runs.

### Collection

- **Request log:** the daemon logs every site request itself (both modes), buffered and written in batches. No raw IPs (see the storage section). Classify obvious bots by user agent.
- **Event beacon:** each site host serves `/_sw/a.js` (under 1 KB, no cookies) and accepts `POST /_sw/e` (JSON, 2 KB max, rate-limited per visitor). The create-site skill adds the snippet and, where the site has meaningful interactions (search, filters, form submits), custom events.
- **Uptime:** every 60 seconds, fetch each site's public URL through real DNS and TLS. Record status, latency and certificate days remaining. Site status is up, degraded (slow, or certificate under 14 days) or down (two failed checks in a row).
- **Rollups:** a nightly job fills `daily_rollups` and prunes raw rows older than 30 days.

### Dashboards

Every site gets a fixed **default dashboard**: views per day, unique visitors, top pages, status codes, p95 latency and uptime. On top of that, the design-dashboard skill writes an **AI dashboard** when a site is created, after significant edits, and when the user taps Regenerate.

Spec format (JSON schema in `docs/metrics.md`):

```json
{
  "version": 1,
  "panels": [
    {
      "id": "trail-searches",
      "title": "Trail searches per day",
      "description": "How often visitors search the trail list",
      "sql": "SELECT date(ts) AS day, count(*) AS n FROM v_events WHERE site_id = :site_id AND name = 'search' AND ts BETWEEN :from AND :to GROUP BY day",
      "vega_lite": { "mark": "line", "encoding": { "x": {"field": "day", "type": "temporal"}, "y": {"field": "n", "type": "quantitative"} } }
    }
  ]
}
```

Validation before saving: at most 12 panels; every query runs against read-only views with only the `:site_id`, `:from` and `:to` parameters bound by the server; the Vega-Lite spec may not contain a `data` URL (the server injects the rows); unknown fields are rejected. The UI renders panels with vega-embed and shows each panel's SQL on demand.

The home screen shows a status grid of all sites: status dot, 30-day uptime, 7-day views sparkline and last deploy.

## Web UI (PWA) and authentication

The UI is mobile-first, installable as a PWA, and protected by passkeys only; the frontend stack is the agent's choice, recorded in an ADR.

### Screens

| Screen | Contents |
| --- | --- |
| Sign in | Passkey button; nothing else |
| Home | Status grid of all sites; "New site" button; recent runs |
| New site | Prompt box, suggested name (editable), provider picker, Create |
| Site | Open-site link, "Edit with a prompt", live run log, release history with change summary and Rollback, default and AI dashboards, Export, Delete (type the name to confirm) |
| Run | Streamed agent output and tool calls (SSE), tokens, cost, Cancel |
| Settings | Providers (add, edit, test), budgets, zones and DNS check, TLS status, updates, passkeys, API tokens |

Design: clean, quiet, readable on a phone in one hand; system font stack; automatic dark mode; no external CDNs (vendor or bundle all frontend assets). The PWA needs a manifest, icons and a minimal service worker that caches the app shell only.

### Authentication

- Passkeys via `webauthn-rs`. The relying party ID is the admin hostname.
- **First run:** `sitewright admin invite` prints a one-time enrollment URL valid for 15 minutes. Running it again later is the recovery path: shell access to the server is the root of trust.
- Sessions: a `__Host-` prefixed cookie (HttpOnly, Secure, SameSite=Strict, host-only so site subdomains can never read it), 30-day expiry, CSRF token on every state-changing request.
- Login attempts rate-limited per IP. All admin actions written to `audit_log`.
- API tokens (hashed at rest) for scripting the HTTP API.
- Local dev works on `http://localhost`, which browsers treat as secure for passkeys.

### CLI

`sitewright serve | init | doctor | site list|create|import|rollback|delete | prompt <site> "..." | admin invite | update [--rollback] | export <site> --format folder|context`. Admin commands talk to the running daemon over a Unix socket in the data directory (owner-only permissions), so the CLI never edits the DB behind the daemon's back.

## Releases, self-update and export

Releases are signed GitHub releases built by `cargo-dist`; the binary updates itself only after verifying the signature, and keeps the previous version for rollback.

### Releases

- Tag `vX.Y.Z` on `main` triggers `release.yml` (generated by `cargo-dist`, then extended): musl builds for x86\_64 and aarch64, SHA-256 checksums, a shell installer, and release notes from the changelog.
- A CI step signs each artifact with `minisign`, using a key stored in GitHub Actions secrets. The public key is compiled into the binary and printed in the README.

### Self-update

- `sitewright update` checks the GitHub Releases API, compares semver, downloads the artifact for this platform, verifies the signature and checksum, keeps the current binary as `sitewright.prev`, swaps in the new one with `self-replace`, then exits so systemd restarts it (`Restart=always`).
- On startup, the new version backs up the DB to `<data>/backups/` before running migrations.
- `sitewright update --rollback` restores `sitewright.prev` and the pre-migration DB backup.
- Automatic updates are opt-in (`updates.auto = false` by default): a daily check that installs patch and minor releases only. The UI shows when an update is available.
- Built-in skills update with the binary; user skills are never touched.

### Export

- `folder`: a `.tar.gz` of the current release, ready for any static host.
- `context`: a `.tar.gz` holding the site files, a `Dockerfile` and a README with `docker build` / `docker run` commands. The Dockerfile uses a tiny static file server image pinned by **digest**, not tag. Look up the current digest while implementing, and document how to bump it.
- Neither format needs Docker on the server. If Docker exists in WSL, an optional test builds and runs the context.

### Install

`install.sh` (from `cargo-dist`, plus a post-install step): creates the `sitewright` system user and data directory, installs the hardened systemd unit, then points the user to `sitewright init`, which writes a starter config (zones, admin host, mode) through a few prompts.

## Security model

The admin UI is on the public internet and drives an AI agent, so the design assumes the model can be wrong or manipulated, and limits what it can do rather than trusting it.

| Threat | Mitigation |
| --- | --- |
| Someone else reaches the admin UI | Passkeys only, rate-limited login, host-only `__Host-` cookie, CSRF tokens, audit log |
| Agent damages the server | No shell or network tools; file tools confined to one site's repo with path canonicalization, extension allowlist and size limits; the daemon runs as an unprivileged user under a hardened systemd unit (`ProtectSystem=strict`, `ReadWritePaths=<data>`, `NoNewPrivileges`, `PrivateTmp`, `CAP_NET_BIND_SERVICE` only) |
| Agent destroys work | Every publish is a git commit plus an immutable release; rollback is instant; delete needs human confirmation and is recoverable for 7 days |
| Agent-written SQL | Separate read-only connection, SQLite authorizer limited to metric views, timeout and row limit |
| A generated site attacks the admin UI | Sites are served from different hostnames than the admin UI; the admin cookie is host-only; sites get `X-Content-Type-Options`, `Referrer-Policy` and `frame-ancestors 'self'` headers |
| Runaway LLM cost | Per-run token cap, per-day spend cap, max concurrent runs |
| Certificate rate-limit abuse via random subdomains | Certificates only for hostnames that belong to sites; never issued on demand |
| Leaked secrets | `secrets.toml` mode 0600, redacted in logs and UI, never sent back to the browser after saving |
| Tampered updates | Signed releases verified before install; checksums |
| Vulnerable dependencies | `cargo-deny` advisories and licenses in CI; Dependabot for Cargo and GitHub Actions |
| Visitor privacy | No raw IPs, no cookies in the beacon, documented retention |

Write `SECURITY.md` (how to report issues) and `docs/security.md` (this threat model, kept current).

## Dev environment and VPS testing

Build and test locally in WSL, then deploy a static binary to the VPS as the restricted `devtest` user, on a dev hostname space that never touches the production sites.

### WSL setup (script it as `scripts/dev-setup.sh`)

- rustup stable with the musl targets, `cargo-zigbuild`, `cargo-nextest`, `cargo-deny`, `cargo-dist`, `sqlx-cli`, `minisign`.
- Node LTS for `npx skills-ref validate`, and for the frontend build if the agent picks a framework.
- Pebble and `pebble-challtestsrv` release binaries for ACME tests.
- Local loop: `cargo run -- serve --config dev/local.toml`. Sites resolve at `http://<name>.localhost:8080` (browsers map `*.localhost` to loopback; use `curl --resolve` in tests). The admin UI at `http://localhost:8080` supports passkeys.
- A git-ignored `.env` holds LLM keys for live tests. Dev config sets a $5 per day LLM spend cap.

### Dev hostnames

| Hostname | DNS (Cloudflare) | Path to the daemon |
| --- | --- | --- |
| `*.dev.machandler.com` (sites) | A record to the VPS, **DNS only** (grey cloud) | nginx port 80 → `127.0.0.1:18080` (proxy mode, plain HTTP) |
| `sitewright-dev.machandler.com` (admin UI) | A record to the VPS, **proxied** (orange cloud) | Cloudflare TLS → nginx, using the same TLS setup as the existing sites → `127.0.0.1:18080` |
| `*.dev.machandler.com:18443` (standalone tests) | same as sites | Daemon serves HTTPS itself on public port 18443; nginx still forwards port-80 ACME challenges to it |

The sites wildcard is DNS-only because Cloudflare's free certificate doesn't cover second-level subdomains, and because standalone tests need Let's Encrypt to reach the server directly. The admin host is first-level, so it gets real HTTPS and passkeys work from Matthew's phone.

### One-time VPS bootstrap (Matthew runs it once with sudo)

Write `scripts/vps-bootstrap.sh`, idempotent, with a `--dry-run` flag that prints every change. Usage: `sudo ./vps-bootstrap.sh --like /etc/nginx/sites-enabled/<an existing site> --pubkey "<WSL public key>"`. It must:

1. Create user `devtest` (no sudo), install the SSH public key, and run `loginctl enable-linger devtest` so its systemd user services run without a login.
2. Add an nginx config file containing two server blocks: `*.dev.machandler.com` on port 80, and `sitewright-dev.machandler.com`, copying the `listen` and `ssl_*` lines from the `--like` file. Both proxy to `127.0.0.1:18080` with `Host`, `X-Forwarded-For` and `X-Forwarded-Proto` set, and SSE-friendly settings (`proxy_buffering off`, long read timeout). Run `nginx -t` before `systemctl reload nginx`, and touch no other nginx file.
3. Open TCP 18443 in the firewall if `ufw` is active.
4. Print a summary, including the DNS records Matthew must add.

### Deploy loop (the agent runs it)

- An SSH alias `racknerd-dev` in `~/.ssh/config` points to `devtest@<VPS>`.
- `scripts/deploy-dev.sh`: check the remote architecture (`uname -m`), build with `cargo zigbuild --release --target <arch>-unknown-linux-musl`, copy to `~/bin/sitewright.new`, swap the binary atomically, then `systemctl --user restart sitewright`.
- `scripts/smoke-dev.sh`: curl checks for a known site, a 404 host, the admin login page, and (after M6) a staging certificate on port 18443.
- In dev, always use the Let's Encrypt **staging** directory. Use production certificates only for at most 5 hostnames in M6 final acceptance.
- Add `.claude/settings.json` to the repo, allowing `ssh racknerd-dev`, `scp` to `racknerd-dev:` and the repo scripts without prompts.

### Testing strategy

- Unit tests for parsers, path confinement, name validation, spec validation, and each provider adapter's translation.
- Integration tests that boot the daemon in-process on temp directories, with a scripted mock LLM provider driving full create, edit, publish and rollback runs.
- ACME integration against Pebble, locally and in CI.
- `insta` snapshots for rendered HTML pages and agent prompts.
- `docs/testing.md` holds a short manual checklist for phone flows (install PWA, passkey sign-in, create, edit, rollback, delete) that Matthew runs at M3 and M6.

## Repo layout, CI and documentation

The GitHub repo is a Cargo workspace whose docs are complete enough that a stranger can install, operate and contribute without asking anything.

```text
sitewright/
  Cargo.toml              # workspace
  crates/
    sitewright/           # binary: CLI + daemon wiring
    sw-core/              # config, domain types, errors, DB, migrations
    sw-sites/             # site store, git, releases, validation, export
    sw-agent/             # agent loop, tools, skills loader
    sw-llm/               # LlmProvider trait + three adapters
    sw-web/               # axum routers: site serving, admin UI, API, SSE, auth
    sw-tls/               # ACME, certificate store and resolver
    sw-metrics/           # request log, beacon, uptime, rollups, dashboards
    sw-update/            # release check, signature verify, self-replace
  skills/                 # built-in skills (embedded at build time)
  web/templates/, web/static/   # askama templates; htmx, vega-embed, CSS, icons
  migrations/
  scripts/                # dev-setup, vps-bootstrap, deploy-dev, smoke-dev
  deploy/                 # systemd unit, nginx and Caddy examples
  docs/
  .github/workflows/      # ci.yml, release.yml; dependabot.yml
  README.md  CHANGELOG.md  CONTRIBUTING.md  SECURITY.md  AGENTS.md
  LICENSE-MIT  LICENSE-APACHE
```

**CI (`ci.yml`, on every push and PR):** `cargo fmt --check`, `clippy --all-targets -D warnings`, `cargo nextest run`, `cargo deny check`, `skills-ref validate skills/*`, a frontend build and lint (if a framework is chosen), a musl release build, and Pebble ACME integration tests.

**Docs to write and keep current:**

| File | Contents |
| --- | --- |
| `README.md` | One-paragraph pitch, screenshots or a GIF of the phone flow, quickstart, feature list, honest v1 limits, signing public key |
| `docs/architecture.md` | Components, request and agent flows, with Mermaid diagrams |
| `docs/install.md`, `docs/configuration.md` | Install paths and every config key with defaults |
| `docs/dns-and-tls.md` | DNS modes, registrar-agnostic steps, wildcard override rules, Cloudflare notes, proxy vs standalone |
| `docs/providers.md` | Setup for each provider, including Azure OpenAI |
| `docs/skills.md` | What skills are, the built-ins, how to write and install your own |
| `docs/metrics.md` | What is collected, privacy, retention, dashboard spec schema |
| `docs/security.md` | Threat model |
| `docs/operations.md` | Backup and restore, moving to a new server, update and rollback, troubleshooting, `sitewright doctor` |
| `docs/development.md`, `docs/testing.md` | WSL setup, the VPS loop, the test strategy, the manual checklist |
| `docs/adr/NNNN-*.md` | One page per significant decision |
| `docs/STATUS.md` | Live progress log and "Needs from Matthew" |
| `AGENTS.md` | Instructions for AI coding agents working on the repo (build, test, conventions) |

## Milestones and acceptance criteria

Seven milestones in order, M0 to M6, each ending in a merged PR and a demo-able result; v1.0.0 is tagged at the end of M6. Don't start a milestone until the previous one meets every "done when" item, except items waiting on Matthew, which go in `docs/STATUS.md`.

1. **M0: Scaffold**
   - Builds: workspace and crates, CI, licenses, README stub, docs skeleton, ADR-0001 (stack), `docs/STATUS.md`, `AGENTS.md`, `.claude/settings.json`, `dev-setup.sh`, `vps-bootstrap.sh`, `deploy-dev.sh`, and a systemd user unit for dev.
   - Done when: CI is green on GitHub; after Matthew runs the bootstrap, `deploy-dev.sh` deploys a binary that answers `hello` at `http://anything.dev.machandler.com` and on `sitewright-dev.machandler.com`.
2. **M1: Serve sites (proxy mode)**
   - Builds: config, data directory, SQLite and migrations, site store with git and releases, hostname routing, static serving (index, `404.html`, caching headers, compression), the request log, CLI `site create/import/list/rollback/delete`, the Unix admin socket.
   - Done when: `sitewright site import hikes ./example` makes `http://hikes.dev.machandler.com` live on the VPS; a second import plus rollback switches content instantly; unknown hosts return 404; integration tests cover all of it.
3. **M2: Agent with tools, one provider**
   - Builds: the `LlmProvider` trait and the `openai_compat` adapter (Azure first), the agent loop, every tool in the tools table, validation, publish with automatic rollback, the delete confirmation flow, budgets, run persistence, CLI `prompt`.
   - Done when: `sitewright prompt --new hikes "a hiking log with my favorite Washington trails"` builds and publishes a working site on the VPS using Azure OpenAI; a follow-up edit prompt changes it; mock-provider tests cover the loop, tool errors and limits; cost appears in the run record.
4. **M3: Web UI and passkeys**
   - Builds: the UI (stack per ADR), all screens, SSE run streaming, passkey enrollment and login, sessions and CSRF, PWA manifest and service worker, provider settings.
   - Done when: from Android Chrome, Matthew installs the PWA from `https://sitewright-dev.machandler.com`, signs in with a passkey, creates a site from a prompt, watches it build, edits it, rolls it back and deletes it (manual checklist in `docs/testing.md`).
5. **M4: Skills and more providers**
   - Builds: the skills loader (index plus on-demand loading), the six built-in skills, `skills-ref` in CI, the `anthropic` and `gemini` adapters, "Test connection".
   - Done when: the same create prompt succeeds with each provider whose key is present (fixture tests cover all three either way); a user skill dropped into `<data>/skills/` appears in the index and is used.
6. **M5: Metrics and dashboards**
   - Builds: the beacon and its snippet, the uptime checker, rollups and pruning, the default dashboard, the design-dashboard skill, spec validation, vega-embed rendering, the home status grid.
   - Done when: phone visits to a dev site show up in its default dashboard within a minute; the AI dashboard has at least one panel specific to that site; a spec with forbidden SQL or a data URL is rejected; a stopped site shows as down within 2 minutes.
7. **M6: Standalone TLS, updates, export, v1.0.0**
   - Builds: ACME HTTP-01 with `instant-acme`, the certificate store and resolver, renewal, the HTTPS redirect, `release.yml` with minisign signing, `update` and `--rollback`, `export_site` in both formats, `install.sh`, `sitewright init` and `doctor`, completed docs.
   - Done when: Pebble tests pass in CI; on the VPS, standalone mode gets staging certificates for 3 site hosts served on port 18443; a v0.9.0 to v1.0.0 self-update verifies the signature and survives a restart; `--rollback` works; an exported context builds and runs (if Docker is present in WSL); `v1.0.0` is tagged with release notes.

## User inputs and decision policy

Matthew provides six one-time inputs; everything else is the agent's decision.

| # | Input from Matthew | Needed by |
| --- | --- | --- |
| 1 | Done: `gh auth login` in WSL. The agent creates the GitHub repo itself (see *Repository setup*). | M0 |
| 2 | Review and run `vps-bootstrap.sh` once with sudo for the nginx blocks and firewall (`--like` an existing site's nginx file). Already done by hand: user `devtest` (no sudo, linger enabled), its SSH key, and the `racknerd-dev` alias in WSL, so the agent has VPS access from the start. | M0 |
| 3 | Done: Cloudflare DNS `*.dev.machandler.com` A record to the VPS (DNS only) and `sitewright-dev.machandler.com` A record to the VPS (proxied). | M0 |
| 4 | LLM credentials in `~/projects/sitewright/.env` as `AZURE_OPENAI_BASE_URL`, `AZURE_OPENAI_API_KEY` and `AZURE_OPENAI_DEPLOYMENT` (a gpt-5-mini deployment with 500K TPM), copied by the agent into the VPS dev `secrets.toml` at M2. Anthropic and Gemini keys are optional. | M2, M4 |
| 5 | Enroll a passkey on his phone and run the manual checklist | M3, M6 |
| 6 | Store the minisign secret key and password (the agent generates them) as GitHub Actions secrets, and keep an offline backup | M6 |

**The agent decides on its own:** the final project name (check crates.io and GitHub for conflicts first; keep "Sitewright" if it's free), crate APIs, internal structure, UI layout and styling, skill wording, test design, defaults and anything else this plan leaves open. Record significant choices as ADRs.

**Stop and write to "Needs from Matthew" instead of acting when:** an action needs `sudo`; it could affect a production site or the production nginx config; it would spend more than the dev LLM budget; or it involves a secret Matthew hasn't provided. Then continue with work that isn't blocked.
