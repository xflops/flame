-- Add RBAC tables for users, roles, and workspaces (RFE mTLS Auth)
-- This enables role-based access control with mTLS authentication

-- Add workspace column to existing tables
ALTER TABLE applications ADD COLUMN workspace TEXT NOT NULL DEFAULT 'default';
ALTER TABLE sessions ADD COLUMN workspace TEXT NOT NULL DEFAULT 'default';
ALTER TABLE tasks ADD COLUMN workspace TEXT NOT NULL DEFAULT 'default';

-- Create index for workspace filtering
CREATE INDEX IF NOT EXISTS idx_applications_workspace ON applications(workspace);
CREATE INDEX IF NOT EXISTS idx_sessions_workspace ON sessions(workspace);
CREATE INDEX IF NOT EXISTS idx_tasks_workspace ON tasks(workspace);

CREATE TABLE IF NOT EXISTS users (
    name                TEXT PRIMARY KEY,
    display_name        TEXT,
    email               TEXT,
    certificate_cn      TEXT NOT NULL UNIQUE,
    enabled             INTEGER NOT NULL DEFAULT 1,
    creation_time       INTEGER NOT NULL,
    last_login_time     INTEGER
);

CREATE TABLE IF NOT EXISTS roles (
    name                TEXT PRIMARY KEY,
    description         TEXT,
    permissions         TEXT,
    workspaces          TEXT,
    creation_time       INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS workspaces (
    name                TEXT PRIMARY KEY,
    description         TEXT,
    labels              TEXT,
    creation_time       INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS user_roles (
    user_name           TEXT NOT NULL,
    role_name           TEXT NOT NULL,
    PRIMARY KEY (user_name, role_name),
    FOREIGN KEY (user_name) REFERENCES users(name) ON DELETE CASCADE,
    FOREIGN KEY (role_name) REFERENCES roles(name) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_users_certificate_cn ON users(certificate_cn);
CREATE INDEX IF NOT EXISTS idx_user_roles_user ON user_roles(user_name);
CREATE INDEX IF NOT EXISTS idx_user_roles_role ON user_roles(role_name);
