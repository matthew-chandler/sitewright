# ADR-0005: A hand-written release workflow instead of cargo-dist

- Status: accepted
- Date: 2026-11-15

## Context

The plan proposes generating `release.yml` with `cargo-dist` and extending it with
minisign signing and a post-install step.

## Decision

`release.yml` is written by hand: it builds both musl targets with `cargo-zigbuild`
(exactly as CI and `scripts/deploy-dev.sh` do), packages `sitewright-<target>.tar.gz` with
a SHA-256 file, signs each archive with minisign, takes the notes from `CHANGELOG.md` and
publishes with `gh release create`. `scripts/install.sh` is the installer and is attached
to every release.

## Why

- **One build path.** The binary links vendored C code (libgit2, SQLite, ring) for musl
  through zig. CI already proves that path on every commit; a second, generated build
  configuration would have to be kept working separately.
- **The updater depends on exact asset names and layout** (`sitewright-<target>.tar.gz`,
  `.sha256`, `.minisig`, the binary inside a top-level folder). Owning the workflow keeps
  that contract in one visible place, tested by `crates/sw-update/tests`.
- **The installer needs root-level steps** (system user, systemd unit, data directory)
  that the generated shell installer does not do; a wrapper around it would be about as
  long as the script itself.
- The workflow is about a hundred lines and has no generated parts to regenerate.

## Consequences

- No Homebrew formula, MSI or npm installer, none of which apply to a Linux daemon.
- `cargo-dist` stays out of `scripts/dev-setup.sh`'s required tools (it is still
  installed there, harmlessly; remove it when convenient).
- Adding a target means adding a matrix entry and a `case` branch in `install.sh`.
