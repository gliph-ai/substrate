-- "Review with Claude": the request as the developer and their agent rewrote it
-- before approving, and the line the tester gets on why it changed.
ALTER TABLE messages ADD COLUMN proposal TEXT NOT NULL DEFAULT '';
ALTER TABLE messages ADD COLUMN proposal_why TEXT NOT NULL DEFAULT '';
