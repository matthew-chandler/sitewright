# Sitewright

[![CI](https://github.com/matthew-chandler/sitewright/actions/workflows/ci.yml/badge.svg)](https://github.com/matthew-chandler/sitewright/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Describe a website from your phone; have it live on its own subdomain minutes later, on a
server you own.**

Sitewright is a single self-hosted binary. It serves your sites, hosts a mobile-first admin
UI, and runs an AI agent that builds and edits static sites through a small set of guarded
tools. It works with any LLM provider (OpenAI-compatible, Anthropic, Gemini) and any DNS host.

> **Status: under construction.** v1 is being built milestone by milestone. See
> [docs/STATUS.md](docs/STATUS.md) for what works today and [PLAN.md](PLAN.md) for the design.

## What v1 will do

- Static sites, one per hostname, each versioned in git with one-tap rollback.
- An agent that creates, edits, validates and publishes sites. It has no shell and no network
  access; destructive actions need a human to confirm.
- Proxy mode (behind nginx, Caddy or Cloudflare) and standalone mode (built-in Let's Encrypt).
- A PWA admin UI with passkey login and live agent progress.
- Privacy-friendly metrics, uptime checks and an AI-designed dashboard per site.
- Signed releases and self-update with rollback.

## Building from source

```sh
./scripts/dev-setup.sh          # Rust, musl targets and dev tools, all in your home directory
cargo run -- serve --config dev/local.toml                        # admin: http://localhost:8080
cargo run -- --config dev/local.toml site import hello example    # site:  http://hello.localhost:8080
cargo nextest run
```

See [docs/development.md](docs/development.md).

## Documentation

Start at [docs/architecture.md](docs/architecture.md). Decisions are recorded in
[docs/adr/](docs/adr/).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in this project by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
