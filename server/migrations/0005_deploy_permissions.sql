-- What a tester may ask the agent to deploy to. The owner may always ask.
ALTER TABLE members ADD COLUMN can_deploy_test BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE members ADD COLUMN can_deploy_prod BOOLEAN NOT NULL DEFAULT false;
