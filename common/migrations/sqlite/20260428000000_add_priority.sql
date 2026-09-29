-- Add priority field to sessions table (RFE413-priority-scheduling)
-- priority: session priority (higher value = higher priority, default 0 = lowest)

ALTER TABLE sessions ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;
