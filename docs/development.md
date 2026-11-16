# Development

## Local setup (WSL or any Linux)

```sh
./scripts/dev-setup.sh
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
```

The script installs, into your home directory and without sudo: rustup (stable, with both
musl targets), `cargo-zigbuild`, `cargo-nextest`, `cargo-deny`, `cargo-dist`, `sqlx-cli`,
`cargo-insta`, zig, minisign and Pebble.

## Everyday commands

```sh
cargo run -- serve --config dev/local.toml           # http://localhost:8080
cargo run -- --config dev/local.toml site import hello example   # http://hello.localhost:8080
./scripts/sqlx-prepare.sh                            # after changing a query or migration
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo deny check
```

LLM credentials for live tests go in a git-ignored `.env` at the repo root.

## ACME tests

`crates/sw-tls/tests/pebble.rs` and `crates/sitewright/tests/standalone.rs` run against
[Pebble](https://github.com/letsencrypt/pebble), Let's Encrypt's test server. They start
`pebble` and `pebble-challtestsrv` themselves on free ports; `scripts/dev-setup.sh`
installs both. Without the binaries the tests print a notice and pass; CI sets
`SW_REQUIRE_PEBBLE=1` so they cannot be skipped there.

## Releasing

1. Move the `Unreleased` notes in `CHANGELOG.md` under a new `## [X.Y.Z] - date` heading.
2. Set `version` in the workspace `Cargo.toml` (and the `sw-*` entries under
   `[workspace.dependencies]`), run `cargo check` to update `Cargo.lock`, merge.
3. Tag `vX.Y.Z` on `main` and push the tag. `release.yml` builds both architectures,
   signs the archives with minisign and publishes the release with `install.sh`.

The signing key's secret half lives in the repository secrets `MINISIGN_SECRET_KEY` and
`MINISIGN_PASSWORD`; the public half is `release/minisign.pub`, compiled into the binary.

## The dev VPS loop

The dev environment runs on a VPS as an unprivileged user (`devtest`), reached through the
SSH alias `racknerd-dev`. Nothing in this loop needs root.

```sh
./scripts/deploy-dev.sh     # build a static musl binary, upload, restart the user service
./scripts/smoke-dev.sh      # curl checks against the daemon and the public hostnames
```

The daemon listens on `127.0.0.1:18080`. Public traffic reaches it through two nginx server
blocks created once by `scripts/vps-bootstrap.sh` (run by the server owner with sudo; it
supports `--dry-run`). See the plan's "Dev hostnames" table for the hostnames.

### Claude Code permissions

To let Claude Code run the VPS loop without prompting, create `.claude/settings.json`:

```json
{
  "permissions": {
    "allow": [
      "Bash(ssh racknerd-dev:*)",
      "Bash(scp:* racknerd-dev:*)",
      "Bash(./scripts/deploy-dev.sh:*)",
      "Bash(./scripts/smoke-dev.sh:*)",
      "Bash(./scripts/dev-setup.sh:*)"
    ],
    "deny": ["Read(./.env)"]
  }
}
```
