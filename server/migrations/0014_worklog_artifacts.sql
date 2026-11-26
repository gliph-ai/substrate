-- Finds the earlier publishes of an artifact by its URL at ingest, without a scan of every turn.
CREATE INDEX log_turns_attachments ON log_turns USING GIN (attachments jsonb_path_ops);
