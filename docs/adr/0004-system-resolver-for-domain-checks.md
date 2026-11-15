# ADR-0004: Domain checks use the system resolver, not hickory-resolver

- Status: accepted
- Date: 2026-11-15

## Context

The plan lists `hickory-resolver` plus an HTTP token check for `check_domain`. The check
has to answer one question: do requests for this hostname reach this server?

## Decision

`check_domain` resolves the name with the operating system's resolver
(`tokio::net::lookup_host`) and then fetches
`http://<hostname>/.well-known/sitewright/<random token>`, which the daemon answers on
every hostname. `hickory-resolver` is not a dependency.

## Why

- The HTTP token check is what proves reachability, and it is the only part that works
  behind Cloudflare or another proxy, where the name resolves to someone else's addresses.
  DNS only has to say "resolves" or "does not resolve".
- The system resolver sees what visitors on this network see and needs no configuration.
  It works in the static musl build (musl ships its own stub resolver).
- One fewer large dependency tree in a binary that should stay small.

## Consequences

- The check reports addresses, not record types (A, AAAA, CNAME) or TTLs. If the UI
  later wants to explain *which* record is wrong, add `hickory-resolver` then.
- The check makes the server send one GET to a hostname the caller chooses. The path is
  fixed, redirects are not followed, the body is only compared with the expected token,
  and the result says only whether it matched, so it cannot be used to read internal
  services.
