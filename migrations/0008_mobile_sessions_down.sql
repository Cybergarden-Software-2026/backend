-- The old API cannot revoke a mobile session using its bearer. Unlink these
-- installations (including their queued pushes) before removing the column.
DELETE FROM mobile_devices WHERE client_device_id IS NOT NULL;
ALTER TABLE mobile_devices DROP COLUMN client_device_id;
