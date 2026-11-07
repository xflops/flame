-- Support per-session task history and recovery queries.
CREATE INDEX IF NOT EXISTS tasks_session_id ON tasks (ssn_id, id);
