CREATE INDEX IF NOT EXISTS sessions_creation_id ON sessions(creation_time, id);
CREATE INDEX IF NOT EXISTS tasks_session_creation_id ON tasks(ssn_id, creation_time, id);
CREATE INDEX IF NOT EXISTS tasks_session_state_id ON tasks(ssn_id, state, id);
