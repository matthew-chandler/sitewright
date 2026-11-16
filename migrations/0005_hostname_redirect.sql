-- 1.2: a hostname can redirect to the site's primary hostname instead of serving the
-- site. Used for the old address after a site is renamed.

ALTER TABLE site_hostnames ADD COLUMN redirect INTEGER NOT NULL DEFAULT 0 CHECK (redirect IN (0, 1));
