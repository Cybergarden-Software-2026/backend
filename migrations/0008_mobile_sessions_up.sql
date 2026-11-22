-- Older push-only registrations keep their original UUID and token.
ALTER TABLE mobile_devices ADD COLUMN client_device_id TEXT COLLATE "C" UNIQUE
    CHECK (client_device_id ~ '^[!-~]{1,128}$');
