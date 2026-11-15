# ADR-0003: Server-rendered UI with askama and htmx

- Status: accepted
- Date: 2026-11-15

## Context

The UI is a handful of mobile-first screens: sign in, a status grid, a prompt box, a live run
log, release history, dashboards and settings. The plan leaves the choice between
server-rendered HTML and a JavaScript framework over a JSON API to the implementer.

## Decision

Server-rendered HTML with `askama` templates, `htmx` for partial updates and SSE, and a small
amount of hand-written JavaScript for passkeys and charts. All assets are vendored and
embedded with `rust-embed`.

## Why

- No Node toolchain in the build: `cargo build` is the whole build, in CI and for
  contributors.
- Templates are type-checked at compile time and snapshot-testable with `insta`.
- The live run log is a natural fit for htmx's SSE extension.
- A light footprint: tens of kilobytes of JavaScript instead of a framework bundle.

## Consequences

- Rich client-side interactions would need more hand-written JavaScript; none are planned.
- The same handlers also expose a JSON API (for API tokens and scripting), so a different
  frontend remains possible later.
