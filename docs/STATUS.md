# Status

Live progress log for the v1 build described in [PLAN.md](../PLAN.md). A fresh session can
resume from this file.

_Last updated: 2026-11-15 (M0 in progress)_

## Needs from Matthew

1. **Run the VPS bootstrap once (needs sudo).** It adds one nginx file with two server blocks
   proxying to `127.0.0.1:18080`, and opens TCP 18443 if ufw is active. It modifies no
   existing nginx file and reverts itself if `nginx -t` fails.

   ```sh
   # from WSL, in ~/projects/sitewright
   scp scripts/vps-bootstrap.sh <your sudo user>@<vps>:/tmp/
   # on the VPS: review with --dry-run first, then run for real
   sudo /tmp/vps-bootstrap.sh --like /etc/nginx/sites-enabled/<an existing machandler.com site> --dry-run
   sudo /tmp/vps-bootstrap.sh --like /etc/nginx/sites-enabled/<an existing machandler.com site>
   ```

   `devtest`, its SSH key and lingering already exist, so `--pubkey` is not needed. Until
   this is done the daemon is only reachable on the VPS itself (`127.0.0.1:18080`); all
   public-hostname checks in `scripts/smoke-dev.sh` fail with that as the only cause.

2. **Create `.claude/settings.json` (optional, convenience).** Claude Code's permission
   classifier refused to let the agent write its own allow-list, which is reasonable. The
   exact file contents are in [development.md](development.md#claude-code-permissions).

## Milestones

| Milestone | State |
| --- | --- |
| M0 Scaffold | in progress |
| M1 Serve sites (proxy mode) | not started |
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

## In progress

- M0: GitHub repo, branch protection, green CI, first deploy to the VPS.

## Next

- M1: config, data directory, SQLite, site store with git and releases, host routing,
  static serving, request log, CLI and admin socket.

## Environment facts

- VPS: Ubuntu 20.04, x86_64, 1 vCPU class, ~1 GB RAM, nginx on 80/443. Ports 5000-5004 are
  used by Matthew's existing services; Sitewright dev uses 18080 (HTTP) and 18443 (TLS tests).
- `gh` in WSL is 2.4.0 (old): no `gh search`, use `gh api`.

## What to test by hand

Nothing yet.
