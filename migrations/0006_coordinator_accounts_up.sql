CREATE TABLE coordinator_accounts (
    login TEXT PRIMARY KEY CHECK (login ~ '^[a-z0-9_.@-]{1,64}$'),
    display_name TEXT NOT NULL CHECK (char_length(display_name) BETWEEN 1 AND 100),
    password_hash TEXT NOT NULL,
    key_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(key_hash) = 32),
    disabled_at TIMESTAMPTZ
);

ALTER TABLE coordinator_sessions ADD COLUMN account_login TEXT
    REFERENCES coordinator_accounts (login) ON DELETE CASCADE;
CREATE INDEX coordinator_sessions_account_idx ON coordinator_sessions (account_login)
    WHERE account_login IS NOT NULL;
