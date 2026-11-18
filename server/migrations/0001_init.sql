-- Substrate cloud schema (docs/substrate.md). PostgreSQL.

CREATE TABLE accounts (
  id            TEXT PRIMARY KEY,
  email         TEXT UNIQUE NOT NULL,
  name          TEXT NOT NULL DEFAULT '',
  gliph_user_id TEXT NOT NULL DEFAULT '',
  gliph_nick    TEXT NOT NULL DEFAULT '',
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX accounts_gliph_nick ON accounts(gliph_nick) WHERE gliph_nick <> '';

-- Every credential, stored as a SHA-256 digest only.
CREATE TABLE tokens (
  hash        TEXT PRIMARY KEY,
  kind        TEXT NOT NULL,
  account_id  TEXT NOT NULL DEFAULT '',
  machine_id  TEXT NOT NULL DEFAULT '',
  client_id   TEXT NOT NULL DEFAULT '',
  ref         TEXT NOT NULL DEFAULT '',
  extra       TEXT NOT NULL DEFAULT '',
  expires_at  TIMESTAMPTZ NOT NULL,
  used        BOOLEAN NOT NULL DEFAULT false,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX tokens_machine ON tokens(machine_id) WHERE machine_id <> '';
CREATE INDEX tokens_account ON tokens(account_id, kind);

CREATE TABLE pairings (
  device_hash  TEXT PRIMARY KEY,
  user_code    TEXT UNIQUE NOT NULL,
  machine_name TEXT NOT NULL,
  platform     TEXT NOT NULL DEFAULT '',
  version      TEXT NOT NULL DEFAULT '',
  account_id   TEXT NOT NULL DEFAULT '',
  status       TEXT NOT NULL DEFAULT 'pending',
  expires_at   TIMESTAMPTZ NOT NULL
);

CREATE TABLE machines (
  id          TEXT PRIMARY KEY,
  account_id  TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  platform    TEXT NOT NULL DEFAULT '',
  version     TEXT NOT NULL DEFAULT '',
  panes       JSONB NOT NULL DEFAULT '[]',
  last_seen   TIMESTAMPTZ,
  revoked     BOOLEAN NOT NULL DEFAULT false,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE projects (
  id            TEXT PRIMARY KEY,
  owner_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  handle        TEXT NOT NULL,
  name          TEXT NOT NULL,
  description   TEXT NOT NULL DEFAULT '',
  machine_id    TEXT NOT NULL DEFAULT '',
  pane_id       TEXT NOT NULL DEFAULT '',
  agent_session TEXT NOT NULL DEFAULT '',
  agent         TEXT NOT NULL DEFAULT '',
  cwd           TEXT NOT NULL DEFAULT '',
  state         TEXT NOT NULL DEFAULT 'unbound',
  mode          TEXT NOT NULL DEFAULT 'approve',
  auto_until    TIMESTAMPTZ,
  retest_hosts  TEXT NOT NULL DEFAULT '',
  ci_public     BOOLEAN NOT NULL DEFAULT false,
  gliph_channel TEXT NOT NULL DEFAULT '',
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (owner_id, handle)
);

CREATE TABLE members (
  project_id   TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  role         TEXT NOT NULL DEFAULT 'tester',
  trusted_auto BOOLEAN NOT NULL DEFAULT false,
  invite_id    TEXT NOT NULL DEFAULT '',
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (project_id, account_id)
);

CREATE TABLE invites (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  token_hash  TEXT UNIQUE NOT NULL,
  label       TEXT NOT NULL DEFAULT '',
  max_uses    INT NOT NULL DEFAULT 1,
  uses        INT NOT NULL DEFAULT 0,
  revoked     BOOLEAN NOT NULL DEFAULT false,
  created_by  TEXT NOT NULL,
  expires_at  TIMESTAMPTZ NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE clients (
  id            TEXT PRIMARY KEY,
  name          TEXT NOT NULL,
  redirect_uris JSONB NOT NULL,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE conversations (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  sender_id   TEXT NOT NULL,
  title       TEXT NOT NULL DEFAULT '',
  last_seq    BIGINT NOT NULL DEFAULT 0,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX conversations_project ON conversations(project_id, updated_at DESC);

CREATE SEQUENCE message_order;
CREATE TABLE messages (
  id               TEXT PRIMARY KEY,
  conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  project_id       TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  sender_id        TEXT NOT NULL,
  sender_label     TEXT NOT NULL DEFAULT '',
  client_name      TEXT NOT NULL DEFAULT '',
  kind             TEXT NOT NULL DEFAULT 'feedback',
  text             TEXT NOT NULL,
  envelope         TEXT NOT NULL DEFAULT '',
  idem_key         TEXT NOT NULL DEFAULT '',
  stage            TEXT NOT NULL,
  deploy_stage     TEXT NOT NULL DEFAULT '',
  deploy           JSONB NOT NULL DEFAULT '{}',
  risk             TEXT NOT NULL DEFAULT '',
  reply_hash       TEXT NOT NULL DEFAULT '',
  delivery_id      TEXT NOT NULL DEFAULT '',
  agent_idle       BOOLEAN NOT NULL DEFAULT false,
  question         TEXT NOT NULL DEFAULT '',
  excerpt          TEXT NOT NULL DEFAULT '',
  order_n          BIGINT NOT NULL DEFAULT nextval('message_order'),
  created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
  expires_at       TIMESTAMPTZ NOT NULL
);
CREATE INDEX messages_queue ON messages(project_id, stage, order_n);
CREATE INDEX messages_conv ON messages(conversation_id);
CREATE INDEX messages_delivery ON messages(delivery_id) WHERE delivery_id <> '';
CREATE UNIQUE INDEX messages_idem ON messages(sender_id, idem_key) WHERE idem_key <> '';

CREATE TABLE events (
  conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  seq             BIGINT NOT NULL,
  message_id      TEXT NOT NULL DEFAULT '',
  actor_kind      TEXT NOT NULL,
  actor_name      TEXT NOT NULL DEFAULT '',
  kind            TEXT NOT NULL,
  text            TEXT NOT NULL DEFAULT '',
  data            JSONB NOT NULL DEFAULT '{}',
  held            BOOLEAN NOT NULL DEFAULT false,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (conversation_id, seq)
);

CREATE TABLE attachments (
  id              TEXT PRIMARY KEY,
  project_id      TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  conversation_id TEXT NOT NULL DEFAULT '',
  message_id      TEXT NOT NULL DEFAULT '',
  uploader_id     TEXT NOT NULL,
  origin          TEXT NOT NULL,
  name            TEXT NOT NULL,
  mime            TEXT NOT NULL,
  size            BIGINT NOT NULL,
  sha256          TEXT NOT NULL,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX attachments_message ON attachments(message_id);

CREATE TABLE audit (
  id          TEXT PRIMARY KEY,
  account_id  TEXT NOT NULL DEFAULT '',
  project_id  TEXT NOT NULL DEFAULT '',
  actor       TEXT NOT NULL,
  action      TEXT NOT NULL,
  target      TEXT NOT NULL DEFAULT '',
  detail      TEXT NOT NULL DEFAULT '',
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX audit_project ON audit(project_id, created_at DESC);
CREATE INDEX audit_account ON audit(account_id, created_at DESC);
