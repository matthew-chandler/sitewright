# Status

Live progress log for the v1 build described in [PLAN.md](../PLAN.md). A fresh session can
resume from this file.

_Last updated: 2026-11-15 (M1 done; M2 next)_

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

2. **Create `.claude/settings.json` (optional, convenience).** Claude Code's permission
   classifier refused to let the agent write its own allow-list, which is reasonable. The
   exact file contents are in [development.md](development.md#claude-code-permissions).

Done by Matthew so far: nginx bootstrap run (2026-11-15), `*.dev.machandler.com` switched
to DNS only.

## Milestones

| Milestone | State |
| --- | --- |
| M0 Scaffold | done |
| M1 Serve sites (proxy mode) | done |
| M2 Agent with tools, one provider | not started |
| M3 Web UI and passkeys | not started |
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

## In progress

- M2: `LlmProvider` trait, `openai_compat` adapter, agent loop and tools.

## Next

- M3: web UI and passkeys.

## Decisions worth knowing (beyond the ADRs)

- sqlx is 0.9 with compile-time-checked queries; `.sqlx/` is committed and CI verifies it
  (`scripts/sqlx-prepare.sh --check`). Run the script after touching a query or migration.
- `deny.toml` ignores RUSTSEC-2026-0459 (bundled SQLite, local-only attack) with a written
  justification, because no sqlx release accepts the fixed `libsqlite3-sys` yet.
- Rollback without a target goes to the release published before the current one, and
  resets the working tree to match.

## Environment facts

- VPS: Ubuntu 20.04, x86_64, 1 vCPU class, ~1 GB RAM, nginx on 80/443. Ports 5000-5004 are
  used by Matthew's existing services; Sitewright dev uses 18080 (HTTP) and 18443 (TLS tests).
- `gh` in WSL is 2.4.0 (old): no `gh search`, use `gh api`.

## What to test by hand

- `http://smoke.dev.machandler.com` shows the example site; any other
  `*.dev.machandler.com` name shows a plain 404.
- On the VPS as devtest: `~/bin/sitewright --data-dir ~/sitewright-data site list`.
- `./scripts/smoke-dev.sh` from WSL (the admin-over-HTTPS check fails until Needs #1).
