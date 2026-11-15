# ADR-0002: Keep the name "Sitewright"

- Status: accepted
- Date: 2026-11-15

## Context

The plan asks for a name check before the first commit.

- **crates.io:** `sitewright` and every `sw-*` crate name used by the workspace were free.
- **GitHub:** a handful of small, unrelated repositories use the name (the largest had 6
  stars). None is a Rust crate, a registered product, or a self-hosted prompt-to-site daemon
  of any size.

## Decision

Keep **Sitewright**: repository `matthew-chandler/sitewright`, binary `sitewright`, internal
crates prefixed `sw-`.

## Consequences

If the crates are ever published and a name has been taken in the meantime, only the internal
`sw-*` crates would need renaming; the binary crate name is the one that matters.
