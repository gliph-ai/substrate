-- Sidra's APNs tokens: a push per request that waits for the developer's approval.
CREATE TABLE devices (
    token TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    bundle TEXT NOT NULL,
    sandbox BOOLEAN NOT NULL DEFAULT false,
    client_ref TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX devices_account ON devices (account_id);
