# Site backends

A site can have a backend: JavaScript that the agent writes under `backend/` and that
answers the site's `/api/...` requests. Everything else on the site stays static. The
design and its reasons are in [ADR-0006](adr/0006-site-backends.md).

Ask for it in a prompt ("add a guestbook that saves entries") and the agent loads its
`build-backend` skill, writes the code and a database migration, tests them, and
publishes.

## What a backend is

```
backend/index.js                      entry point; exports a Router
backend/lib/*.js                      optional helpers
backend/migrations/0001_<what>.sql    database schema, applied in order at publish
```

- The files live in the site's repository, so they are versioned, published and rolled
  back with the site. They are never served to visitors: `/backend/...` is always 404.
- Requests to `/api` and `/api/...` go to the backend. A site without `backend/index.js`
  serves those paths as ordinary files.
- Each site has its own SQLite database at `<data>/sites/<name>/data/site.db`. It moves
  with the site when you rename it and is removed with it when a deleted site is purged.
- The API a backend can use (`Router`, `db`, `json`, ...) is described in
  [`skills/build-backend/references/api.md`](../skills/build-backend/references/api.md).

## How it runs

- **One worker process per site**, started on the first `/api` request and stopped after
  five minutes without one. It is the `sitewright` binary itself, started with an
  internal argument, running the QuickJS engine. A worker uses a few megabytes.
- **The worker is jailed.** Before it reads any site code it installs a seccomp filter
  that allows about thirty system calls: reading and writing its two pipes, memory, the
  clock, randomness and exiting. It cannot open files, use the network, start processes
  or send signals, and it is started with an empty environment.
- **Everything else is a request to the daemon**, which knows which site the worker
  belongs to: SQL on the site's own database, reading one of the site's secrets, and an
  HTTPS request to a host approved for the site. The worker never sees
  another site's data, Sitewright's own database, `secrets.toml` or a provider key.
- **Site SQL is fenced in**: handlers can read and write rows; only migrations can change
  the schema; `ATTACH`, `PRAGMA`, transaction statements and extension loading are
  refused for both. Statements have a 5-second limit, queries a 5000-row limit, and the
  database a size limit.
- **Visitors' addresses are not passed on.** A backend gets `x-sitewright-visitor`, the
  same anonymous, daily-rotating id the metrics use, and none of the `X-Forwarded-For`
  family of headers.

## Visitor accounts

A backend can let visitors sign up and sign in with `auth.signUp`, `auth.signIn` and
`auth.user`. The backend's code never handles a password hash or a session token:

