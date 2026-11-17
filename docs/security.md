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
| Uploaded files | Owners can upload media, fonts, documents and data only, never HTML or JavaScript; names are cleaned to the path rules and the size limits apply. The agent can read uploads but not change them. The portal shows them from `/sites/<name>/uploads/<file>` with `Content-Security-Policy: sandbox`, so a hostile SVG cannot run script on the admin origin. An upload goes live only when nothing else is pending. See ADR-0009. |
| Agent damages the server | No shell tool and no network tool exist (a test asserts it). File tools are bound to the run's one site and go through path confinement: relative paths only, no `..`, no hidden components (so no `.git`), an extension allowlist, symlinks refused by `lstat`, 1 MiB per file and 20 MiB per site. Releases are exported from git as regular files only. |
| Agent destroys work | Every publish is a git commit plus an immutable release; rollback is one pointer switch; a failed health check rolls back automatically. `delete_site` only raises a confirmation that a person answers by typing the site's name; deletion is soft for 7 days. |
| Prompt injection through site content | File contents and tool results are data: the system prompt says so, and, more importantly, nothing the model can be talked into exceeds the tool sandbox above. |
| Runaway LLM cost | Per-run turn and token caps, a per-day spend cap, one run per site and a cap on parallel runs. Limits end a run cleanly. |
| Leaked secrets | API keys live in `secrets.toml` (mode 0600; the daemon refuses a file other users can read), never in `config.toml` or the database. They are write-only in the UI, absent from `Debug` output, and error messages are built without request URLs or key material. |
| Enrollment-link or API-token theft | Both are random 256-bit values stored only as hashes. Invites are single-use and short-lived. API tokens work only under `/api` and cannot create enrollment links (that endpoint exists only on the Unix socket). |
| Agent-written backend code | Runs in a separate worker process per site that jails itself with seccomp before reading any site code: no files, no network, no processes, no signals, an empty environment. Its only way out is a request to the daemon, confined to that site. Time, memory and size limits apply. See [backends.md](backends.md) and ADR-0006. |
| A backend reaching internal services or leaking data out | Backends have no network access of their own. The daemon makes their HTTPS requests, only to hosts the owner approved for that site, only to public addresses (resolved and pinned by the daemon, so no loopback, private ranges, metadata endpoints or DNS rebinding), without following redirects. |
| A site's visitors running up the model bill | Model access for backends is off per site until the owner enables it, limited to the models the owner lists, rate-limited per minute and capped per month. Limits are checked before a request is sent; every call is recorded. Backends never hold a provider key. |
| Backend secrets reaching the model | The agent can list secret names only. Test output and console lines are passed through a redaction step before the model sees them. Deliberate obfuscation by agent-written code is not prevented; see [backends.md](backends.md#secrets-and-outside-services). |
| Agent-written code mishandling visitors' passwords | It never sees them stored: accounts and sessions are managed by the daemon (Argon2id, hashed session tokens, attempt limits). An authorizer hides the hash column and the sessions table from site SQL, and triggers are refused so no site code runs with the daemon's rights. |
| Agent-written SQL in backends | Each site has its own database file on its own connection. An authorizer refuses `ATTACH`, `PRAGMA`, transaction control and extension loading; handlers cannot change the schema; statements have time and row limits. |
| Agent-written SQL | A separate read-only connection; an SQLite authorizer that allows only `SELECT` on the metric views with an allowlist of functions; views scoped to one site by construction; a 5-second limit and a 1000-row limit. See [metrics.md](metrics.md#the-sql-sandbox). |
| A dashboard spec that loads or leaks data | Specs are data, not code: unknown fields are rejected, chart descriptions may not contain `data`, URLs or links, and the server injects the rows. Charts render with Vega's expression interpreter, so the admin UI needs no `unsafe-eval`. |
| Abuse of the event beacon | Strict validation, a 2 KB body limit, a per-visitor rate limit, bots ignored. Events can only add rows for the site whose hostname received them. |
| Abuse of `check_domain` to probe internal services | One GET to a fixed path with a random token, no redirects, and only "matched or not" is reported. |
| Visitor privacy | No raw IP addresses are stored: visitors are a hash of IP, user agent and site, salted with a key that rotates (and is destroyed) daily. Query strings are not logged. The beacon sets no cookies, stores nothing on the device and honours Do Not Track and Global Privacy Control. Raw rows are kept 30 days. |
| Certificate rate-limit abuse through random subdomains | In standalone mode certificates are requested only for hostnames that belong to a site (and the admin host). The TLS resolver returns no certificate for anything else, and a handshake never triggers issuance. Failed attempts back off for up to 6 hours, and a precheck skips names that do not reach the server. |
| Tampered updates | Release archives are signed with minisign. `sitewright update` verifies the signature against the public key compiled into the running binary, and the SHA-256, before replacing anything; the previous binary and a pre-migration database copy are kept for `--rollback`. |
| Vulnerable dependencies | `cargo deny` (advisories, licenses, bans) in CI; Dependabot. One TLS stack (`rustls` + `ring`); OpenSSL is banned. Current, justified advisory exceptions are listed in `deny.toml`. |

## Notes on specific mechanisms

- **The systemd unit** (`deploy/sitewright.service`) runs the daemon as the unprivileged
  `sitewright` user with `ProtectSystem=strict`, `NoNewPrivileges`, `PrivateTmp`,
  `ProtectHome`, a system-call filter, and only `CAP_NET_BIND_SERVICE`. It may write to its
  data directory and to the directory holding its own binary; the latter is what allows a
  verified self-update.
- **The release key** is a minisign key. Its public half is compiled into every binary
  (`release/minisign.pub`); the secret half exists only as a GitHub Actions secret and in
  the owner's offline backup. Rotating it means shipping a release, signed with the old
  key, that carries the new public key.

## What is deliberately out of scope in v1

- Multiple users or roles: there is one admin.
- Protecting against someone with shell access as the daemon's user.
- Sandboxing JavaScript inside generated sites beyond normal browser origin rules. Do not
  point a site hostname at something that shares cookies with other services.
