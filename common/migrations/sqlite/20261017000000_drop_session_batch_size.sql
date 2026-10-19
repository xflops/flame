-- Remove the unused session batch_size field (RFE591).
ALTER TABLE sessions DROP COLUMN batch_size;
