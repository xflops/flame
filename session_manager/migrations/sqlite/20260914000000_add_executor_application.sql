-- Persist the application that owns each executor instance.
ALTER TABLE executors ADD COLUMN application TEXT NOT NULL DEFAULT '';
