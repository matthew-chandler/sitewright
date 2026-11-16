# Instructions for AI coding agents

Read `PLAN.md` (the v1 brief) and `docs/STATUS.md` (live progress) before changing anything.

## Build and test

```sh
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo deny check
```

All four must pass before a commit. CI runs the same commands plus a static musl build.

## Conventions

- Rust stable, edition 2024. No `unsafe` outside modules that explicitly allow it with a
  comment explaining why.
- No `unwrap()` in non-test code; use `?`, or `expect("why this cannot fail")`.
- Errors: `thiserror` enums in library crates, `anyhow` only in the binary crate.
- TLS is `rustls` with the `ring` provider. Never add a dependency on OpenSSL or `native-tls`
  (`cargo deny` enforces this). Enable `reqwest` with `rustls-no-provider`.
- Database access goes through `sqlx` compile-time-checked macros. After changing a query or
  a migration, run `scripts/sqlx-prepare.sh` and commit the `.sqlx/` directory.
- Migrations are additive. Never edit a migration that has been merged.
- Every significant decision gets an ADR in `docs/adr/`. Keep `docs/STATUS.md` current.
- Conventional Commits. One branch per milestone, squash-merged through a PR with green CI.

## Hard rules

- Never commit, print or log secrets. `.env` and `secrets.toml` are git-ignored; keep it so.
- On the dev VPS act only as `devtest` over `ssh racknerd-dev`. Never use `sudo`. Anything
  that needs root goes into `docs/STATUS.md` under "Needs from Matthew".
- The agent inside Sitewright must never gain a shell tool or a network tool. Code it
  writes for a site's backend runs only inside the jailed worker (`sw-backend`, ADR-0006)
  and reaches the network only through the daemon, to hosts the owner approved for that
  site. Never widen the worker's system-call allowlist without an ADR.
