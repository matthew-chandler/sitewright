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
  belongs to. Today that is SQL on the site's own database. The worker never sees
  another site's data, Sitewright's own database, `secrets.toml` or a provider key.
- **Site SQL is fenced in**: handlers can read and write rows; only migrations can change
  the schema; `ATTACH`, `PRAGMA`, transaction statements and extension loading are
  refused for both. Statements have a 5-second limit, queries a 5000-row limit, and the
  database a size limit.
- **Visitors' addresses are not passed on.** A backend gets `x-sitewright-visitor`, the
  same anonymous, daily-rotating id the metrics use, and none of the `X-Forwarded-For`
  family of headers.

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

Calling other services, stored secrets, visitor accounts, LLM access, scheduled jobs,
binary uploads and npm packages. The first four are on the [roadmap](ROADMAP.md).

## Backups

Site databases use SQLite's WAL mode. Copy them with the daemon stopped, or with
`sqlite3 site.db ".backup copy.db"`, not with a plain `cp` of a live file.
