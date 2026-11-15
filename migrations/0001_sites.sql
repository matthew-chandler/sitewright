-- M1: sites, hostnames, releases, request log, settings, audit log.
-- Timestamps are Unix milliseconds (UTC).

CREATE TABLE sites (
    id              INTEGER PRIMARY KEY,
    name            TEXT    NOT NULL UNIQUE,
    title           TEXT    NOT NULL,
    status          TEXT    NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'deleted')),
    current_release INTEGER REFERENCES releases (id),
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    deleted_at      INTEGER
) STRICT;

CREATE TABLE site_hostnames (
    id         INTEGER PRIMARY KEY,
    site_id    INTEGER NOT NULL REFERENCES sites (id) ON DELETE CASCADE,
    hostname   TEXT    NOT NULL UNIQUE,
    is_primary INTEGER NOT NULL DEFAULT 0 CHECK (is_primary IN (0, 1)),
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX site_hostnames_site ON site_hostnames (site_id);

CREATE TABLE releases (
    id         INTEGER PRIMARY KEY,
    site_id    INTEGER NOT NULL REFERENCES sites (id) ON DELETE CASCADE,
    sha        TEXT    NOT NULL,
    message    TEXT    NOT NULL,
    author     TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (site_id, sha)
) STRICT;

CREATE INDEX releases_site_created ON releases (site_id, id);

CREATE TABLE requests (
    id            INTEGER PRIMARY KEY,
    ts            INTEGER NOT NULL,
    site_id       INTEGER NOT NULL,
    method        TEXT    NOT NULL,
    path          TEXT    NOT NULL,
    status        INTEGER NOT NULL,
    bytes         INTEGER NOT NULL,
    duration_ms   INTEGER NOT NULL,
    referrer_host TEXT,
    ua_class      TEXT    NOT NULL,
    visitor_hash  TEXT    NOT NULL
) STRICT;

CREATE INDEX requests_site_ts ON requests (site_id, ts);
CREATE INDEX requests_ts ON requests (ts);

CREATE TABLE settings (
    key        TEXT    PRIMARY KEY,
    value      TEXT    NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE audit_log (
    id     INTEGER PRIMARY KEY,
    ts     INTEGER NOT NULL,
    actor  TEXT    NOT NULL,
    action TEXT    NOT NULL,
    target TEXT,
    detail TEXT
) STRICT;

CREATE INDEX audit_log_ts ON audit_log (ts);
