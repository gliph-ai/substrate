-- Pushleft's Admin page (src/admin.rs): an operator can block an account, and the
-- users list puts the person seen last on top.
ALTER TABLE accounts ADD COLUMN blocked_at TIMESTAMPTZ;
ALTER TABLE accounts ADD COLUMN blocked_reason TEXT NOT NULL DEFAULT '';
ALTER TABLE accounts ADD COLUMN last_seen_at TIMESTAMPTZ;
UPDATE accounts a SET last_seen_at = (SELECT max(created_at) FROM tokens t WHERE t.account_id=a.id AND t.kind IN ('session','app'));
CREATE INDEX machines_account ON machines (account_id);

-- No credential or Mac is issued to a blocked account, whichever path asks.
-- With Block's and Unblock's cleanup this leaves no token from before a block.
-- FOR SHARE holds the account row until the insert commits, so Block's UPDATE
-- waits for an issuance in flight and its cleanup then finds that token.
CREATE FUNCTION refuse_blocked() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE b TIMESTAMPTZ;
BEGIN
  IF NEW.account_id <> '' THEN
    SELECT blocked_at INTO b FROM accounts WHERE id = NEW.account_id FOR SHARE;
    IF b IS NOT NULL THEN
      RAISE EXCEPTION 'account % is blocked', NEW.account_id USING ERRCODE = 'P0001';
    END IF;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER tokens_refuse_blocked BEFORE INSERT ON tokens FOR EACH ROW EXECUTE FUNCTION refuse_blocked();
CREATE TRIGGER machines_refuse_blocked BEFORE INSERT ON machines FOR EACH ROW EXECUTE FUNCTION refuse_blocked();

-- Each signed write from Pushleft runs once (the token's jti), until it expires.
CREATE TABLE admin_requests (
  jti        TEXT PRIMARY KEY,
  expires_at TIMESTAMPTZ NOT NULL
);

-- The same for a share link (its owner) and a pairing being approved.
CREATE FUNCTION refuse_blocked_owner() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE b TIMESTAMPTZ;
BEGIN
  SELECT blocked_at INTO b FROM accounts WHERE id = NEW.owner_id FOR SHARE;
  IF b IS NOT NULL THEN
    RAISE EXCEPTION 'account % is blocked', NEW.owner_id USING ERRCODE = 'P0001';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER access_links_refuse_blocked BEFORE INSERT ON access_links FOR EACH ROW EXECUTE FUNCTION refuse_blocked_owner();
CREATE TRIGGER pairings_refuse_blocked BEFORE INSERT OR UPDATE OF account_id ON pairings FOR EACH ROW EXECUTE FUNCTION refuse_blocked();
