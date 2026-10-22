-- Restore the session batch_size field, now the gang size (it was dropped in 20261017000000).
ALTER TABLE sessions ADD COLUMN batch_size INTEGER NOT NULL DEFAULT 1;
