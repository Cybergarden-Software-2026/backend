ALTER TABLE teams
    ADD COLUMN phone TEXT CHECK (phone ~ '^[+][1-9][0-9]{1,14}$');

-- Keep a stable placeholder for each imported team. 819 is in the unassigned
-- common reserve of numbering zone +7 (ITU Operational Bulletin 1341).
CREATE FUNCTION team_demo_phone(team_name TEXT) RETURNS TEXT
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$
    SELECT '+7819' || lpad((('x' || left(md5(team_name), 8))::bit(32)::bigint % 10000000)::text, 7, '0');
$$;
