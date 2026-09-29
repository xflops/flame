-- Add GPU columns to nodes and executors tables (Issue #433)
-- This enables GPU resource tracking for DRF scheduling

-- Add GPU capacity and allocatable to nodes table
ALTER TABLE nodes ADD COLUMN capacity_gpu INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN allocatable_gpu INTEGER NOT NULL DEFAULT 0;

-- Add GPU to executor resource requirements
ALTER TABLE executors ADD COLUMN resreq_gpu INTEGER NOT NULL DEFAULT 0;

-- Add explicit resource requirements to sessions (mutually exclusive with slots)
ALTER TABLE sessions ADD COLUMN resreq_cpu INTEGER;
ALTER TABLE sessions ADD COLUMN resreq_memory INTEGER;
ALTER TABLE sessions ADD COLUMN resreq_gpu INTEGER;
