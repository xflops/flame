-- Add nodes and executors tables for persistence (Issue #25)
-- This enables node and executor state to survive session manager restarts

CREATE TABLE IF NOT EXISTS nodes (
    name                TEXT PRIMARY KEY,
    state               INTEGER NOT NULL DEFAULT 0,
    
    -- Capacity resources
    capacity_cpu        INTEGER NOT NULL DEFAULT 0,
    capacity_memory     INTEGER NOT NULL DEFAULT 0,
    
    -- Allocatable resources
    allocatable_cpu     INTEGER NOT NULL DEFAULT 0,
    allocatable_memory  INTEGER NOT NULL DEFAULT 0,
    
    -- Node info
    info_arch           TEXT NOT NULL DEFAULT '',
    info_os             TEXT NOT NULL DEFAULT '',
    
    creation_time       INTEGER NOT NULL,
    last_heartbeat      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS executors (
    id                  TEXT PRIMARY KEY,
    node                TEXT NOT NULL,

    -- Resource requirements
    resreq_cpu          INTEGER NOT NULL DEFAULT 0,
    resreq_memory       INTEGER NOT NULL DEFAULT 0,

    shim                INTEGER NOT NULL DEFAULT 0,
    
    task_id             INTEGER,
    ssn_id              TEXT,
    
    creation_time       INTEGER NOT NULL,
    state               INTEGER NOT NULL DEFAULT 0,
    
    FOREIGN KEY (node) REFERENCES nodes(name) ON DELETE CASCADE
);

-- Index for efficient executor lookups by node
CREATE INDEX IF NOT EXISTS idx_executors_node ON executors(node);

-- Index for efficient executor lookups by state
CREATE INDEX IF NOT EXISTS idx_executors_state ON executors(state);

-- Index for efficient executor lookups by session
CREATE INDEX IF NOT EXISTS idx_executors_ssn_id ON executors(ssn_id);
