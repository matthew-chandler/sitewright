# Security model

The admin UI is on the public internet and drives an AI agent. The design assumes the
model can be wrong or manipulated, and limits what it can do rather than trusting it.
This page is the threat model; keep it current when behaviour changes.

To report a vulnerability, see [SECURITY.md](../SECURITY.md).

## Trust boundaries

| Party | Trusted to |
| --- | --- |
| Whoever has shell access as the daemon's user | Everything. This is the root of trust: the admin socket, `sitewright admin invite` and the data directory are all theirs. |
| A signed-in admin (passkey session) or API token | Manage sites, run the agent, change providers. Not: read stored secrets back. |
| The model | Nothing by default. It can only call the tools, each of which enforces its own rules. |
| Site content (written by the model or imported) | Nothing. It is served from separate hostnames and never executed by the server. |
| Visitors | Read published files. |

## Threats and mitigations

| Threat | Mitigation |
| --- | --- |
| Someone else reaches the admin UI | Passkeys only (no passwords to guess or phish); one-time enrollment links created from the server's shell, valid 15 minutes; login attempts rate-limited per client address; every admin action in `audit_log`. |
| Session theft or fixation | The cookie is `__Host-sw_session`: `Secure`, `HttpOnly`, `SameSite=Strict`, host-only (no `Domain`), so a site on a subdomain can neither read nor overwrite it. Only a SHA-256 of the token is stored. Sessions expire after 30 days. |
| Cross-site request forgery | Every state-changing request must carry the session's CSRF token in a header and, when the browser sends `Origin`, come from the admin origin. `SameSite=Strict` is a third layer. |
| Phishing the passkey | WebAuthn binds a credential to the admin origin; an assertion made for any other origin fails verification (tested). |
| A generated site attacks the admin UI | Sites and the admin UI are different hosts. The admin UI sends `Content-Security-Policy: default-src 'self'; frame-ancestors 'none'; …` and contains no inline scripts. Sites get `X-Content-Type-Options: nosniff`, `Referrer-Policy` and `frame-ancestors 'self'`. |
| Agent damages the server | No shell tool and no network tool exist (a test asserts it). File tools are bound to the run's one site and go through path confinement: relative paths only, no `..`, no hidden components (so no `.git`), an extension allowlist, symlinks refused by `lstat`, 1 MiB per file and 20 MiB per site. Releases are exported from git as regular files only. |
| Agent destroys work | Every publish is a git commit plus an immutable release; rollback is one pointer switch; a failed health check rolls back automatically. `delete_site` only raises a confirmation that a person answers by typing the site's name; deletion is soft for 7 days. |
| Prompt injection through site content | File contents and tool results are data: the system prompt says so, and, more importantly, nothing the model can be talked into exceeds the tool sandbox above. |
| Runaway LLM cost | Per-run turn and token caps, a per-day spend cap, one run per site and a cap on parallel runs. Limits end a run cleanly. |
| Leaked secrets | API keys live in `secrets.toml` (mode 0600; the daemon refuses a file other users can read), never in `config.toml` or the database. They are write-only in the UI, absent from `Debug` output, and error messages are built without request URLs or key material. |
| Enrollment-link or API-token theft | Both are random 256-bit values stored only as hashes. Invites are single-use and short-lived. API tokens work only under `/api` and cannot create enrollment links (that endpoint exists only on the Unix socket). |
| Agent-written SQL | A separate read-only connection; an SQLite authorizer that allows only `SELECT` on the metric views with an allowlist of functions; views scoped to one site by construction; a 5-second limit and a 1000-row limit. See [metrics.md](metrics.md#the-sql-sandbox). |
| A dashboard spec that loads or leaks data | Specs are data, not code: unknown fields are rejected, chart descriptions may not contain `data`, URLs or links, and the server injects the rows. Charts render with Vega's expression interpreter, so the admin UI needs no `unsafe-eval`. |
| Abuse of the event beacon | Strict validation, a 2 KB body limit, a per-visitor rate limit, bots ignored. Events can only add rows for the site whose hostname received them. |
| Abuse of `check_domain` to probe internal services | One GET to a fixed path with a random token, no redirects, and only "matched or not" is reported. |
| Visitor privacy | No raw IP addresses are stored: visitors are a hash of IP, user agent and site, salted with a key that rotates (and is destroyed) daily. Query strings are not logged. The beacon sets no cookies, stores nothing on the device and honours Do Not Track and Global Privacy Control. Raw rows are kept 30 days. |
| Vulnerable dependencies | `cargo deny` (advisories, licenses, bans) in CI; Dependabot. One TLS stack (`rustls` + `ring`); OpenSSL is banned. Current, justified advisory exceptions are listed in `deny.toml`. |

Arriving with M6: certificate issuance limited to known hostnames, the hardened systemd
unit and signed updates.

## What is deliberately out of scope in v1

- Multiple users or roles: there is one admin.
- Protecting against someone with shell access as the daemon's user.
- Sandboxing JavaScript inside generated sites beyond normal browser origin rules. Do not
  point a site hostname at something that shares cookies with other services.
