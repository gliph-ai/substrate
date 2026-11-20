-- The daily trim deletes audit entries by age (180 days).
CREATE INDEX audit_created ON audit(created_at);
