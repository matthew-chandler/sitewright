---
name: design-dashboard
description: Design the analytics dashboard for a site - look at what the site actually does, pick the few metrics that matter for it, write a read-only SQL query and a Vega-Lite chart for each, test them, and save the dashboard. Use after creating a site, after an edit that changes what visitors can do, or when the user asks for a dashboard, analytics, stats or metrics.
license: MIT OR Apache-2.0
metadata:
  author: sitewright
  version: "1.0"
---

# Design a dashboard

Every site already has a standard dashboard: page views, visitors, top pages, status
codes, response time and uptime. Your job is the part only you can do: panels that are
**specific to this site**. A recipe site wants "most viewed recipes" and "searches with no
results"; a trail log wants "most viewed trails" and "which filters people use".

Do not repeat the standard panels. Three to six good panels beat twelve generic ones.

## Steps

1. **Understand the site.** Look at the file list; `read_file` the home page and any page
   with interactions (search, filters, forms, buttons). Note:
   - the sections of the site (path prefixes such as `/trails/`, `/recipes/`),
   - custom analytics events the pages send: look for `sw("name", {...})` calls.
2. **Call `metrics_schema`** for the views, columns, parameters and the dashboard format.
   If the site has a backend (files under `backend/`), also call `backend_schema` for the
   tables of its own database: what the site stores is often its most telling number.
3. **Explore** with `run_readonly_query`. Check what data exists before designing around
   it: which paths get views, which event names occur, what their `props` contain. A new
   site has little or no data; that is fine, design for the data it *will* have.
4. **Design the panels.** For each one decide the question it answers, write the SQL, and
   test it with `run_readonly_query`.
5. **Call `save_dashboard`** with the whole spec. It validates the spec and runs every
   query; if it returns problems, fix them and call it again.
6. **Tell the user** which panels you chose and why, in a short list.

## Writing the SQL

- One `SELECT` per panel over `v_requests`, `v_events`, `v_checks` or `v_daily`.
- Always limit the time range: `ts BETWEEN :from AND :to` (for `v_daily`:
  `day BETWEEN date(:from) AND date(:to)`). The user picks the range; `:from` and `:to`
  are filled in for them.
- Count people's page views with `is_page_view` (it excludes bots, assets and errors):
  `sum(is_page_view)` or `WHERE is_page_view`.
- Give every column a clear alias; the chart refers to columns by these names.
- Keep results small: group by day, or `ORDER BY ... DESC LIMIT 10` for rankings.
- Read event properties with `json_extract(props, '$.key')`.

Useful shapes:

```sql
-- Views of one section per day
SELECT day, sum(is_page_view) AS views
FROM v_requests
WHERE ts BETWEEN :from AND :to AND path LIKE '/trails/%'
GROUP BY day ORDER BY day

-- Ranking within a section
SELECT path, count(*) AS views
FROM v_requests
WHERE ts BETWEEN :from AND :to AND is_page_view AND path LIKE '/trails/%'
GROUP BY path ORDER BY views DESC LIMIT 10

-- A custom event per day
SELECT day, count(*) AS searches
FROM v_events
WHERE ts BETWEEN :from AND :to AND name = 'search'
GROUP BY day ORDER BY day

-- The most common value of an event property
SELECT json_extract(props, '$.q') AS query, count(*) AS times
FROM v_events
WHERE ts BETWEEN :from AND :to AND name = 'search' AND json_extract(props, '$.q') <> ''
GROUP BY query ORDER BY times DESC LIMIT 10

-- Where visitors come from
SELECT coalesce(referrer_host, 'direct') AS source, count(*) AS views
FROM v_requests
WHERE ts BETWEEN :from AND :to AND is_page_view
GROUP BY source ORDER BY views DESC LIMIT 10
```

## Panels over the site's own data

A site with a backend has its own database: guestbook entries, sign-ups, orders, votes.
A panel with `"source": "site"` runs its query there instead of on the metric views, so
the dashboard can show what visitors *did* next to how many came.

```json
{
  "id": "entries",
  "title": "Guestbook entries per day",
  "description": "How many visitors signed the guestbook.",
  "source": "site",
  "sql": "SELECT date(created_at / 1000, 'unixepoch') AS day, count(*) AS n FROM entries WHERE created_at BETWEEN :from AND :to GROUP BY day ORDER BY day",
  "vega_lite": {
    "mark": "bar",
    "encoding": {
      "x": {"field": "day", "type": "temporal", "title": null},
      "y": {"field": "n", "type": "quantitative", "title": "Entries"}
    }
  }
}
```

- Test such a query with `run_readonly_query` and `"source": "site"`.
- Here `:from` and `:to` are **Unix milliseconds**, matching columns filled with
  `Date.now()`. Check in `backend_schema` how a table stores its time before filtering
  on it. There is no `:site_id`.
- One panel reads one source: a site query cannot use `v_requests` and the other views,
  and a metrics query cannot use the site's tables.
- Read-only. `sw_users` (visitor accounts) gives `id`, `email` and `created_at` only, so
  name the columns instead of `SELECT *`; sessions cannot be read.
- Chart counts and totals, never people: no emails, names or message texts in a panel.
- Only the published database counts. A table from a migration that is not published yet
  does not exist here, and a site without a published backend has no database: leave
  these panels out until it does.

## Writing the chart

`vega_lite` is a Vega-Lite spec **without `data`**: the server injects the query's rows.
Allowed top-level properties: `mark`, `encoding`, `layer`, `transform`, `title`,
`description`, `width`, `height`, `config`, `resolve`. URLs, `data`, `href` and `$schema`
are rejected. Leave width and height out; the page sizes charts to fit.

| Data | Chart |
| --- | --- |
| A number per day | `"mark": "line"` or `"bar"`, `x` = the day (`"type": "temporal"`), `y` = the number (`"quantitative"`) |
| A ranking (top pages, top queries) | `"mark": "bar"`, `y` = the name (`"nominal"`, `"sort": "-x"`), `x` = the count |
| Shares of a whole (few categories) | `"mark": "bar"` with `x` = category; avoid pie charts |
| Two series over time | `x` = day, `y` = value, `"color"` = series name (`"nominal"`) |

```json
{
  "id": "top-trails",
  "title": "Most viewed trails",
  "description": "Which trail pages visitors open most.",
  "sql": "SELECT path, count(*) AS views FROM v_requests WHERE ts BETWEEN :from AND :to AND is_page_view AND path LIKE '/trails/%' GROUP BY path ORDER BY views DESC LIMIT 10",
  "vega_lite": {
    "mark": "bar",
    "encoding": {
      "y": {"field": "path", "type": "nominal", "sort": "-x", "title": null},
      "x": {"field": "views", "type": "quantitative", "title": "Views"}
    }
  }
}
```

## The whole spec

```json
{ "version": 1, "panels": [ { "id": "...", "title": "...", "description": "...", "sql": "...", "vega_lite": { } } ] }
```

- `source`: optional; leave it out for the metric views, `"site"` for the site's own
  database.

- `id`: lowercase letters, digits and hyphens; unique.
- `title`: what the panel shows, in plain words. `description`: one sentence on how to
  read it or why it matters.
- At most 12 panels.

## Custom events

Pages that include `<script src="/_sw/a.js" defer></script>` send a `pageview` event
automatically and can send custom ones with `sw("name", {key: value})`. If the site has an
interaction worth measuring and does not record it yet, you may add the call while you are
editing the page (see the create-site and edit-site skills), then build a panel on it.
Event names are lowercase letters, digits, `_`, `-` and `.`; properties are a few short
scalar values. Never put personal data (names, emails, free-text messages) in an event.
