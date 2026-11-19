-- A project's own logo, found by the relay in the project's folder (the same
-- candidates as Sidra's tab icons) and shown wherever the project appears.
ALTER TABLE projects ADD COLUMN icon BYTEA;
ALTER TABLE projects ADD COLUMN icon_mime TEXT NOT NULL DEFAULT '';
ALTER TABLE projects ADD COLUMN icon_etag TEXT NOT NULL DEFAULT '';
