# Status

Live progress log for the v1 build described in [PLAN.md](../PLAN.md). A fresh session can
resume from this file.

_Last updated: 2026-11-16 (1.4.0 on main: site backends; waiting on the nginx block for the domain move)_

## Needs from Matthew

1. **Add the nginx block for `*.machandler.com` (needs sudo).** Everything else for the
   move to `*.machandler.com` and `sitewright.machandler.com` is done; this is the last
   step. Your existing sites keep their own server blocks (exact names win over the
   wildcard) and no existing file is changed.

   ```sh
   # from WSL
   scp ~/projects/sitewright/scripts/vps-production.sh <your sudo user>@<vps>:/tmp/
   # on the VPS: look at the dry run, then run it
   sudo /tmp/vps-production.sh --like /etc/nginx/sites-enabled/fireworks.machandler.com --dry-run
   sudo /tmp/vps-production.sh --like /etc/nginx/sites-enabled/fireworks.machandler.com
   ```

   It asks certbot for a certificate for `sitewright.machandler.com` and writes one file,
   `/etc/nginx/sites-available/sitewright`. Two things change for visitors:
   `www.machandler.com` and any other subdomain without its own block now get Sitewright's
   "no site at this address" page instead of nginx's default site (today that is the `api`
   block).

2. **Enroll a new passkey** for the new admin address (passkeys are bound to the
   hostname, so the one made for `sitewright-dev` no longer applies). On the VPS as
   devtest:

   ```sh
   ~/bin/sitewright --data-dir ~/sitewright-data admin invite
   ```

   Open the printed `https://sitewright.machandler.com/enroll/...` link on your phone,
   then remove the old passkey under Settings, and re-add the app to the home screen.

3. **Optional clean-up, once you are happy:** remove the old dev names
   (`sitewright site hostname remove hikes hikes.dev.machandler.com`, same for `smoke`),
   delete `/etc/nginx/sites-enabled/sitewright-dev` and reload nginx, and delete the
   `*.dev` and `sitewright-dev` DNS records.

4. **Optional: publish a release.** `main` is at 1.5.0 and nothing after 1.0.0 is tagged.
   `git tag -a v1.5.0 -m "Sitewright 1.5.0" && git push origin v1.5.0` publishes it; the
   VPS can then move from its local build to `sitewright update`.

5. Still open from before: an offline copy of `~/.config/sitewright-release/`, README
   screenshots, `.claude/settings.json`.

6. **Check release 1.2 on your phone** (it is running on the VPS, reachable once #1 is
   done): the run page clock and "Back to site", Markdown in agent messages, Ctrl+Enter
   with a keyboard, and "Name and address" on a site's page.

7. **GitHub Copilot (roadmap 1.3) needs two things from you before I can build it.**
   What I found (2026-11-16):
   - GitHub Models, the simple hosted API, was retired on 2026-07-30, so that is out.
   - Calling Copilot's chat API directly is officially supported only for specific
     partner tools (OpenCode since January 2026). Other tools do it with VS Code's
     client ID; I will not ship that in a public repository tied to your account.
   - The supported way is GitHub's **Copilot SDK** (there is a Rust crate,
     `github-copilot-sdk`). It drives GitHub's Copilot runtime as a helper process and
     accepts a fine-grained personal access token or an OAuth app token. Sitewright's
     tools can be registered as custom tools with the built-in ones switched off. The
     runtime is a 209 MB download (150 MB compressed), and Copilot runs the agent loop,
     so this is a second engine beside the current one, not a new provider entry.
   - Unknown: how much memory the runtime uses. The VPS has about 550 MB free. I
     downloaded the runtime but was not permitted to execute it.

   What I need:
   - A decision: build the SDK route (my recommendation if the memory fits), or drop
     Copilot.
   - To measure memory, run this in WSL and send me the two numbers it prints (it
     starts the runtime with an empty home directory, waits, and stops it):

     ```sh
     cd /tmp/claude-1000/-home-matthew-projects-sitewright/1b882e3a-1b73-4cca-9cf6-d3391cee77b0/scratchpad/copilot-cli
     HOME=$PWD/home ./x/package/copilot --headless --stdio < /dev/null > /dev/null 2>&1 &
     sleep 8; ps -o rss=,vsz= -p $!; kill $!
     ```

     If that folder is gone (it is temporary), `npm pack @github/copilot-linux-x64`
     and unpack it first.
   - For testing against the real service: a fine-grained personal access token with
     the "Copilot Requests" permission, as `COPILOT_GITHUB_TOKEN` in `.env`.

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

- **Releases:** v0.9.0 and v1.0.0 were built, signed and published by `release.yml`
  (two architectures, each archive with `.sha256` and `.minisig`, plus `install.sh`).
