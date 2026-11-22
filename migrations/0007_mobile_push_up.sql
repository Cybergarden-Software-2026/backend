-- A coordinator can issue one live invitation and pair several devices.
CREATE TABLE mobile_pairings (
    account_login TEXT PRIMARY KEY REFERENCES coordinator_accounts (login) ON DELETE CASCADE,
    code_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(code_hash) = 32),
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE mobile_devices (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_login TEXT NOT NULL REFERENCES coordinator_accounts (login) ON DELETE CASCADE,
    platform TEXT NOT NULL CHECK (platform IN ('android', 'web')),
    fcm_token TEXT UNIQUE,
    fcm_target_type TEXT NOT NULL DEFAULT 'token' CHECK (fcm_target_type IN ('token', 'fid')),
    secret_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(secret_hash) = 32),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX mobile_devices_account_idx ON mobile_devices (account_login);

CREATE TABLE mobile_push_deliveries (
    notification_id BIGINT NOT NULL REFERENCES notifications (id) ON DELETE CASCADE,
    device_id UUID NOT NULL REFERENCES mobile_devices (id) ON DELETE CASCADE,
    available_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL DEFAULT now() + interval '1 hour',
    attempts INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (notification_id, device_id)
);

CREATE INDEX mobile_deliveries_ready_idx ON mobile_push_deliveries (available_at, notification_id);
CREATE INDEX mobile_deliveries_expiry_idx ON mobile_push_deliveries (expires_at);
CREATE INDEX mobile_deliveries_device_idx ON mobile_push_deliveries (device_id);

CREATE FUNCTION enqueue_mobile_critical() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    -- Bound the backlog even while the HTTP service is offline.
    DELETE FROM mobile_push_deliveries WHERE expires_at <= now();
    INSERT INTO mobile_push_deliveries (notification_id, device_id)
    SELECT NEW.id, d.id FROM mobile_devices d
    JOIN coordinator_accounts a ON a.login = d.account_login
    WHERE a.disabled_at IS NULL AND d.fcm_token IS NOT NULL;
    RETURN NEW;
END;
$$;

CREATE TRIGGER mobile_critical_created
AFTER INSERT ON notifications FOR EACH ROW
WHEN (NEW.type = 'critical.raised' AND NEW.sys_id IS NOT NULL AND NEW.incident_id IS NOT NULL)
EXECUTE FUNCTION enqueue_mobile_critical();
