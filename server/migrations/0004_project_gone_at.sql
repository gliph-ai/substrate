-- When the project's agent left (pane closed, or another agent in it) or the
-- project was unbound. Housekeeping deletes a project gone for 10 minutes
-- with nothing queued. Projects already in that state start their clock now.
ALTER TABLE projects ADD COLUMN gone_at TIMESTAMPTZ;
UPDATE projects SET gone_at = now() WHERE state IN ('needs_rebind', 'unbound');
