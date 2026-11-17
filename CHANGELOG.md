# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.8.0] - 2026-11-16

### Added

- **GitHub Copilot as a provider (experimental).** A new provider kind, `copilot`, uses
  the models of a GitHub Copilot subscription through GitHub's Copilot SDK and runtime.
  `sitewright copilot install` downloads and verifies the runtime;
  `sitewright copilot check` signs in, lists the models, makes a test call and reports
  the runtime's memory use. Not yet run against the real runtime; see ADR-0008 and
  `docs/providers.md`.

- **Sign in to Copilot with your GitHub account** instead of creating a token: a
  short code to enter at github.com, from Settings ("Sign in with GitHub") or
  `sitewright copilot login`. `sitewright copilot logout` forgets it. A Copilot
  provider without a key uses this sign-in.

### Changed

- JSON shown to the agent keeps the order it was written in instead of being sorted
  by key (a side effect of the Copilot SDK).

## [1.7.0] - 2026-11-16

### Added

- **Containerized sites.** A site can be marked as containerized when it is created
  (a checkbox on the new-site form) or later (the site's page, or
  `sitewright site containerize`). Its Container page offers an image that runs without
  Sitewright, a setup guide generated for that site, and a copy of its data;
  `sitewright site image` downloads all three. See `docs/containers.md` and ADR-0007.
- `sitewright run-site`: serve one packaged site with no daemon, with secrets and AI
  model keys from the environment. `sitewright import-data` places a downloaded
  database into a container volume.
- `sw-image`: writes `docker load`-compatible image archives without Docker.
- CI loads and runs a site image with Docker (`scripts/container-smoke.sh`).

### Changed

- Site databases keep SQLite's temporary data in memory.

## [1.6.0] - 2026-11-16

### Added

- AI models for backends: `llm.chat` goes through the daemon with its credentials. Per
  site: off by default, a list of allowed models, per-minute and per-month limits, and
  recorded usage with cost. Managed on the site's page, over the API, or with
  `sitewright backend ai`.
- Agent tool `list_models`, so "use the new model" in a prompt can be resolved by name.
- Dashboard panels over a site's own data: a panel with `"source": "site"` runs one
  read-only `SELECT` on the site's backend database (entries per day, sign-ups), with
  `:from` and `:to` as Unix milliseconds. The agent's `run_readonly_query` takes the same
  `source`, and `metrics_schema` describes it.

### Changed

- Time a backend handler spends waiting for the daemon (queries, outside requests, model
  calls) no longer counts against its 10-second limit; a request may take 90 seconds in
  all.

## [1.5.0] - 2026-11-16


### Added

- Backend secrets: the owner stores API keys per site (portal, API, or
  `sitewright backend set-secret`); a backend reads them by name with `secrets.get`. The
  agent sees names only, and secret values are redacted from everything it is shown.
- Outbound requests from backends: `fetch` to HTTPS hosts the owner approved for the
  site. The daemon makes the request, to public addresses only, without following
  redirects. The agent can request a host with `request_backend_host`; only the owner can
  approve it.
- Agent tool `backend_access` lists a site's secret names and hosts.
- Visitor accounts for backends: `auth.signUp`, `auth.signIn`, `auth.user`,
  `auth.requireUser`, `auth.signOut`. Passwords (Argon2id) and sessions are managed by
  Sitewright in two protected tables of the site's database.
- `test_backend` takes a `cookie`, to test as a signed-in visitor.

### Changed

- Site migrations can no longer create triggers.

## [1.4.0] - 2026-11-16

### Added

- **Site backends.** A site can have server-side JavaScript under `backend/` that answers
  its `/api/` requests, with its own SQLite database and migrations. The code runs in a
  worker process per site that is jailed with seccomp and can reach nothing but its own
  site's database. See `docs/backends.md` and ADR-0006.
- The agent has a `build-backend` skill and three tools: `test_backend` (try unpublished
  code against a private copy of the data), `backend_logs` and `backend_schema`.
- Publishing a backend checks that the code loads and applies its migrations; the
  database is restored if the release is rolled back by a failed health check.
- The site page shows a Backend section with the database size and console output.
- `sitewright doctor` checks that a sandboxed worker can start.
- `[backends]` configuration: limits, and `enabled = false` to turn the feature off.

### Changed

- Exports contain the static files only, without `backend/`.
- A NULL in a typed column of a dashboard query is now reported as null, not as 0.