- Accounts and sessions live in two tables Sitewright manages inside the site's
  database, `sw_users` and `sw_sessions`. Site SQL can read `id`, `email` and
  `created_at` of users (to join its own tables to them) and nothing else; it cannot
  write to either table, and migrations cannot alter them or attach triggers to
  anything (a trigger would run with Sitewright's own rights).
- Passwords are hashed with Argon2id. Sessions are random 256-bit tokens, stored only
  as SHA-256 hashes, valid for 30 days.
- The session cookie is `HttpOnly`, `SameSite=Lax`, and on HTTPS `Secure` with the
  `__Host-` prefix, so it belongs to that one site's hostname.
- Five failed sign-ins for an account lock it for 15 minutes. A sign-in for an unknown
  address takes as long as one for a known address.
- Changing a password ends all of that account's sessions.

There is no email verification and no password reset, because Sitewright cannot send
email. To remove an account or sign someone out yourself, stop the daemon and edit the
site's database: `sqlite3 <data>/sites/<name>/data/site.db "DELETE FROM sw_users WHERE
email = '...'"`.

## AI models

A backend can ask one of your configured models with `llm.chat`, without a key of its
own: the daemon makes the call with its own credentials. A model is known to backends
and to the agent by its provider id (the name you gave it in Settings or in
`[[llm.providers]]`).

Per site, under **Backend access** on the site's page or with the CLI:

```sh
sitewright backend ai hikes                                   # show settings and usage
sitewright backend ai hikes --enable --monthly 500 --per-minute 10
sitewright backend ai hikes --models fast,claude              # which models it may use
sitewright backend ai hikes --disable
```

- **Off by default.** Until you turn it on for a site, `llm.chat` fails there.
- **Which models.** With no list, the site may use the default model only. With a list,
  exactly those; the first is the site's default.
- **Limits.** Requests per minute (default 20) and per calendar month, UTC (default
  1000). Visitors can trigger these calls, so these two numbers bound what a site can
  cost you. Requests refused by a limit are not sent to the model.
- **Accounting.** Every call is recorded with the site, model, tokens and, when you
  configured a price for the model, cost. The site's page shows the month's total.
  This spend is separate from `agent.daily_spend_cap_usd`, which covers building sites.
- Backends get plain chat: text in, text out, no tools.

**Changing models.** Add a model in Settings, allow it for the site, and tell the agent
in a prompt to use it by name. The agent has a `list_models` tool to see the names and
keeps the model's name in one constant in the backend.

A model call can take longer than the 10-second handler limit. Time a handler spends
waiting for the daemon (a query, an outside request, a model call) does not count
against that limit; each of those has its own (5 s, 8 s and 60 s), and a request may
take 90 seconds in all. While one request waits, the site's other API requests queue.

## Secrets and outside services

A backend can call other services over HTTPS, under two controls that only you hold.
Both are on the site's page under **Backend access**, and in the CLI.

**Secrets** are values such as API keys that a backend reads by name:

```sh
sitewright backend set-secret hikes WEATHER_API_KEY    # reads the value from standard input
sitewright backend secrets hikes                       # names only
sitewright backend remove-secret hikes WEATHER_API_KEY
```

- They are stored in `<data>/site-secrets.toml` (mode 0600), like provider keys in
  `secrets.toml`: not in the database, not in the site's repository, not in a log. They
  are not encrypted on disk; protect the data directory and its backups.
- A value cannot be read back through the portal, the API or the CLI.
- The agent sees the names, never a value. Anything that carries backend output back to
  the model (test results, console lines) has secret values replaced by `[secret NAME]`.
  This stops accidental exposure; it cannot stop code that deliberately transforms a
  secret before printing it, so read what the agent wrote before you trust a site with a
  valuable key.
- A site's backend, once it has a secret, can send it to any host you approved for that
  site and can show it to visitors if its code does so. Approve hosts with that in mind.

**Approved hosts** are the only places a backend's `fetch` can reach:

```sh
sitewright backend allow-host hikes api.open-meteo.com
sitewright backend hosts hikes
sitewright backend remove-host hikes api.open-meteo.com
```

- The agent can ask for a host (it then shows as "requested" with the agent's reason);
  only you can approve it. Until then requests to it fail.
- Requests are HTTPS on port 443 to a host name. The daemon resolves the name itself,
  refuses loopback, private, link-local and other non-public addresses, and connects to
  the address it checked. Redirects are not followed. A request has 8 seconds and a
  1 MB text response.
- Lists are per site: approving a host for one site does nothing for another.

## Publishing and rolling back

When a release with a backend is published:

1. The code must load. A syntax error or a missing import stops the publish.
2. The database is copied to `site.db.before-publish`, then the new migrations run, each
   in its own transaction. If one fails, the copy is put back and the publish stops.
3. The release goes live. If the health check then fails, the previous release *and* the
   database copy are restored.

Rolling back to an earlier release switches the code but leaves the database as it is.
That is why migrations should only add (tables, columns with defaults, indexes): the
older code keeps working with a newer schema. The agent's skill tells it so. A migration
that has run can never be edited; a change is a new numbered file.

`site.db.before-publish` is the state before the most recent publish. Restore it by hand
if you need to: stop the daemon, replace `site.db` with it (remove `site.db-wal` and
`site.db-shm`), start the daemon.

## Limits

| | Default | Setting |
|---|---|---|
| Handler run time | 10 s | `backends.request_timeout_secs` |
| JavaScript memory per site | 64 MB | `backends.memory_mb` |
| Database size per site | 100 MB | `backends.max_db_mb` |
| Idle time before a worker stops | 300 s | `backends.idle_secs` |
| Workers alive at once | 8 | `backends.max_workers` |
| Request body | 1 MB | fixed |
| Response body | 4 MB | fixed |

One request runs at a time per site; others wait up to ten seconds and then get 503.
When the worker limit is reached, the worker idle longest is stopped to make room.
Set `backends.enabled = false` to turn backends off; sites with a backend can then not
be published.

## Seeing what a backend does

- The site's page in the portal has a **Backend** section with the database size and the
  most recent console output (kept in memory, 200 lines per site).
- `sitewright doctor` starts a throwaway worker to check that backends can run here.
- Backend requests appear in the site's metrics like any other request.

## What backends cannot do yet

Email, scheduled jobs, binary uploads, streaming responses and npm packages.

## Backups

Site databases use SQLite's WAL mode. Copy them with the daemon stopped, or with
`sqlite3 site.db ".backup copy.db"`, not with a plain `cp` of a live file.
