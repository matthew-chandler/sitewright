-- M2: agent runs and their replayable event streams.

CREATE TABLE agent_runs (
    id            INTEGER PRIMARY KEY,
    site_name     TEXT    NOT NULL,
    prompt        TEXT    NOT NULL,
    status        TEXT    NOT NULL CHECK (status IN ('running', 'succeeded', 'failed', 'cancelled', 'limit')),
    provider      TEXT    NOT NULL,
    model         TEXT    NOT NULL,
    turns         INTEGER NOT NULL DEFAULT 0,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    -- NULL when no price is configured for the model.
    cost_usd      REAL,
    -- Human-readable outcome: the error, or which limit was hit.
    message       TEXT,
    created_at    INTEGER NOT NULL,
    finished_at   INTEGER
) STRICT;

CREATE INDEX agent_runs_site ON agent_runs (site_name, id);
CREATE INDEX agent_runs_created ON agent_runs (created_at);

CREATE TABLE agent_events (
    id     INTEGER PRIMARY KEY,
    run_id INTEGER NOT NULL REFERENCES agent_runs (id) ON DELETE CASCADE,
    seq    INTEGER NOT NULL,
    ts     INTEGER NOT NULL,
    -- JSON: {"type": "...", ...}
    data   TEXT    NOT NULL,
    UNIQUE (run_id, seq)
) STRICT;
