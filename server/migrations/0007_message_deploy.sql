-- What the agent may deploy while it works on this message: production, test or
-- none. The relay's guard enforces it on the developer's computer.
ALTER TABLE messages ADD COLUMN deploy_allow TEXT NOT NULL DEFAULT 'none';
