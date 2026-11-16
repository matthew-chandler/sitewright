# The `sitewright` module

```js
import { Router, json, text, html, redirect, HttpError, db, auth, secrets, fetch, log, crypto } from "sitewright";
```

## Router

```js
const app = new Router();
app.get("/api/entries", handler);        // also post, put, patch, delete, all
app.get("/api/entries/:id", handler);    // req.params.id
app.get("/api/files/*", handler);        // req.params["*"] is the rest of the path
app.use(async (req, next) => {           // runs around every handler
  const res = await next();
  res.headers.set("x-example", "1");
  return res;
});
export default app;
```

- Routes are tried in the order they were added; the first match wins.
- A path no route matches answers 404 `{"error":"not found"}`; a path that matches with
  another method answers 405.
- `HEAD` requests are answered by the `GET` handler.
- Only paths under `/api/` reach the backend.

## The request

A handler is `(req) => response`, and may be `async`.

| | |
|---|---|
| `req.method` | `"GET"`, `"POST"`, ... |
| `req.path` | `"/api/entries/5"` |
| `req.params` | path parameters, e.g. `{ id: "5" }` (always strings) |
| `req.query` | query string as an object, e.g. `{ limit: "10" }` (always strings) |
| `req.searchParams` | `get(name)`, `getAll(name)`, `has(name)` for repeated names |
| `req.headers.get(name)` | a header, or `null` |
| `req.cookies` | cookies as an object |
| `await req.json()` | the body parsed as JSON; answers 400 by itself if it is not JSON |
| `await req.form()` | a posted HTML form as an object |
| `await req.data()` | JSON or form, whichever was sent |
| `await req.text()` | the raw body |

Bodies are limited to 1 MB.

## The response

Return any of these from a handler:

| | |
|---|---|
| `json(data, status = 200, headers)` | JSON |
| `text(string, status = 200, headers)` | plain text |
| `html(string, status = 200, headers)` | an HTML page |
| `redirect(location, status = 303)` | a redirect, e.g. after a form post |
| `new Response(body, { status, headers })` | anything else |
| a plain object or array | same as `json(...)` |
| a string | same as `text(...)` |
| nothing | 204 No Content |

`throw new HttpError(status, message)` answers `{"error": message}` with that status.
Any other thrown error answers 500 `{"error":"internal error"}` and is logged.

Set a cookie with a header:
`new Response("", { status: 204, headers: { "set-cookie": "seen=1; Path=/; Max-Age=86400; HttpOnly; Secure; SameSite=Lax" } })`.

Responses are sent with `Cache-Control: no-store` unless you set that header.

## The database

SQLite. All functions are `async`.

```js
const rows = await db.query("SELECT id, name FROM entries WHERE name LIKE ? ORDER BY id DESC LIMIT ?", `%${q}%`, 20);
const row = await db.get("SELECT * FROM entries WHERE id = ?", id);      // an object or null
const { changes, lastInsertRowid } = await db.run("INSERT INTO entries (name, created_at) VALUES (?, ?)", name, Date.now());
await db.batch([                                                         // one transaction: all or nothing
  ["UPDATE polls SET votes = votes + 1 WHERE id = ?", [pollId]],
  ["INSERT INTO ballots (poll_id, visitor) VALUES (?, ?)", [pollId, visitor]],
]);
```

- Parameters are passed after the SQL, or as one array.
- Values: numbers, strings, `true`/`false` (stored as 1/0) and `null`. Objects and arrays
  are stored as JSON text; read them back with `JSON.parse`.
- Rows come back as plain objects keyed by column name.
- `INSERT ... RETURNING` works through `db.query`.
- A query may return at most 5000 rows and run for 5 seconds. The database may grow to
  100 MB.
- Not available in handlers: `CREATE`/`ALTER`/`DROP` (use migrations), `PRAGMA`,
  `ATTACH`, `BEGIN`/`COMMIT` (use `db.batch`). Triggers are not available at all.
- A constraint violation (for example `UNIQUE`) throws; catch it when it is an expected
  outcome, such as "already voted", and answer 409.

## Visitor accounts

`auth.signUp`, `auth.signIn`, `auth.user`, `auth.requireUser`, `auth.signOut`: see
`references/accounts.md`.

## Secrets and outbound requests

```js
const key = await secrets.get("WEATHER_API_KEY");   // throws if the owner has not added it

const res = await fetch("https://api.example.com/v1/forecast?city=Seattle", {
  method: "GET",                                     // GET, POST, PUT, PATCH, DELETE, HEAD
  headers: { authorization: `Bearer ${key}` },
  // body: { any: "object" }  → sent as JSON;  body: "text" → sent as is
});
if (!res.ok) throw new HttpError(502, "The weather service is not answering.");
const data = await res.json();                       // or await res.text()
res.status; res.headers.get("content-type");
```

- `fetch` throws when the host is not approved for this site, the request times out
  (8 seconds), the response is larger than 1 MB or is not text.
- Only `https://` URLs on the standard port, by host name. Redirects are returned to
  you (`res.status` 301/302 with a `location` header), not followed.
- `backend_access` shows which hosts are approved and which secret names exist.

## Logging and randomness

```js
console.log("saved", { id });     // also console.warn, console.error; shown by backend_logs
crypto.randomUUID();              // "3b241101-e2bb-4255-8caf-4136c566a962"
crypto.randomHex(16);             // 32 hex characters
```

## Not available

Timers, `URL`, `TextEncoder`, `Buffer`, `require`, Node and browser APIs, npm
packages, reading or writing files, environment variables.
