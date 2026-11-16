-- 1.5: hosts a site's backend may send requests to. A backend can only ask for a host
-- ('requested'); the owner approves it in the portal.

CREATE TABLE site_hosts (
    site_id    INTEGER NOT NULL REFERENCES sites (id) ON DELETE CASCADE,
    host       TEXT    NOT NULL,
    status     TEXT    NOT NULL CHECK (status IN ('requested', 'approved')),
    -- Why the agent asked for it, shown to the owner.
    reason     TEXT,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (site_id, host)
) STRICT;
