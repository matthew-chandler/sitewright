-- 1.12: background work for site backends. One row per queued job (`jobs.enqueue`) and
-- one per run of a scheduled task (`jobs.schedule`), kept for a week after it ends.

CREATE TABLE site_jobs (
    id           INTEGER PRIMARY KEY,
    site_id      INTEGER NOT NULL REFERENCES sites (id) ON DELETE CASCADE,
    kind         TEXT    NOT NULL CHECK (kind IN ('job', 'schedule')),
    name         TEXT    NOT NULL,
    -- JSON. `null` for scheduled runs.
    payload      TEXT    NOT NULL DEFAULT 'null',
    status       TEXT    NOT NULL CHECK (status IN ('queued', 'running', 'done', 'failed')),
    -- Unix milliseconds: when the job may run next.
    run_at       INTEGER NOT NULL,
    attempts     INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 1,
    last_error   TEXT,
    created_at   INTEGER NOT NULL,
    finished_at  INTEGER
) STRICT;

CREATE INDEX site_jobs_due ON site_jobs (status, run_at);
CREATE INDEX site_jobs_site ON site_jobs (site_id, id);
