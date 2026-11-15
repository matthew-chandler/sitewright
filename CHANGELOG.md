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
