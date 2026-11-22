DROP INDEX dashboard_events_team_contact_request_idx;

-- Keep recorded contacts and their audit metadata in the existing journal tables.
UPDATE dashboard_events SET reason = 'source' WHERE reason = 'team_contacted';

ALTER TABLE dashboard_events DROP CONSTRAINT ck_dashboard_events_reason;
ALTER TABLE dashboard_events ADD CONSTRAINT ck_dashboard_events_reason
    CHECK (reason IN ('import', 'source', 'expiry', 'assignment', 'mass_problems'));
