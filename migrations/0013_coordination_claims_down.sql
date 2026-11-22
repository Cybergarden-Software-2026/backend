DROP TRIGGER release_incident_coordination_claim ON incidents;
DROP FUNCTION release_incident_coordination_claim();
DROP TABLE incident_coordination_claims;

ALTER TABLE dashboard_events DROP CONSTRAINT ck_dashboard_events_reason;
UPDATE dashboard_events SET reason = 'source' WHERE reason = 'coordination_claim';
ALTER TABLE dashboard_events ADD CONSTRAINT ck_dashboard_events_reason
    CHECK (reason IN ('import', 'source', 'expiry', 'assignment', 'mass_problems', 'team_contacted'));
