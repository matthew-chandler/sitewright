# Metrics, uptime and dashboards

All numbers come from deterministic code. The AI only decides which charts fit a site, by
writing queries and chart descriptions that the server validates and runs.

## What is collected

| Source | What | Kept |
| --- | --- | --- |
| **Request log** | Every request to a site: time, method, path (no query string), status, bytes, duration, referring hostname, a browser/bot/other class, and a visitor hash | 30 days |
| **Event beacon** | Events sent by a site's pages: a `pageview` on load (with the referring hostname and a small/medium/large screen class) and any custom events the page sends | 30 days |
| **Uptime checks** | Every 60 seconds, a fetch of each site's public front page through real DNS and TLS: success, latency, HTTP status, days until the certificate expires | 30 days |
| **Daily rollups** | Per site and day: page views, unique visitors, errors, 95th-percentile response time, uptime percentage | forever |

Raw rows older than `metrics.retention_days` are deleted hourly, after their day has been
rolled up.

### Privacy

- **No IP addresses are stored**, anywhere. Visitors are told apart by a hash of IP
  address, user agent and site, salted with a random key that is replaced every UTC day.
  The same visitor has the same hash within a day (so unique visitors can be counted) and
  an unrelated one the next day; once the salt is gone the hash cannot be traced back.
- **No cookies and no browser storage.** The beacon script sets nothing on the device.
- The beacon stays silent for browsers that send **Do Not Track** or **Global Privacy
  Control**. (The server-side request log still counts the page view, without any
  identifier beyond the daily hash.)
- Query strings are never logged. Referrers are reduced to a hostname.
- Events are validated strictly: a short name, a path, and at most ten short scalar
  properties. Nothing else is accepted.

## The beacon

Every site host serves a script of under 1 KB at `/_sw/a.js` and accepts events at
`POST /_sw/e`. A page opts in with one line (the `create-site` skill adds it):

```html
<script src="/_sw/a.js" defer></script>
```

Custom events:

```js
if (window.sw) sw("search", { q: query, results: count });
```

Event names are lowercase letters, digits, `_`, `-` and `.` (at most 40 characters).
Bodies are limited to 2 KB and each visitor to 60 events a minute. Requests to `/_sw/`
are not themselves counted as traffic.

## Site status

| Status | Meaning |
| --- | --- |
| **Up** | The latest check succeeded, quickly, with a healthy certificate |
| **Degraded** | The latest check failed (once), or took longer than `metrics.slow_ms`, or the certificate expires in under 14 days |
| **Down** | Two checks in a row failed |
| **Not checked yet** | Published, but no check has run |

The home screen shows each site's status, 30-day uptime, a 7-day page-view sparkline and
its last deploy.

## Dashboards

Each site has two:

- the **overview**, the same for every site: page views per day, unique visitors, top
  pages, responses by status, 95th-percentile response time, uptime;
- panels **for this site**, designed by the agent with the `design-dashboard` skill when
  the site is created, after edits that change what visitors can do, and on **Regenerate**.

Both are made of the same thing: panels with a SQL query and a Vega-Lite chart. Every
panel shows its SQL on demand.

