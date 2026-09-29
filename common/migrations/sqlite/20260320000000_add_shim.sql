-- Add 'shim' column back to applications table (RFE379)
-- This re-introduces the shim field to enable application-level shim selection

ALTER TABLE applications ADD COLUMN shim INTEGER NOT NULL DEFAULT 0;
