# Status

Live progress log for the v1 build described in [PLAN.md](../PLAN.md). A fresh session can
resume from this file.

_Last updated: 2026-11-15 (M3 built and deployed; M4 next)_

## Needs from Matthew

1. **Get a certificate for the admin hostname (needs sudo).** The nginx bootstrap copied
   the `fireworks.machandler.com` Let's Encrypt certificate into the admin block, so
   Cloudflare rejects the origin (error 525) on `https://sitewright-dev.machandler.com`.
   On the VPS:

   ```sh
   sudo certbot --nginx -d sitewright-dev.machandler.com
   ```

   Nothing else needs re-running. (`scripts/vps-bootstrap.sh` now does this itself, and a
   later re-run reuses the certificate.) This blocks only the M3 phone test: passkeys need
   HTTPS.

2. **Run the phone checklist** in [testing.md](testing.md#manual-checklist-phone-flows)
   once #1 is done. On the VPS as devtest:
   `~/bin/sitewright --data-dir ~/sitewright-data admin invite` prints the enrollment link
   to open on your phone. Tell me what fails, if anything.

3. **Create `.claude/settings.json` (optional, convenience).** Claude Code's permission
   classifier refused to let the agent write its own allow-list, which is reasonable. The
   exact file contents are in [development.md](development.md#claude-code-permissions).

Done by Matthew so far: nginx bootstrap run (2026-11-15), `*.dev.machandler.com` switched
to DNS only.

## Milestones

| Milestone | State |
| --- | --- |
| M0 Scaffold | done |
| M1 Serve sites (proxy mode) | done |
| M2 Agent with tools, one provider | done |
| M3 Web UI and passkeys | done, pending the phone checklist (Needs #1, #2) |
| M4 Skills and more providers | not started |
| M5 Metrics and dashboards | not started |
| M6 Standalone TLS, updates, export, v1.0.0 | not started |

## Done

- Dev toolchain installed in WSL via `scripts/dev-setup.sh` (Rust 1.100, musl targets, zig,
  nextest, deny, dist, sqlx-cli, insta, minisign, Pebble).
- Workspace with nine crates; placeholder daemon answering `hello`; smoke test.
- `vps-bootstrap.sh` (idempotent, `--dry-run`), `deploy-dev.sh`, `smoke-dev.sh`, dev systemd
  user unit.
- CI workflow, cargo-deny, Dependabot, licenses, ADR-0001 to 0003, docs skeleton.

- Public repo https://github.com/matthew-chandler/sitewright, `main` protected (requires the
  `ci-ok` check, admins included), CI green.
- First deploy to the VPS: the daemon runs as a systemd user service and answers `hello` on
  `127.0.0.1:18080`.

- **M1:** config file and data directory; SQLite with embedded migrations and a
  pre-migration backup; site store (create, file tools with path confinement, import,
  publish with health check and automatic rollback, rollback, soft delete, restore, purge);
  host-based routing; static serving (index, clean URLs, `404.html`, caching and security
  headers, compression); batched request log with daily-rotating visitor hashes; admin Unix
  socket and the `site` CLI. 41 tests. Deployed: `http://smoke.dev.machandler.com` is live
  through nginx.

- **M2:** `LlmProvider` trait with a hand-written SSE parser, retries with backoff and
  `Retry-After`, and the `openai_compat` adapter (Azure verified live); site validation
  (links, assets, sizes, page weight) enforced by publish; the agent loop with turn, token
  and daily-spend limits, cancellation, one run per site, replayable event streams and
  cost tracking; twelve tools; the delete confirmation flow; `sitewright prompt`; the runs
  API with SSE; `check_domain`. 82 tests. On the VPS, `prompt --new hikes ...` built
  `http://hikes.dev.machandler.com` in about a minute for $0.02, and a follow-up prompt
  edited it.

- **M3:** the admin web UI (askama templates, htmx, about 300 lines of hand-written
  JavaScript, no build step): sign-in, enrollment, home grid, new site, site page (prompt,
  runs, releases with change summaries and rollback, delete with typed confirmation), live
  run view over SSE with cancel, settings (providers with Test connection, budgets, DNS
  check, passkeys, API tokens). Passkeys with `webauthn-rs`, `__Host-` session cookie,
  CSRF tokens plus an Origin check, rate-limited auth endpoints, API tokens for
  `/api/v1`, `sitewright admin invite`, PWA manifest, icons and a shell-only service
  worker. Providers can be added in the UI (keys go to `secrets.toml`). 96 tests,
  including the whole passkey flow with a software authenticator. Deployed to the VPS.

## In progress

- M4: skills loader and the six built-in skills, `anthropic` and `gemini` adapters.

## Next

- M5: metrics beacon, uptime checks, rollups, default and AI dashboards.

## Decisions worth knowing (beyond the ADRs)

- sqlx is 0.9 with compile-time-checked queries; `.sqlx/` is committed and CI verifies it
  (`scripts/sqlx-prepare.sh --check`). Run the script after touching a query or migration.
- `deny.toml` ignores RUSTSEC-2026-0459 (bundled SQLite, local-only attack) with a written
  justification, because no sqlx release accepts the fixed `libsqlite3-sys` yet.
- Tools that belong to later subsystems arrive with them: `load_skill` and
  `read_skill_file` in M4, `metrics_schema`, `run_readonly_query` and `save_dashboard` in
  M5, `export_site` in M6. Until M4 the agent's instructions are built into its system
  prompt.
- Every agent tool is bound to the run's site; the model cannot name another site for a
  write. `create_site` only works in a run started with `--new`, for that name.
- `scripts/dev-secrets.sh local|vps` turns `.env` into `secrets.toml` and an `[llm]`
  config section without printing anything. The VPS has both.
- The run view uses a plain `EventSource` and renders JSON events in `run.js`, rather
  than the htmx SSE extension mentioned in ADR-0003: the events are data, not HTML.
- `deny.toml` also ignores RUSTSEC-2023-0071 (`rsa`, used by webauthn-rs only to verify
  signatures) and RUSTSEC-2026-0374 (`basic-toml`, build-time only), each with a reason.
- Rollback without a target goes to the release published before the current one, and
  resets the working tree to match.

## Environment facts

- VPS: Ubuntu 20.04, x86_64, 1 vCPU class, ~1 GB RAM, nginx on 80/443. Ports 5000-5004 are
  used by Matthew's existing services; Sitewright dev uses 18080 (HTTP) and 18443 (TLS tests).
- `gh` in WSL is 2.4.0 (old): no `gh search`, use `gh api`.

## What to test by hand

- The phone checklist in [testing.md](testing.md) (after Needs #1).
- Until then, the UI can be tried on a desktop over an SSH tunnel:
  `ssh -L 18080:127.0.0.1:18080 racknerd-dev` shows the pages, but passkeys will not work
  there because the origin differs from the configured admin URL. Locally:
  `scripts/dev-secrets.sh local`, `cargo run -- serve --data-dir dev/data`,
  `cargo run -- --data-dir dev/data admin invite`, then open the printed
  `http://localhost:8080/enroll/...` link (browsers allow passkeys on localhost).
- `http://hikes.dev.machandler.com`: the agent-built site.
- `./scripts/smoke-dev.sh` from WSL.