A panel reads from one of two places, chosen with its optional `source` field:
`"metrics"` (the default) is the site's traffic, through the [metric views](#metric-views);
`"site"` is the site's own backend database, so a guestbook can chart its entries per day
next to its page views. See [Panels over the site's own data](#panels-over-the-sites-own-data).

### Dashboard spec

```json
{
  "version": 1,
  "panels": [
    {
      "id": "trail-searches",
      "title": "Trail searches per day",
      "description": "How often visitors search the trail list",
      "sql": "SELECT day, count(*) AS n FROM v_events WHERE name = 'search' AND ts BETWEEN :from AND :to GROUP BY day",
      "vega_lite": {
        "mark": "line",
        "encoding": { "x": {"field": "day", "type": "temporal"}, "y": {"field": "n", "type": "quantitative"} }
      }
    }
  ]
}
```

JSON Schema:

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "Sitewright dashboard",
  "type": "object",
  "additionalProperties": false,
  "required": ["version", "panels"],
  "properties": {
    "version": { "const": 1 },
    "panels": {
      "type": "array",
      "minItems": 1,
      "maxItems": 12,
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["id", "title", "sql", "vega_lite"],
        "properties": {
          "id": { "type": "string", "pattern": "^[a-z0-9]([a-z0-9-]{0,38}[a-z0-9])?$" },
          "title": { "type": "string", "minLength": 1, "maxLength": 80 },
          "description": { "type": "string", "maxLength": 300 },
          "source": { "enum": ["metrics", "site"], "default": "metrics" },
          "sql": { "type": "string" },
          "vega_lite": { "type": "object" }
        }
      }
    }
  }
}
```

Validation before a spec is saved:

- Unknown fields are rejected, at the top level and in panels. At most 12 panels; ids are
  unique.
- `vega_lite` may use only `mark`, `encoding`, `layer`, `transform`, `title`,
  `description`, `width`, `height`, `config` and `resolve`. Anywhere inside it, `data`,
  `datasets`, `url`, `href`, `lookup`, `from`, `usermeta` and `$schema` are rejected, as is
  any string that is a URL. The server injects the query's rows as the chart's data.
- Every query is **run** where its panel's `source` says: in the sandbox below, or
  read-only on the site's own database. If it fails, or the chart refers to a column the
  query does not return, the spec is refused with the reasons.

### Metric views

Queries read four views. `ts` is ISO-8601 UTC text, so `date(ts)` and comparisons with
`:from` and `:to` work directly.

| View | Columns |
| --- | --- |
| `v_requests` | `ts`, `day`, `hour`, `method`, `path`, `status`, `bytes`, `duration_ms`, `referrer_host`, `ua_class`, `visitor`, `is_page_view` |
| `v_events` | `ts`, `day`, `name`, `path`, `props` (JSON text), `visitor` |
| `v_checks` | `ts`, `day`, `ok`, `latency_ms`, `status`, `cert_days` |
| `v_daily` | `day`, `views`, `visitors`, `errors`, `p95_ms`, `uptime_pct` |

`is_page_view` is 1 for a successful `GET` of a page (not an asset) by something that is
not a bot. Parameters filled in by the server: `:site_id`, `:from`, `:to`.

### The SQL sandbox

Agent-written SQL is untrusted. It runs on a separate, read-only connection with four
independent limits (`crates/sw-metrics/src/sandbox.rs`):

1. **An authorizer**, SQLite's compile-time access hook, allows only `SELECT`, only an
   allowlist of ordinary functions, and only reads that go through the metric views. Other
   tables (`sessions`, `api_tokens`, `settings`, `sqlite_master`) cannot be named;
   `PRAGMA`, `ATTACH`, writes and DDL are refused.
2. **Site scoping by construction.** The views are temporary views on that connection
   which filter on a scope set by the server before each query. A query sees one site's
   rows even if it omits `WHERE site_id = :site_id` or asks for another site's id.
3. **A 5-second time limit**, enforced with a progress handler.
4. **A 1000-row limit.**

### Panels over the site's own data

A panel with `"source": "site"` runs its query on the database of the site's
[backend](backends.md) instead of the metric views:

```json
{
  "id": "entries",
  "title": "Guestbook entries per day",
  "source": "site",
  "sql": "SELECT date(created_at / 1000, 'unixepoch') AS day, count(*) AS n FROM entries WHERE created_at BETWEEN :from AND :to GROUP BY day ORDER BY day",
  "vega_lite": {
    "mark": "bar",
    "encoding": { "x": {"field": "day", "type": "temporal"}, "y": {"field": "n", "type": "quantitative"} }
  }
}
```

- `:from` and `:to` are the selected time range as **Unix milliseconds** (integers, bound
  as parameters), because site tables conventionally store `Date.now()`. `:site_id` and
  `?` placeholders are refused: the database is already the site's own.
- One `SELECT` (or `WITH`), at most 4000 bytes, 5 seconds and 1000 rows.
- The query runs under the site database's authorizer in its read-only mode
  (`crates/sw-backend/src/sitedb.rs`): no writes, no schema changes, no `ATTACH` or
  `PRAGMA`. Visitor sessions (`sw_sessions`) and password hashes cannot be read;
  `sw_users` gives `id`, `email` and `created_at` only.
- It reaches that site's database file and nothing else: not Sitewright's own database
  (so not the metric views either, one panel reads one source) and not another site's.
- A site with no backend database shows an error on such a panel; the other panels are
  unaffected. Nothing is created by asking.
- Rows the agent sees through `run_readonly_query` have the site's secret values
  replaced, like the rest of a backend's output. The owner's dashboard shows rows as
  they are.
- The query uses the same connection as the site's request handlers, one statement at a
  time, so a slow panel query can make a visitor's request wait (for at most the
  5-second limit).

Charts are drawn in the browser with vendored Vega-Lite using its expression interpreter,
so the admin UI's Content-Security-Policy does not need `unsafe-eval`.

## Configuration

See `[metrics]` in [configuration.md](configuration.md).
