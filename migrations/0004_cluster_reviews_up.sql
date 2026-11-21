-- 0004: проверка групп массовых сбоев локальной моделью в общей очереди analysis_jobs.
-- Задача 'cluster' привязана к записи группы с наименьшим pk; вход — поля всех участников.
-- Уникальность учитывает вход: у одной записи может быть несколько групп за одну ревизию.
ALTER TABLE analysis_jobs DROP CONSTRAINT ck_analysis_jobs_kind;
ALTER TABLE analysis_jobs ADD CONSTRAINT ck_analysis_jobs_kind CHECK (kind IN ('summary', 'cluster'));

ALTER TABLE analysis_jobs DROP CONSTRAINT uq_analysis_jobs_incident_pk;
CREATE UNIQUE INDEX uq_analysis_jobs_input
    ON analysis_jobs (incident_pk, kind, incident_revision, model, prompt_version, md5(input::text));
CREATE INDEX ix_analysis_jobs_incident_pk_kind ON analysis_jobs (incident_pk, kind);
