-- Add batch scheduling fields (RFE400-batch-session)
-- batch_size: reserved session field, currently normalized to 1
-- batch_index: reserved executor field

ALTER TABLE sessions ADD COLUMN batch_size INTEGER NOT NULL DEFAULT 1;

ALTER TABLE executors ADD COLUMN batch_index INTEGER;