## [1.2.0] - 2026-11-16

### Added

- Rename a site from its page in the portal (title, name and address), with
  `sitewright site rename <old> <new> [--no-redirect]` and `sitewright site title`, or
  over the API (`/v1/sites/<name>/rename`, `/v1/sites/<name>/title`). The old address
  redirects to the new one unless you turn that off.
- The run page shows when a run started and how long it has been running, and has a
  "Back to site" link while the run continues in the background. The site page links to
  the run in progress.
- Agent messages are rendered as Markdown. Raw HTML in a message is shown as text.
- Ctrl+Enter (Cmd+Enter on a Mac) submits a prompt.

### Changed

- The agent is told to write Markdown and to finish with a summary, never a question.

## [1.1.0] - 2026-11-16

### Added

- `sitewright site hostname add|remove`, with `--primary` to change a site's main address;
  the same over the API (`/v1/sites/<name>/hostnames`).
- `domains.reserved_names`: site names that may not be used, for zones shared with other
  services.
- `scripts/vps-production.sh`: adds a wildcard nginx block for Sitewright beside existing
  sites without modifying them.
- Documentation for moving to another domain.

### Fixed

- Rare "database is locked" errors when two things wrote at the same moment (for example
  a publish while the request log was flushing). Write transactions now take SQLite's
  write lock at their start, where the busy timeout applies.

## [1.0.0] - 2026-11-16

The first stable release. It contains everything in 0.9.0, which was published as a
release candidate, and was reached from 0.9.0 with `sitewright update` on a live server.

### Changed

- Release signatures made by the `minisign` command-line tool are covered by a test, as is
  the agreement between the public key compiled into the binary and the one printed in
  the README and the installer.

### Notes for people installing for the first time

- Start with [docs/install.md](docs/install.md).
- The honest limits of v1 are listed in the [README](README.md#honest-limits-of-v1):
  static sites only, one admin, Linux hosts, manual DNS.

## [0.9.0] - 2026-11-15

The first public release: everything planned for v1, published as a release candidate so
that the update path to 1.0.0 can be exercised on a real server.

### Added

- **Serving.** Static sites, one per hostname, each versioned in git with immutable
  releases, instant rollback, soft delete with a 7-day recovery window, and import of
  existing folders. Host-based routing, clean URLs, a per-site `404.html`, caching and
  security headers, compression.
- **Proxy mode and standalone mode.** Run behind nginx, Caddy or Cloudflare, or let the
  daemon serve HTTPS itself with Let's Encrypt certificates (HTTP-01), requested only for
  hostnames that belong to a site, renewed at a third of their lifetime or when the
  certificate authority's renewal information says so.
- **The agent.** A tool-calling loop with eighteen sandboxed tools (no shell, no network),
  site validation before every publish, automatic rollback of a release that cannot be
  served, turn, token and daily-spend limits, cancellation, and replayable run histories
  with cost tracking. Deleting a site always needs a person to type its name.
- **LLM providers.** Adapters for OpenAI-compatible APIs (OpenAI, Azure OpenAI,
  OpenRouter, Groq, Ollama and others), Anthropic and Gemini, with retries, backoff and a
  "Test connection" check.
- **Skills.** Agent Skills loaded on demand: `create-site`, `edit-site`, `delete-site`,
  `setup-domain`, `design-dashboard`, `export-site`, plus your own in the data directory.
- **The admin app.** A mobile-first, installable PWA: passkey sign-in, live run progress,
  release history with one-tap rollback, dashboards, provider settings, API tokens.
- **Metrics.** A request log without IP addresses, a cookie-free event beacon, uptime
  checks with site status, daily rollups with 30-day retention of raw data, a standard
  dashboard and an AI-designed dashboard per site whose SQL runs in a read-only sandbox.
- **Operations.** Signed releases, `sitewright update` with signature verification and
  `--rollback`, `sitewright init`, `sitewright doctor`, an installer with a hardened
  systemd unit, and export of any site as plain files or a Docker build context.
- A CLI for every core operation over an owner-only Unix socket.

[Unreleased]: https://github.com/matthew-chandler/sitewright/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/matthew-chandler/sitewright/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/matthew-chandler/sitewright/compare/v0.9.0...v1.0.0
[0.9.0]: https://github.com/matthew-chandler/sitewright/releases/tag/v0.9.0
