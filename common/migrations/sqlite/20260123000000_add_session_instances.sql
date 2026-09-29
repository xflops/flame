-- Add min_instances and max_instances columns to sessions table for RFE323
-- This migration adds support for controlling the minimum and maximum number
-- of executor instances that can be allocated to a session.

-- Add min_instances column (minimum number of instances to maintain)
-- Default is 0 (no minimum guarantee)
ALTER TABLE sessions 
ADD COLUMN min_instances INTEGER NOT NULL DEFAULT 0;

-- Add max_instances column (maximum number of instances allowed)
-- NULL means unlimited
ALTER TABLE sessions 
ADD COLUMN max_instances INTEGER;

-- Note: Existing sessions will automatically get default values:
-- - min_instances = 0 (no minimum)
-- - max_instances = NULL (unlimited)
