-- Add url field to applications table for RFE284
ALTER TABLE applications ADD COLUMN url TEXT;
ALTER TABLE applications ADD COLUMN package_signature TEXT;
