-- Access links: a producer hands one URL to a consumer. The URL is an MCP
-- server scoped to the panes (projects) and permissions the producer picked.
-- Each link has its own hidden account, so membership, approval, conversations
-- and the audit log work as for a signed-in tester. The secret is a row in
-- tokens (kind 'link', hash only).
CREATE TABLE access_links (
  id           TEXT PRIMARY KEY,
  owner_id     TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  label        TEXT NOT NULL,
  can_read     BOOLEAN NOT NULL DEFAULT true,
  can_deploys  BOOLEAN NOT NULL DEFAULT true,
  can_files    BOOLEAN NOT NULL DEFAULT true,
  auto         BOOLEAN NOT NULL DEFAULT false,
  expires_at   TIMESTAMPTZ NOT NULL,
  revoked      BOOLEAN NOT NULL DEFAULT false,
  last_used_at TIMESTAMPTZ,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX access_links_owner ON access_links(owner_id);
