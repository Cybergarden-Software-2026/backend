CREATE TABLE mass_problem_workspace (
    id smallint PRIMARY KEY CHECK (id = 1),
    state jsonb NOT NULL
);

INSERT INTO mass_problem_workspace (id, state) VALUES (1, '{"groups":[],"exclusions":[]}');

ALTER TABLE dashboard_events DROP CONSTRAINT ck_dashboard_events_reason;
ALTER TABLE dashboard_events ADD CONSTRAINT ck_dashboard_events_reason
    CHECK (reason IN ('import', 'source', 'expiry', 'assignment', 'mass_problems'));
