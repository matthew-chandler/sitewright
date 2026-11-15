# Security policy

## Reporting a vulnerability

Please do not open a public issue for security problems. Use GitHub's
[private vulnerability reporting](https://github.com/matthew-chandler/sitewright/security/advisories/new)
for this repository. You should get a reply within a week.

Include the version (`sitewright --version`), the serving mode (proxy or standalone), and
steps to reproduce.

## Scope

The threat model is described in [docs/security.md](docs/security.md). Of particular interest:
escaping the agent's file sandbox, reaching the admin UI without a passkey, reading metrics
outside the read-only views, and bypassing update signature verification.

## Supported versions

Only the latest release receives security fixes.
