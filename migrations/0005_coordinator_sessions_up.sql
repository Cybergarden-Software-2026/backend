CREATE TABLE coordinator_sessions (
    token_hash BYTEA PRIMARY KEY CHECK (octet_length(token_hash) = 32),
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX coordinator_sessions_expiry_idx ON coordinator_sessions (expires_at);
