-- Add batch scheduling fields (RFE400-batch-session)
-- batch_size: number of executors per batch for batch scheduling
-- batch_index: executor's index within its batch (0 to batch_size-1)

ALTER TABLE sessions ADD COLUMN batch_size INTEGER NOT NULL DEFAULT 1;

ALTER TABLE executors ADD COLUMN batch_index INTEGER;
