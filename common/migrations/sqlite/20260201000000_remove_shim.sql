-- Remove deprecated 'shim' column from applications table
-- Shim configuration is now handled by executor-manager, not Application API

-- Use non-destructive migration to preserve existing application data
-- Step 1: Rename existing table
ALTER TABLE applications RENAME TO applications_old;

-- Step 2: Create new table without shim column
CREATE TABLE applications (
    name                TEXT NOT NULL,
    description         TEXT,
    labels              TEXT,

    image               TEXT,
    command             TEXT,
    arguments           TEXT,
    environments        TEXT,
    working_directory   TEXT,
    schema              TEXT,
    url                 TEXT,

    max_instances       INTEGER NOT NULL,
    delay_release       INTEGER NOT NULL,
    creation_time       INTEGER NOT NULL,
    version             INTEGER NOT NULL DEFAULT 1,

    state INTEGER       NOT NULL,

    PRIMARY KEY (name)
);

-- Step 3: Copy data from old table to new table (excluding shim column)
INSERT INTO applications (
    name, description, labels, image, command, arguments, environments,
    working_directory, schema, url, max_instances, delay_release,
    creation_time, version, state
)
SELECT 
    name, description, labels, image, command, arguments, environments,
    working_directory, schema, url, max_instances, delay_release,
    creation_time, version, state
FROM applications_old;

-- Step 4: Drop old table
DROP TABLE applications_old;
