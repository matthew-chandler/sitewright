-- 1.7: a site can be marked as containerized: packaged, on request, as a container image
-- that runs without Sitewright.

ALTER TABLE sites ADD COLUMN containerized INTEGER NOT NULL DEFAULT 0 CHECK (containerized IN (0, 1));
