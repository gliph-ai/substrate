-- The account's own picture, shown next to its name (testers see the developer's).
ALTER TABLE accounts ADD COLUMN avatar BYTEA;
ALTER TABLE accounts ADD COLUMN avatar_mime TEXT NOT NULL DEFAULT '';
ALTER TABLE accounts ADD COLUMN avatar_etag TEXT NOT NULL DEFAULT '';
