CREATE TABLE teams (
    name text PRIMARY KEY CHECK (btrim(name) <> '' AND name <> 'AutoClosedMi')
);

INSERT INTO teams (name)
SELECT DISTINCT assigned_group_name FROM incidents
WHERE assigned_group_name <> '' AND assigned_group_name <> 'AutoClosedMi';

-- Keep the last source assignment separate from the coordinator's local decision.
CREATE TABLE assignment_overrides (
    incident_id text PRIMARY KEY REFERENCES incidents(id) ON DELETE CASCADE,
    source_assignment jsonb NOT NULL,
    team text NOT NULL REFERENCES teams(name),
    assigned_at timestamptz NOT NULL
);

ALTER TABLE dashboard_events DROP CONSTRAINT ck_dashboard_events_reason;
ALTER TABLE dashboard_events ADD CONSTRAINT ck_dashboard_events_reason
    CHECK (reason IN ('import', 'source', 'expiry', 'assignment'));
