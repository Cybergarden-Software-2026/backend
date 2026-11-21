DROP TABLE assignment_overrides;
DROP TABLE teams;
ALTER TABLE dashboard_events DROP CONSTRAINT ck_dashboard_events_reason;
UPDATE dashboard_events SET reason = 'import' WHERE reason = 'assignment';
ALTER TABLE dashboard_events ADD CONSTRAINT ck_dashboard_events_reason
    CHECK (reason IN ('import', 'source', 'expiry'));
