DELETE FROM analysis_jobs WHERE kind = 'cluster';
DROP INDEX ix_analysis_jobs_incident_pk_kind;
DROP INDEX uq_analysis_jobs_input;
-- Из нескольких задач одной ревизии остаётся последняя.
DELETE FROM analysis_jobs a USING analysis_jobs b
WHERE a.incident_pk = b.incident_pk AND a.kind = b.kind AND a.incident_revision = b.incident_revision
  AND a.model = b.model AND a.prompt_version = b.prompt_version AND a.created_at < b.created_at;
ALTER TABLE analysis_jobs ADD CONSTRAINT uq_analysis_jobs_incident_pk
    UNIQUE (incident_pk, kind, incident_revision, model, prompt_version);

ALTER TABLE analysis_jobs DROP CONSTRAINT ck_analysis_jobs_kind;
ALTER TABLE analysis_jobs ADD CONSTRAINT ck_analysis_jobs_kind CHECK (kind IN ('summary'));
