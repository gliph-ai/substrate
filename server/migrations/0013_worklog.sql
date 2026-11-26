-- The work log (ported from Theasa): every Claude Code and Codex turn on the developer's
-- Macs, summarised on the Mac by the relay. Grouped by git repo, not by Herdr pane:
-- a project here lives as long as its turns. Diffs, images and the raw tool log stay
-- on the Mac; the cloud asks the relay for them when a turn is opened.
CREATE TABLE log_repos (
  id           BIGSERIAL PRIMARY KEY,
  owner_id     TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  key          TEXT NOT NULL,
  name         TEXT NOT NULL,
  display_name TEXT NOT NULL DEFAULT '',
  root         TEXT NOT NULL DEFAULT '',
  remote       TEXT NOT NULL DEFAULT '',
  hidden       BOOLEAN NOT NULL DEFAULT false,
  icon         BYTEA,
  icon_mime    TEXT NOT NULL DEFAULT '',
  icon_etag    TEXT NOT NULL DEFAULT '',
  -- an icon the developer uploaded: the relay's finds no longer replace it
  icon_locked  BOOLEAN NOT NULL DEFAULT false,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (owner_id, key)
);

CREATE TABLE log_turns (
  id             BIGSERIAL PRIMARY KEY,
  owner_id       TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  repo_id        BIGINT NOT NULL REFERENCES log_repos(id) ON DELETE CASCADE,
  -- the Mac that recorded it: it holds the diffs, images and tool log
  machine_id     TEXT NOT NULL DEFAULT '',
  machine_name   TEXT NOT NULL DEFAULT '',
  agent          TEXT NOT NULL,
  session_id     TEXT NOT NULL,
  turn_key       TEXT NOT NULL,
  cwd            TEXT NOT NULL DEFAULT '',
  branch         TEXT NOT NULL DEFAULT '',
  head           TEXT NOT NULL DEFAULT '',
  started_at     TIMESTAMPTZ NOT NULL,
  ended_at       TIMESTAMPTZ NOT NULL,
  duration_ms    BIGINT NOT NULL DEFAULT 0,
  tool_ms        BIGINT NOT NULL DEFAULT 0,
  prompt         TEXT NOT NULL DEFAULT '',
  final_text     TEXT NOT NULL DEFAULT '',
  model          TEXT NOT NULL DEFAULT '',
  in_tok         BIGINT NOT NULL DEFAULT 0,
  out_tok        BIGINT NOT NULL DEFAULT 0,
  cache_read     BIGINT NOT NULL DEFAULT 0,
  cache_write    BIGINT NOT NULL DEFAULT 0,
  model_calls    INT NOT NULL DEFAULT 0,
  tools          JSONB NOT NULL DEFAULT '{}',
  files          JSONB NOT NULL DEFAULT '[]',
  attachments    JSONB NOT NULL DEFAULT '[]',
  -- at API list prices; NULL when the model has no known price
  cost           DOUBLE PRECISION,
  headline       TEXT NOT NULL DEFAULT '',
  summary        JSONB NOT NULL DEFAULT '[]',
  verdict        TEXT NOT NULL DEFAULT '',
  steps          JSONB NOT NULL DEFAULT '[]',
  -- none | queued | running | done | failed | skipped
  summary_status TEXT NOT NULL DEFAULT 'none',
  summary_error  TEXT NOT NULL DEFAULT '',
  summary_model  TEXT NOT NULL DEFAULT '',
  summary_tokens BIGINT NOT NULL DEFAULT 0,
  summary_cost   DOUBLE PRECISION,
  summary_at     TIMESTAMPTZ,
  search         TSVECTOR,
  created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (owner_id, agent, session_id, turn_key)
);
CREATE INDEX log_turns_owner_time ON log_turns (owner_id, started_at DESC);
CREATE INDEX log_turns_repo_time ON log_turns (repo_id, started_at DESC);
CREATE INDEX log_turns_session ON log_turns (owner_id, session_id);
CREATE INDEX log_turns_search ON log_turns USING GIN (search);

CREATE TABLE log_decisions (
  id            BIGSERIAL PRIMARY KEY,
  turn_id       BIGINT NOT NULL REFERENCES log_turns(id) ON DELETE CASCADE,
  repo_id       BIGINT NOT NULL,
  owner_id      TEXT NOT NULL,
  text          TEXT NOT NULL,
  why           TEXT NOT NULL DEFAULT '',
  alternative   TEXT NOT NULL DEFAULT '',
  -- active | dismissed | superseded
  status        TEXT NOT NULL DEFAULT 'active',
  superseded_by BIGINT,
  created_at    TIMESTAMPTZ NOT NULL
);
CREATE INDEX log_decisions_repo ON log_decisions (repo_id, created_at DESC);
CREATE INDEX log_decisions_owner ON log_decisions (owner_id, created_at DESC);
CREATE INDEX log_decisions_turn ON log_decisions (turn_id);

CREATE TABLE log_watch (
  id         BIGSERIAL PRIMARY KEY,
  turn_id    BIGINT NOT NULL REFERENCES log_turns(id) ON DELETE CASCADE,
  repo_id    BIGINT NOT NULL,
  owner_id   TEXT NOT NULL,
  text       TEXT NOT NULL,
  -- high | medium | low
  impact     TEXT NOT NULL DEFAULT 'medium',
  -- open | done
  status     TEXT NOT NULL DEFAULT 'open',
  done_at    TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX log_watch_repo ON log_watch (repo_id, created_at DESC);
CREATE INDEX log_watch_owner ON log_watch (owner_id, status);
CREATE INDEX log_watch_turn ON log_watch (turn_id);
