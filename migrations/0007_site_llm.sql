-- 1.6: per-site access to AI models for backends. Off until the owner turns it on.

CREATE TABLE site_llm (
    site_id          INTEGER PRIMARY KEY REFERENCES sites (id) ON DELETE CASCADE,
    enabled          INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    -- JSON array of model names (provider ids) the site may use. Empty: the default model.
    models           TEXT    NOT NULL DEFAULT '[]',
    monthly_requests INTEGER NOT NULL DEFAULT 1000,
    per_minute       INTEGER NOT NULL DEFAULT 20,
    updated_at       INTEGER NOT NULL
) STRICT;

CREATE TABLE site_llm_usage (
    id            INTEGER PRIMARY KEY,
    site_id       INTEGER NOT NULL REFERENCES sites (id) ON DELETE CASCADE,
    ts            INTEGER NOT NULL,
    -- The name the backend asked for, and the model behind it at the time.
    model_name    TEXT    NOT NULL,
    model         TEXT    NOT NULL,
    input_tokens  INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    -- NULL when no price is configured for the model.
    cost_usd      REAL,
    ok            INTEGER NOT NULL CHECK (ok IN (0, 1))
) STRICT;

CREATE INDEX site_llm_usage_site_ts ON site_llm_usage (site_id, ts);
