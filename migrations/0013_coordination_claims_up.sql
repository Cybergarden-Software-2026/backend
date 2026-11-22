CREATE TABLE incident_coordination_claims (
    incident_pk bigint PRIMARY KEY REFERENCES incidents(pk) ON DELETE CASCADE,
    account_login text NOT NULL REFERENCES coordinator_accounts(login) ON DELETE CASCADE,
    claimed_at timestamptz NOT NULL
);
CREATE INDEX incident_coordination_claims_account_idx ON incident_coordination_claims(account_login);

ALTER TABLE dashboard_events DROP CONSTRAINT ck_dashboard_events_reason;
ALTER TABLE dashboard_events ADD CONSTRAINT ck_dashboard_events_reason
    CHECK (reason IN ('import', 'source', 'expiry', 'assignment', 'mass_problems', 'team_contacted',
                      'coordination_claim'));

-- A claim applies to one visit to the coordinator queue. Clear it with the same write that
-- routes the incident out, removes it from the source, or stops active coordination.
CREATE FUNCTION release_incident_coordination_claim() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM incident_coordination_claims WHERE incident_pk = NEW.pk;
    RETURN NEW;
END;
$$;

CREATE TRIGGER release_incident_coordination_claim
AFTER UPDATE OF assigned_group_name, normalized_state, is_present ON incidents
FOR EACH ROW WHEN (
    NOT NEW.is_present
    OR btrim(OLD.assigned_group_name) IS DISTINCT FROM btrim(NEW.assigned_group_name)
    OR NEW.normalized_state NOT IN ('Assigned', 'WorkInProgress', 'Research')
)
EXECUTE FUNCTION release_incident_coordination_claim();
