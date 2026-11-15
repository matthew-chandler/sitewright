# ADR-0001: Technology stack

- Status: accepted
- Date: 2026-11-15

## Context

Sitewright must ship as one static binary with no runtime dependencies, be registrar- and
provider-agnostic, and be boring to operate. `PLAN.md` proposes a stack; this ADR records it
and the adjustments made while scaffolding.

## Decision

| Area | Choice |
| --- | --- |
| Language and build | Rust stable, edition 2024; static `x86_64-` and `aarch64-unknown-linux-musl` binaries via `cargo-zigbuild` |
| Async and HTTP | `tokio`, `axum`, `tower-http` |
| TLS and ACME | `rustls` with the **`ring`** crypto provider; `instant-acme` (HTTP-01) with a per-hostname certificate resolver |
| Database | SQLite via `sqlx` (WAL, embedded migrations, compile-time-checked queries with committed offline data) |
| Site versioning | `git2` with vendored libgit2, network features off |
| HTTP client | `reqwest` with `rustls-no-provider` |
| LLM access | Own `LlmProvider` trait with three adapters on `reqwest`; no LLM framework crate |
| UI | Server-rendered `askama` templates plus `htmx`; assets embedded with `rust-embed` (see ADR-0002) |
| Charts | Vega-Lite rendered by a vendored `vega-embed` |
| Passkeys | `webauthn-rs` |
| CLI and config | `clap`; TOML via `serde`; separate `secrets.toml` |
| Logging | `tracing`, JSON output in production |
| DNS checks | `hickory-resolver` plus an HTTP token check |
| Releases | `cargo-dist`; `minisign` signatures verified with `minisign-verify`; `self-replace` |
| Testing | `cargo-nextest`, `insta`, `wiremock`, Pebble for ACME |
| Supply chain | `cargo-deny`, Dependabot |
| License | MIT OR Apache-2.0 |

### Adjustments to the plan

- **`ring` instead of the rustls default (`aws-lc-rs`).** `ring` cross-compiles to both musl
  targets with `zig cc` without CMake or NASM, and keeps the binary smaller. The provider is
  installed once per process by `sw_core::crypto::install_default_provider()`. `cargo deny`
  bans `openssl-sys` and `native-tls` so there is exactly one TLS stack.

## Consequences

- One file to install and update; no Docker, no system libraries.
- Compile-time-checked SQL needs `scripts/sqlx-prepare.sh` after query or schema changes.
- Replacing any row above requires a new ADR.
