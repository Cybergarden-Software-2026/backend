SELECT id FROM import_state WHERE id = 1 FOR UPDATE;

DROP FUNCTION enqueue_mobile_bell(TEXT, TEXT, TEXT, TIMESTAMPTZ);

CREATE OR REPLACE FUNCTION enqueue_mobile_critical() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM mobile_push_deliveries WHERE expires_at <= now();
    INSERT INTO mobile_push_deliveries (notification_id, device_id)
    SELECT NEW.id, d.id FROM mobile_devices d
    JOIN coordinator_accounts a ON a.login = d.account_login
    WHERE a.disabled_at IS NULL AND d.fcm_token IS NOT NULL;
    RETURN NEW;
END;
$$;

-- The previous sender can only deliver journal-backed notifications.
DELETE FROM mobile_push_deliveries WHERE journal_notification_id IS NULL;
ALTER TABLE mobile_push_deliveries
    DROP CONSTRAINT mobile_push_event_source_ck,
    DROP COLUMN journal_notification_id,
    DROP COLUMN bell_kind,
    DROP COLUMN bell_incident_id,
    DROP COLUMN bell_sys_id,
    ADD CONSTRAINT mobile_push_deliveries_notification_id_fkey
        FOREIGN KEY (notification_id) REFERENCES notifications (id) ON DELETE CASCADE;

DROP TRIGGER mobile_fcm_registration_time ON mobile_devices;
DROP FUNCTION record_fcm_registration_time();
ALTER TABLE mobile_devices DROP COLUMN fcm_registered_at;
DROP TRIGGER mobile_bell_legacy_write ON incidents;
DROP FUNCTION seed_legacy_mobile_bell_state();
DROP TABLE mobile_bell_state;
