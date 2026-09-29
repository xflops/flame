CREATE TABLE IF NOT EXISTS sessions (
    id              TEXT PRIMARY KEY,
    application     TEXT NOT NULL,

    common_data     BLOB,

    creation_time   INTEGER NOT NULL,
    completion_time INTEGER,

    state           INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS tasks (
    id              INTEGER NOT NULL,
    ssn_id          TEXT NOT NULL,

    input           BLOB,
    output          BLOB,

    creation_time   INTEGER NOT NULL,
    completion_time INTEGER,

    state           INTEGER NOT NULL,

    PRIMARY KEY (id, ssn_id)
);
