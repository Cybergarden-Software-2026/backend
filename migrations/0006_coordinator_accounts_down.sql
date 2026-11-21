DELETE FROM coordinator_sessions WHERE account_login IS NOT NULL;
ALTER TABLE coordinator_sessions DROP COLUMN account_login;
DROP TABLE coordinator_accounts;
