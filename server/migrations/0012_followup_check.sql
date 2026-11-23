-- A follow-up sent on without its own approval: its delivery carries the fit check.
ALTER TABLE messages ADD COLUMN followup BOOLEAN NOT NULL DEFAULT false;
-- The agent's reason when it sent such a follow-up back (`substrate hold`).
ALTER TABLE messages ADD COLUMN hold_why TEXT NOT NULL DEFAULT '';
