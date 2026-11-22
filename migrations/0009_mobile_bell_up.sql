-- Imports and local assignments take this lock before changing incidents.
SELECT id FROM import_state WHERE id = 1 FOR UPDATE;

CREATE TABLE mobile_bell_state (
    incident_pk BIGINT PRIMARY KEY REFERENCES incidents (pk) ON DELETE CASCADE,
    initialized BOOLEAN NOT NULL DEFAULT true,
    kind TEXT CHECK (kind IN ('critical', 'waiting', 'deadline')),
    team TEXT,
    assigned_at TIMESTAMPTZ,
    deadline TIMESTAMPTZ,
    next_check_at TIMESTAMPTZ,
    CHECK (initialized OR next_check_at IS NOT NULL)
);

CREATE INDEX mobile_bell_due_idx ON mobile_bell_state (next_check_at, incident_pk)
    WHERE next_check_at IS NOT NULL;

-- The producer reconstructs the initial signature at this instant before
-- processing either a timer or the first edit of an existing incident.
INSERT INTO mobile_bell_state (incident_pk, initialized, next_check_at)
SELECT pk, false, statement_timestamp() FROM incidents WHERE is_present;

-- During a rolling upgrade an older importer can still write this schema.
-- Queue its latest record as a silent baseline so future time thresholds remain
-- scheduled. The current importer maintains the signature in the same transaction.
CREATE FUNCTION seed_legacy_mobile_bell_state() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.is_present THEN
        INSERT INTO mobile_bell_state (incident_pk, initialized, next_check_at)
        VALUES (NEW.pk, false, statement_timestamp())
        ON CONFLICT (incident_pk) DO UPDATE
            SET initialized = false, kind = NULL, team = NULL, assigned_at = NULL,
                deadline = NULL, next_check_at = EXCLUDED.next_check_at;
    ELSE
        DELETE FROM mobile_bell_state WHERE incident_pk = NEW.pk;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER mobile_bell_legacy_write
    AFTER INSERT OR UPDATE OF incident_revision ON incidents FOR EACH ROW
    WHEN (current_setting('underctrl.mobile_bell_writer', true) IS DISTINCT FROM '1')
    EXECUTE FUNCTION seed_legacy_mobile_bell_state();

ALTER TABLE mobile_devices ADD COLUMN fcm_registered_at TIMESTAMPTZ;
UPDATE mobile_devices SET fcm_registered_at = statement_timestamp() WHERE fcm_token IS NOT NULL;
ALTER TABLE mobile_devices ADD CONSTRAINT mobile_devices_fcm_registration_ck
    CHECK ((fcm_token IS NULL) = (fcm_registered_at IS NULL));

CREATE FUNCTION record_fcm_registration_time() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.fcm_token IS NULL THEN
        NEW.fcm_registered_at := NULL;
    ELSIF TG_OP = 'INSERT' OR OLD.fcm_token IS NULL THEN
        NEW.fcm_registered_at := statement_timestamp();
    ELSE
        NEW.fcm_registered_at := OLD.fcm_registered_at;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER mobile_fcm_registration_time
    BEFORE INSERT OR UPDATE OF fcm_token ON mobile_devices
    FOR EACH ROW EXECUTE FUNCTION record_fcm_registration_time();

-- Keep the original primary key and leases. Journal-backed deliveries retain
-- their cascade; time-based bell events need no corresponding journal entry.
ALTER TABLE mobile_push_deliveries
    ADD COLUMN journal_notification_id BIGINT REFERENCES notifications (id) ON DELETE CASCADE,
    ADD COLUMN bell_kind TEXT,
    ADD COLUMN bell_incident_id TEXT,
    ADD COLUMN bell_sys_id TEXT;
UPDATE mobile_push_deliveries SET journal_notification_id = notification_id;
ALTER TABLE mobile_push_deliveries
    DROP CONSTRAINT mobile_push_deliveries_notification_id_fkey,
    ADD CONSTRAINT mobile_push_event_source_ck CHECK (
        (journal_notification_id IS NOT NULL AND journal_notification_id = notification_id
            AND bell_kind IS NULL AND bell_incident_id IS NULL AND bell_sys_id IS NULL)
        OR
        (journal_notification_id IS NULL AND bell_kind IS NOT NULL
            AND bell_kind IN ('critical', 'waiting', 'deadline')
            AND bell_incident_id IS NOT NULL AND bell_sys_id IS NOT NULL)
    );
CREATE INDEX mobile_push_journal_idx ON mobile_push_deliveries (journal_notification_id)
    WHERE journal_notification_id IS NOT NULL;

CREATE OR REPLACE FUNCTION enqueue_mobile_critical() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM mobile_push_deliveries WHERE expires_at <= now();
    INSERT INTO mobile_push_deliveries (notification_id, journal_notification_id, device_id)
    SELECT NEW.id, NEW.id, d.id FROM mobile_devices d
    JOIN coordinator_accounts a ON a.login = d.account_login
    WHERE a.disabled_at IS NULL AND d.fcm_token IS NOT NULL;
    RETURN NEW;
END;
$$;

-- A single opaque ID per event is shared by every recipient and retry. Both
-- sources allocate from one sequence so an alert never overwrites a journal push.
CREATE FUNCTION enqueue_mobile_bell(kind TEXT, incident_id TEXT, sys_id TEXT, happened_at TIMESTAMPTZ)
RETURNS VOID LANGUAGE sql AS $$
    DELETE FROM mobile_push_deliveries WHERE expires_at <= statement_timestamp();
    WITH event AS MATERIALIZED (
        SELECT nextval(pg_get_serial_sequence('notifications', 'id')) AS id
    )
    INSERT INTO mobile_push_deliveries (
        notification_id, device_id, bell_kind, bell_incident_id, bell_sys_id, expires_at
    )
    SELECT event.id, d.id, kind, incident_id, sys_id, happened_at + interval '1 hour'
    FROM mobile_devices d
    JOIN coordinator_accounts a ON a.login = d.account_login
    CROSS JOIN event
    WHERE a.disabled_at IS NULL AND d.fcm_registered_at <= happened_at
      AND happened_at + interval '1 hour' > statement_timestamp();
$$;
