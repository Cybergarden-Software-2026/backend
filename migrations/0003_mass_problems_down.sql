ALTER TABLE dashboard_events DROP CONSTRAINT ck_dashboard_events_reason;
UPDATE dashboard_events SET reason = 'import' WHERE reason = 'mass_problems';
ALTER TABLE dashboard_events ADD CONSTRAINT ck_dashboard_events_reason
    CHECK (reason IN ('import', 'source', 'expiry', 'assignment'));

DROP TABLE mass_problem_workspace;
