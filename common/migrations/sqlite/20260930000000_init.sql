-- Initialize a fresh Flame database with workspace-scoped resource names.
CREATE TABLE workspaces (name TEXT PRIMARY KEY, create_at INTEGER NOT NULL);
INSERT INTO workspaces VALUES ('default', CAST(strftime('%s','now') AS INTEGER));

CREATE TABLE applications (
  id TEXT NOT NULL, workspace TEXT NOT NULL, name TEXT NOT NULL,
  version INTEGER NOT NULL, shim INTEGER NOT NULL, image TEXT, description TEXT,
  labels TEXT, command TEXT, arguments TEXT, environments TEXT,
  working_directory TEXT, max_instances INTEGER NOT NULL, delay_release INTEGER NOT NULL,
  schema TEXT, url TEXT, installer TEXT, creation_time INTEGER NOT NULL,
  state INTEGER NOT NULL, PRIMARY KEY(workspace,name),
  FOREIGN KEY(workspace) REFERENCES workspaces(name)
);

CREATE TABLE sessions (
  id TEXT NOT NULL, workspace TEXT NOT NULL, name TEXT NOT NULL,
  application TEXT NOT NULL, version INTEGER NOT NULL, common_data BLOB, tokens TEXT NOT NULL,
  creation_time INTEGER NOT NULL, completion_time INTEGER, state INTEGER NOT NULL,
  min_instances INTEGER NOT NULL, max_instances INTEGER, batch_size INTEGER NOT NULL,
  priority INTEGER NOT NULL, resreq_cpu INTEGER, resreq_memory INTEGER, resreq_gpu INTEGER,
  PRIMARY KEY(workspace,name),
  FOREIGN KEY(workspace,application) REFERENCES applications(workspace,name)
);

CREATE TABLE tasks (
  id TEXT NOT NULL, workspace TEXT NOT NULL, session TEXT NOT NULL, name TEXT NOT NULL,
  version INTEGER NOT NULL, input BLOB, output BLOB, affinity TEXT,
  creation_time INTEGER NOT NULL, completion_time INTEGER, state INTEGER NOT NULL,
  PRIMARY KEY(workspace,session,name),
  FOREIGN KEY(workspace,session) REFERENCES sessions(workspace,name) ON DELETE CASCADE
);

CREATE TABLE nodes (
  id TEXT NOT NULL, name TEXT PRIMARY KEY, state INTEGER NOT NULL DEFAULT 0,
  capacity_cpu INTEGER NOT NULL DEFAULT 0, capacity_memory INTEGER NOT NULL DEFAULT 0,
  capacity_gpu INTEGER NOT NULL DEFAULT 0, allocatable_cpu INTEGER NOT NULL DEFAULT 0,
  allocatable_memory INTEGER NOT NULL DEFAULT 0, allocatable_gpu INTEGER NOT NULL DEFAULT 0,
  info_arch TEXT NOT NULL DEFAULT '', info_os TEXT NOT NULL DEFAULT '',
  creation_time INTEGER NOT NULL, last_heartbeat INTEGER NOT NULL
);

CREATE TABLE executors (
  id TEXT NOT NULL, workspace TEXT NOT NULL, name TEXT NOT NULL,
  node TEXT NOT NULL, application TEXT NOT NULL, resreq_cpu INTEGER NOT NULL,
  resreq_memory INTEGER NOT NULL, resreq_gpu INTEGER NOT NULL, shim INTEGER NOT NULL,
  task TEXT, session TEXT, creation_time INTEGER NOT NULL, state INTEGER NOT NULL,
  PRIMARY KEY(workspace,name),
  FOREIGN KEY(node) REFERENCES nodes(name) ON DELETE CASCADE
);

CREATE INDEX idx_executors_node ON executors(node);
CREATE INDEX idx_executors_state ON executors(state);
CREATE INDEX idx_executors_session ON executors(workspace,session);
