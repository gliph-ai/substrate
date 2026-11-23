-- The developer approved this conversation: its follow-ups skip approval until a rejection.
ALTER TABLE conversations ADD COLUMN approved BOOLEAN NOT NULL DEFAULT false;
