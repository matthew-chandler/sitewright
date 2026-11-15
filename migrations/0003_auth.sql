-- M3: the admin user, passkeys, sessions, API tokens, enrollment invites, and LLM
-- providers added through the UI. Tokens of every kind are stored only as SHA-256 hashes.

CREATE TABLE users (
    id         INTEGER PRIMARY KEY,
    -- The WebAuthn user handle.
    uuid       TEXT    NOT NULL UNIQUE,
    name       TEXT    NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE passkeys (
    id            INTEGER PRIMARY KEY,
    user_id       INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- base64url credential id
    credential_id TEXT    NOT NULL UNIQUE,
    name          TEXT    NOT NULL,
    -- The credential's public key and counter, as JSON.
    data          TEXT    NOT NULL,
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER
) STRICT;

CREATE TABLE sessions (
    id           INTEGER PRIMARY KEY,
    token_hash   TEXT    NOT NULL UNIQUE,
    user_id      INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    csrf_token   TEXT    NOT NULL,
    user_agent   TEXT    NOT NULL DEFAULT '',
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL
) STRICT;

CREATE TABLE api_tokens (
    id           INTEGER PRIMARY KEY,
    name         TEXT    NOT NULL,
    token_hash   TEXT    NOT NULL UNIQUE,
    -- First characters of the token, to tell tokens apart in the UI.
    prefix       TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER
) STRICT;

CREATE TABLE invites (
    id         INTEGER PRIMARY KEY,
    token_hash TEXT    NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at    INTEGER
) STRICT;

-- Providers added in the UI. Non-secret settings only; keys go to secrets.toml.
CREATE TABLE providers (
    id         TEXT    PRIMARY KEY,
    config     TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
