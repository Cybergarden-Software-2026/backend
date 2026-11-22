CREATE TABLE form_incidents (
    form_id text NOT NULL,
    answer_id text NOT NULL,
    incident_id text NOT NULL UNIQUE REFERENCES incidents(id) ON DELETE CASCADE,
    PRIMARY KEY (form_id, answer_id)
);

CREATE TABLE forms_import_state (
    id smallint PRIMARY KEY CHECK (id = 1),
    cursor bigint NOT NULL CHECK (cursor >= 0)
);

INSERT INTO forms_import_state (id, cursor) VALUES (1, 0);
