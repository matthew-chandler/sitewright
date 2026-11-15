-- M5: beacon events, uptime checks, daily rollups and per-site AI dashboards.

CREATE TABLE events (
    id           INTEGER PRIMARY KEY,
    ts           INTEGER NOT NULL,
    site_id      INTEGER NOT NULL,
    name         TEXT    NOT NULL,
    path         TEXT    NOT NULL DEFAULT '',
    -- JSON object of small scalar values.
    props        TEXT    NOT NULL DEFAULT '{}',
    visitor_hash TEXT    NOT NULL
) STRICT;

CREATE INDEX events_site_ts ON events (site_id, ts);
CREATE INDEX events_ts ON events (ts);

CREATE TABLE checks (
    id         INTEGER PRIMARY KEY,
    ts         INTEGER NOT NULL,
    site_id    INTEGER NOT NULL,
    kind       TEXT    NOT NULL DEFAULT 'uptime',
    ok         INTEGER NOT NULL CHECK (ok IN (0, 1)),
    latency_ms INTEGER NOT NULL,
    -- HTTP status, when a response arrived.
    status     INTEGER,
    -- Days until the TLS certificate expires, for HTTPS sites.
    cert_days  INTEGER,
    detail     TEXT    NOT NULL DEFAULT ''
) STRICT;

CREATE INDEX checks_site_ts ON checks (site_id, ts);
CREATE INDEX checks_ts ON checks (ts);

CREATE TABLE daily_rollups (
    site_id    INTEGER NOT NULL,
    -- UTC day, YYYY-MM-DD.
    day        TEXT    NOT NULL,
    views      INTEGER NOT NULL,
    visitors   INTEGER NOT NULL,
    errors     INTEGER NOT NULL,
    p95_ms     INTEGER NOT NULL,
    -- NULL when no uptime check ran that day.
    uptime_pct REAL,
    PRIMARY KEY (site_id, day)
) STRICT;

CREATE TABLE dashboards (
    site_id    INTEGER PRIMARY KEY REFERENCES sites (id) ON DELETE CASCADE,
    -- The validated dashboard spec, as JSON.
    spec       TEXT    NOT NULL,
    version    INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
