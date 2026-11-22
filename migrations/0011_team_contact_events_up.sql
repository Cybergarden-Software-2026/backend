ALTER TABLE dashboard_events DROP CONSTRAINT ck_dashboard_events_reason;
ALTER TABLE dashboard_events ADD CONSTRAINT ck_dashboard_events_reason
    CHECK (reason IN ('import', 'source', 'expiry', 'assignment', 'mass_problems', 'team_contacted'));

-- A previous downgrade keeps the audit entries and normalizes their reason.
UPDATE dashboard_events d SET reason = 'team_contacted'
WHERE d.reason = 'source' AND d.payload ? 'contact' AND EXISTS (
    SELECT 1 FROM change_events c WHERE c.revision = d.id AND 'team_contacted' = ANY (c.types)
);

CREATE UNIQUE INDEX dashboard_events_team_contact_request_idx ON dashboard_events (
    (payload->'contact'->>'actorLogin'), (payload->>'sysId'), (payload->'contact'->>'requestId')
) WHERE reason = 'team_contacted';