- **Self-update acceptance on the VPS (2026-11-16):** the published 0.9.0 binary ran
  `sitewright update`, verified the signature and checksum, installed 1.0.0 and the
  service came back as 1.0.0; `update --rollback` returned it to 0.9.0; a second `update`
  returned it to 1.0.0. `doctor` was clean after every step and the sites stayed up.

- **Move to `*.machandler.com` (2026-11-16):** the VPS config now has
  `zones = ["machandler.com"]`, admin host `sitewright.machandler.com`, HTTPS links, proxy
  mode, and the names of the other services on that zone reserved. `hikes` and `smoke`
  have `<name>.machandler.com` as their primary hostname; their `.dev` names stay attached
  for now. A proxied `*.machandler.com` wildcard already existed in Cloudflare, so no DNS
  change was needed. The previous config is `config.toml.bak-dev-zone` on the VPS.
  Version 1.1.0 adds what this needed: `site hostname add|remove`, `reserved_names`,
  `scripts/vps-production.sh`, and a fix for a rare "database is locked" error.
- The likely cause of the earlier Cloudflare 525: unknown subdomains already work through
  Cloudflare via nginx's default block, and the one block that failed
  (`sitewright-dev`) is the only one without
  `include /etc/letsencrypt/options-ssl-nginx.conf;`. The new block includes it.

## In progress

- Waiting on Needs #1 to finish the move to `*.machandler.com`.
- Post-v1 work follows [ROADMAP.md](ROADMAP.md) (approved 2026-11-16).
  - **1.2** (portal polish, site rename): merged as version 1.2.0 and running on the VPS.
    Not tagged. Not yet checked on a phone (Needs #6).
  - **1.3** (GitHub Copilot): blocked on Needs #7. The "named models" half needs no
    work: a provider entry already is a named model with a picker in the prompt box.
    The `list_models` tool for the agent moves to 1.6, where it is first used.
  - **1.4** (backends: runtime and site database): merged as version 1.4.0. A site can
    have JavaScript under `backend/` answering `/api/`, with its own SQLite database;
    the agent has the `build-backend` skill and the `test_backend`, `backend_logs` and
    `backend_schema` tools. See `docs/backends.md`. Not yet tried from a phone.
  - **1.5** (secrets, outbound requests, visitor login): merged as version 1.5.0.
    Owner-managed secrets and approved hosts per site ("Backend access" on the site page,
    `sitewright backend ...`), `fetch` and `secrets.get` for backends, and built-in
    visitor accounts (`auth.*`).
  - **1.6** (AI models for backends, dashboards over site data): `llm.chat` through the
    daemon with per-site switch, model list, limits and usage is merged and on the VPS.
    Tried with the real model: it built a working AI-backed page, and usage was counted.
    Dashboards over a site's own database: in progress.
  - **1.7** (other backend frameworks): optional on the roadmap, and not started. My
    advice is to use 1.4 to 1.6 for a while first; say so if you want it built now.

## Not done, by choice or by circumstance

- The HTTPS certificate test (`sitewright::standalone`) failed 2 times in about 10 full
  local runs once the suite grew (it polls a local ACME server and timed out under
  load); alone it passes. It now gets up to two retries (`.config/nextest.toml`). Not
  investigated further.

- **Docker build of an exported context** was not run: Docker is not installed in WSL.
  The archive layout and digest-pinned Dockerfile are covered by tests.
- **Production Let's Encrypt certificates** were not requested; the plan allows them on at
  most 5 dev hostnames. Standalone mode was accepted with staging certificates. To try
  production: remove `acme_directory` from `[tls]` in the VPS config and restart.
- **Anthropic and Gemini** are verified by mock-server fixtures only; no keys were given.
- **README screenshots** are missing.

## Ideas for after v1

From the plan's "not in v1" list: DNS provider APIs (a `DnsProvider` trait), publishing
to other hosts, sites with backends, multiple users. Smaller follow-ups: bump GitHub
Actions that still target Node 20, drop `cargo-dist` from `dev-setup.sh` (ADR-0005), move
to a `libsqlite3-sys` that clears RUSTSEC-2026-0459 once sqlx allows it.

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
- Backends, from the portal once Needs #1 and #2 are done. Each is one prompt:
  1. "A guestbook where visitors leave their name and a message." Then post an entry on
     the site and look at **Backend** on the site's page (database size, console).
  2. On a site's page, under **Backend access**, tick "Allow this site's backend to use
     AI models" and save. Then prompt: "Add a box where visitors ask a question about
     the site and an AI model answers." Check the usage line afterwards.
  3. "Let visitors sign up and keep a private list of notes." Sign up as two people and
     check that neither sees the other's notes.
  4. Add a secret (any name and value) and prompt: "What secrets and hosts can this
     site's backend use?" The answer should name the secret and never show its value.
