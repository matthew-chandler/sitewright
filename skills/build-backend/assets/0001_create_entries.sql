CREATE TABLE entries (
    id         INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL,
    message    TEXT    NOT NULL,
    visitor    TEXT    NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX entries_visitor_created ON entries (visitor, created_at);
