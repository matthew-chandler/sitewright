---
name: build-backend
description: Add or change a site's backend - server-side JavaScript under backend/ that answers /api/ requests, stores data in the site's own SQLite database, signs visitors in, and calls outside APIs with the owner's keys. Use when the site must save or serve data (forms, guestbooks, counters, votes, sign-ups, small APIs), needs visitor accounts or members-only content, must call another service, or when changing existing files under backend/.
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
  `process`, no timers (`setTimeout`), no `Buffer`, no `URL` class. Standard JavaScript
  (ES2023: `JSON`, `Math`, `Date`, `Map`, `RegExp`, `async`/`await`) works.
- **`fetch` reaches only hosts the owner approved**, over HTTPS, and the owner's API keys
  are read by name with `secrets.get`. See "Calling other services" below.
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
- Tables, indexes and views only. `CREATE TRIGGER` is refused.
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

## Calling other services

A backend can call an outside HTTPS API, with two things only the site's owner controls:

- **Approved hosts.** `fetch` works only for hosts on the site's list. Call
  `backend_access` to see the list. If the host you need is missing, call
  `request_backend_host` with the bare host name (`api.example.com`) and a one-sentence
  reason. You cannot approve it. Build and publish the feature anyway, make the page
  show a friendly "not available yet" message when the call fails, and say in your
  summary that the owner must approve the host on the site's page before it works.
- **Secrets.** API keys are stored by the owner under a name such as `WEATHER_API_KEY`.
  `backend_access` lists the names. Read one inside a handler with
  `await secrets.get("WEATHER_API_KEY")`. If the key you need is not there, pick a clear
  name, use it in the code, and tell the owner in your summary to add a secret with
  exactly that name on the site's page. Never ask for the value, never write a key into
  a file, and never log a secret or send it to the browser. You will only ever see
  `[secret NAME]` in test output.

Rules for outbound calls:

- Keep keys on the server: the page calls your `/api/...` route, and your route calls
  the outside service. Never put a key in a page or its JavaScript.
- A call has 8 seconds and the handler 10, so make one call per request, not a chain.
- Check `res.ok` and handle failure: answer 502 with a short message rather than
  passing the other service's error text through.
- Responses are text up to 4 MB. Compressed responses (gzip, deflate, Brotli) are
  decompressed for you; do not set `accept-encoding`. Redirects are not followed.
- Never work around a failed `fetch` by calling the outside API from the page's own
  JavaScript: most APIs do not allow other sites' pages to read them (CORS), so it
  fails in the visitor's browser. Fix the backend call, or tell the owner what failed.
- Do not call an outside service on every page view if the answer changes slowly: store
  it in the database with a timestamp and reuse it for a few minutes.
- Visitors can reach your route as often as they like. If the outside service costs
  money per call, limit calls per visitor (see the template's rate limit) and cache.

`test_backend` makes real outbound calls with the real secrets, so test sparingly
against services that charge or that change data.

## Using AI models

A backend can ask an AI model with `llm.chat`, through Sitewright. It needs no API key
and no approved host. Whether a site may do this, with which models and how often, is
the owner's setting.

- Call `list_models` first. It shows the models by name, which ones this site may use,
  and whether AI access is on for the site at all.
- Put the model's name in one constant at the top of `backend/index.js`
  (`const MODEL = "fast";`) and pass it as `llm.chat({ model: MODEL, ... })`. When the
  user later says "use the new model" or names one, `list_models` tells you which name
  they mean, and the change is that one line.
- If access is off, or the model the user wants is not allowed for this site, you
  cannot change that. Write the code anyway, make the page show a friendly message when
  the call fails, and tell the owner in your summary to turn it on under "Backend
  access" on the site's page.
- Every visitor can trigger the route, and each call counts against the site's monthly
  allowance. Limit calls per visitor (see the template's rate limit), cap the length of
  what visitors may send, and store answers that can be reused.
- Put your instructions in `system`, and the visitor's text in a `user` message. Never
  paste visitor text into `system`. Treat the model's answer as untrusted text: show it
  with `textContent`, and do not run SQL or make decisions about access from it.
- A model call can take many seconds. Show a "thinking" state on the page.

The call's shape is in `references/api.md`. `test_backend` makes real model calls.

## Visitor accounts

When visitors need to sign up and sign in, use the built-in `auth` module. Read
`references/accounts.md` first. Never write your own password or session handling, and
never create your own users table.

## When the user asks for something backends cannot do yet

Sending email (unless through an HTTPS API the owner approves), password reset by email,
file uploads, scheduled jobs and triggers in the database are not available. Say so
plainly in your summary, and build the part that is possible.
