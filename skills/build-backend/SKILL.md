---
name: build-backend
description: Add or change a site's backend - server-side JavaScript under backend/ that answers /api/ requests and stores data in the site's own SQLite database. Use when the site must save or serve data (forms, guestbooks, counters, votes, sign-ups, small APIs), or when changing existing files under backend/.
license: MIT OR Apache-2.0
metadata:
  author: sitewright
  version: "1.0"
---

# Build a backend

A backend is a few JavaScript files under `backend/` that answer the site's `/api/...`
requests. Everything else on the site stays static. Only add a backend when the server
has to remember or decide something; a page that only shows content needs none.

## What you are writing for

The code runs in Sitewright's own small JavaScript runtime, not in Node, Deno or a
browser. This shapes everything:

- **Only the `sitewright` module exists.** No `npm` packages, no `require`, no `fs`, no
  `process`, no `fetch`, no timers (`setTimeout`), no `Buffer`, no `URL` class. Standard
  JavaScript (ES2023: `JSON`, `Math`, `Date`, `Map`, `RegExp`, `async`/`await`) works.
- **You can import your own files** with relative paths: `import { x } from "./lib/x.js"`.
- **One database per site**, SQLite, reached through `db`. It is the only place to keep
  data. Variables in memory are lost whenever the server restarts the backend, which is
  often.
- **Requests and responses are UTF-8 text** (JSON, forms, HTML). No file uploads.
- **A handler has 10 seconds** and one request runs at a time per site. Keep handlers
  short; never loop waiting for something.
- The backend's files are never served to visitors, but they are in the site's history.
  Do not put passwords or API keys in them.

The full API is in `references/api.md`. Read it with `read_skill_file` before writing
code; do not guess at names.

## Files

```
backend/index.js                      entry point: must `export default` a Router
backend/lib/*.js                      optional helpers you import
backend/migrations/0001_<what>.sql    database schema, applied in order when you publish
```

Start from `assets/index.js` and `assets/0001_create_entries.sql`.

## Steps

1. **Design the data first.** Decide the tables and columns. Write them as a migration:
   `backend/migrations/0001_create_<things>.sql`. Use `INTEGER PRIMARY KEY` ids,
   `NOT NULL` where a value is required, `created_at INTEGER NOT NULL` holding
   `Date.now()` for timestamps, and an index for every column you filter or sort on.
2. **Write `backend/index.js`.** One route per action, all under `/api/`. Validate every
   input (see below). Return JSON.
3. **Test before publishing** with `test_backend`. It runs your unpublished code against
   a private copy of the site's data, so try the real flows: create something, read it
   back, send bad input and check for a 400. Read the `console` it returns; use
   `console.log` freely while debugging and remove the noise afterwards.
4. **Write the page that uses it.** Call the API with a relative URL:
   `fetch("/api/entries")`. Show a clear message when a request fails. Put user-supplied
   text into the page with `textContent`, never `innerHTML`.
5. **Validate and publish** as usual. Publishing loads the code and applies the
   migrations to the live database; if either fails, nothing changes and the error tells
   you why. After publishing, `backend_logs` shows what the live backend printed.

## Rules for migrations

- Migrations only ever **add**: new tables, new columns (`ALTER TABLE ... ADD COLUMN`
  with a `DEFAULT`), new indexes. This keeps older releases working if the user rolls
  back.
- **Never edit or delete a migration that has been published.** To change something, add
  the next numbered file. `backend_schema` shows what the live database looks like now.
- Handlers cannot run `CREATE`, `ALTER` or `DROP`; schema changes belong in migrations.
- Names are four digits, an underscore and lowercase words: `0002_add_votes.sql`.

## Rules for handlers

- **Always use `?` placeholders.** Never build SQL by joining strings with input.
- **Validate input and answer 400** with `throw new HttpError(400, "what is wrong")`.
  Check types, trim strings, and cap lengths (a name is not 10,000 characters).
- **Limit what you return.** Every list query needs `ORDER BY` and `LIMIT`.
- **Anyone on the internet can call the API.** Do not offer routes that delete or expose
  other people's data unless the request asked for that. `req.headers.get(
  "x-sitewright-visitor")` is an anonymous id for the visitor that changes daily; use it
  to stop one visitor voting twice or posting too often. Visitor IP addresses are not
  available.
- Do not catch errors just to hide them. An uncaught error becomes a 500 for the visitor
  and a stack trace in `backend_logs`, which is what you want.
- Changing data in two steps that must both happen? Use `db.batch`.

## When the user asks for something backends cannot do yet

Calling other web services, sending email, user accounts with passwords, file uploads
and scheduled jobs are not available yet. Say so plainly in your summary, and build the
part that is possible.
