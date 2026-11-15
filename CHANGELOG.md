# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Workspace scaffold, CI, dev and deploy scripts, and a placeholder daemon (M0).
- Proxy-mode site serving (M1): TOML configuration, SQLite state, a git-versioned site
  store with immutable releases and instant rollback, host-based routing, static file
  serving, a privacy-preserving request log, and `sitewright site` commands over an admin
  Unix socket.
- The agent (M2): an `LlmProvider` trait with an OpenAI-compatible adapter (OpenAI, Azure
  OpenAI, OpenRouter, Ollama and others), a tool-calling loop with twelve sandboxed tools,
  site validation, turn, token and daily-spend limits, cancellation, replayable run
  events with cost tracking, and `sitewright prompt`.
- The web UI (M3): a mobile-first, installable admin app with passkey sign-in, live run
  streaming, release history with one-tap rollback, typed-confirmation delete, provider
  settings with "Test connection", API tokens, and `sitewright admin invite`.
- Skills and more providers (M4): Agent Skills loaded on demand, four built-in skills,
  user skills in the data directory, and adapters for Anthropic and Gemini.
- Metrics and dashboards (M5): a cookie-free event beacon, uptime checks with site status,
  daily rollups with 30-day retention of raw data, a default dashboard, AI-designed
  dashboards whose SQL runs in a read-only sandbox, and a status grid on the home screen.
