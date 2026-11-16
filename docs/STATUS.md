# Status

Live progress log for the v1 build described in [PLAN.md](../PLAN.md). A fresh session can
resume from this file.

_Last updated: 2026-11-15 (M6 built; v0.9.0 and v1.0.0 releases wait on Needs #1)_

## Needs from Matthew

1. **Store the release signing key as GitHub Actions secrets** (plan input #6). The key
   pair was generated on your WSL machine, outside the repository, in
   `~/.config/sitewright-release/` (`minisign.key`, `minisign.password`, `minisign.pub`).
   Claude Code's permission system did not let the agent set repository secrets, so:

   ```sh
   cd ~/projects/sitewright
   gh secret set MINISIGN_SECRET_KEY < ~/.config/sitewright-release/minisign.key
   gh secret set MINISIGN_PASSWORD   < ~/.config/sitewright-release/minisign.password
   ```

   Then **copy that directory somewhere offline** (a password manager or a USB stick):
   whoever holds `minisign.key` and its password can sign updates every installation will
   accept. The public half is already in the repository (`release/minisign.pub`).

2. **Publish the releases** (after #1). Either tell the agent to continue, or:

   ```sh
   git checkout main && git pull
   git tag v0.9.0 && git push origin v0.9.0      # release.yml builds, signs, publishes
   ```

   What remains after that is the last M6 acceptance step: install v0.9.0 on the VPS, bump
   the version to 1.0.0 (one small PR), tag `v1.0.0`, and run `sitewright update` and
   `sitewright update --rollback` on the VPS. The steps are under "Finishing v1.0.0" below.

3. **Optional: screenshots or a GIF** of the phone flow for the README. The agent has no
   browser to capture them.

4. **Optional: `.claude/settings.json`.** The exact contents are in
   [development.md](development.md#claude-code-permissions).

Done by Matthew: nginx bootstrap, `*.dev` DNS-only, certbot for the admin host, the admin
record set to DNS only, and the full phone checklist (2026-11-15).

## Milestones

| Milestone | State |
| --- | --- |
| M0 Scaffold | done |
| M1 Serve sites (proxy mode) | done |
| M2 Agent with tools, one provider | done |
| M3 Web UI and passkeys | done; phone checklist passed 2026-11-15 |
| M4 Skills and more providers | done (Anthropic and Gemini verified by fixtures only; no keys provided) |
| M5 Metrics and dashboards | done |
| M6 Standalone TLS, updates, export, v1.0.0 | built; releases wait on the signing secrets (Needs #1, #2) |

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

- **M4:** the skills loader (index in the prompt, `load_skill` and `read_skill_file` on
  demand, user skills in `<data>/skills/` picked up per run and overriding built-ins),
  four built-in skills validated by `skills-ref` in CI, and the `anthropic` and `gemini`
  adapters with fixture tests. Each adapter replays the model's own turn (thinking
  blocks, thought signatures) unchanged. 110 tests. Live on Azure the agent now loads
  `create-site`, reads its templates and builds from them.

- **M5:** the event beacon (`/_sw/a.js`, `/_sw/e`), uptime checks with up / degraded /
  down status, per-minute rollups and hourly pruning, the read-only SQL sandbox
  (authorizer, per-site temporary views, time and row limits), dashboard specs validated
  by running every query, a default dashboard, the `design-dashboard` skill with
  `metrics_schema`, `run_readonly_query` and `save_dashboard`, a dashboard page drawn with
  vendored Vega-Lite (no `eval`), and the home status grid with uptime and sparklines.
  130 tests.

- **M6:** standalone mode (`sw-tls`: ACME HTTP-01 with `instant-acme`, a certificate
  store with a per-hostname resolver, renewal at a third of the lifetime plus ARI, an HTTPS
  listener, HTTP redirect) tested against Pebble locally and in CI; `sw-update` (release
  lookup, minisign and SHA-256 verification, atomic swap, rollback including the
  pre-migration database); `sitewright update`, `init`, `doctor`, `export`; the
  `export_site` tool and `export-site` skill; `scripts/install.sh`; a hardened systemd
  unit; nginx and Caddy examples; `release.yml`; all docs. 145 tests.
  **Verified on the VPS:** in standalone mode three site hosts got Let's Encrypt staging
  certificates and are served on port 18443; an unknown hostname gets no certificate.

## In progress

- Nothing. Waiting on Needs #1 to publish v0.9.0.

## Finishing v1.0.0

Once the signing secrets exist:

1. `git tag v0.9.0 && git push origin v0.9.0`; wait for the Release workflow; check the
   release has two archives, each with `.sha256` and `.minisig`, and `install.sh`.
2. On the VPS as devtest, install that release's binary over `~/bin/sitewright`
   (download the x86_64 archive, verify, extract) and restart the user service.
3. Bump the workspace version to `1.0.0`, add a `## [1.0.0]` section to `CHANGELOG.md`,
   merge, `git tag v1.0.0 && git push origin v1.0.0`.
4. On the VPS: `~/bin/sitewright --data-dir ~/sitewright-data update` (expect "Signature
   and checksum verified", then the service restarts as 1.0.0), `... doctor`, then
   `... update --rollback` (back to 0.9.0) and `... update` again.
5. Per the plan, production Let's Encrypt certificates may be tried on at most 5 dev
   hostnames: remove `acme_directory` from `[tls]` in the VPS config, restart, and check.
6. Docker is not installed in WSL, so building an exported context was not run; the
   archive layout and the digest-pinned Dockerfile are covered by tests.

## Decisions worth knowing (beyond the ADRs)

- sqlx is 0.9 with compile-time-checked queries; `.sqlx/` is committed and CI verifies it
  (`scripts/sqlx-prepare.sh --check`). Run the script after touching a query or migration.
- `deny.toml` ignores RUSTSEC-2026-0459 (bundled SQLite, local-only attack) with a written
  justification, because no sqlx release accepts the fixed `libsqlite3-sys` yet.
- Releases are built by a hand-written `release.yml`, not cargo-dist (ADR-0005).
- The binary is installed to `/opt/sitewright/bin`, owned by the service user, so that a
  verified self-update can replace it under `ProtectSystem=strict`.
- The publish health check calls the site router in-process instead of over the network,
  so it behaves the same in proxy and standalone mode.
- The dev VPS now runs in standalone mode (HTTPS on 18443, staging certificates) while
  nginx keeps forwarding port 80; the previous config is `config.toml.bak-proxy` there.
- The AI dashboard is designed inside the create run (the `create-site` skill's last step)
  and updated by edit runs when they change what visitors can do, rather than by a
  separate automatic run: one run, one cost, and the agent already has the site in
  context. **Regenerate** on the dashboard page starts a dedicated run.
- Uptime checks go through public DNS as the plan says; `metrics.uptime_resolve_local`
  exists for local development and tests, where `*.localhost` does not resolve.
- `vega-interpreter` ships only as an ES module, so `scripts/vendor-vega.sh` repackages it
  as a classic script. This keeps the admin UI free of a bundler and of `unsafe-eval`.
- Tools and skills that belong to later subsystems arrive with them: `export_site` and the `export-site` skill in M6.
- Skill folders use `references/` and `assets/` (the Agent Skills spec's names) rather
  than the plan's `templates/`, so the reference validator accepts them.
- Anthropic and Gemini keys are optional inputs and were not provided, so those adapters
  are covered by mock-server fixtures only. If you add `ANTHROPIC_API_KEY` or
  `GEMINI_API_KEY` to `.env` and rerun `scripts/dev-secrets.sh`, add a provider in
  Settings and press Test.
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

## Phone checklist results

**2026-11-15, Android, Firefox, passkey stored in Bitwarden (run by Matthew).** Steps 1 to
13 and 15 of [testing.md](testing.md#manual-checklist-phone-flows) passed in the browser:
enrollment, sign-in, create with live streaming, edit, rollback, typed-confirmation
delete, agent-requested delete, provider test.

- **Found:** in the app installed to the home screen, signing in failed with "missing or
  wrong CSRF token". Cause: the session cookie is `SameSite=Strict`, so the browser
  withholds it from the navigation that launches the installed app. The server therefore
  rendered the sign-in page (with no CSRF token), but the page's own requests did carry
  the cookie, and the guard then demanded the session's token.
- **Fixed:** the passkey endpoints (`/auth/*`) no longer need the session's CSRF token;
  they require the admin `Origin` instead and are protected by the passkey ceremony
  itself. Adding a passkey from Settings still needs the token. The sign-in page also asks
  `/auth/session` on load and goes straight to the app when the browser is already signed
  in. Covered by a test.
- **Re-tested by Matthew after the fix:** sign-in from the home-screen app works, and
  step 14 (airplane mode) passes. All 15 steps pass.

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
