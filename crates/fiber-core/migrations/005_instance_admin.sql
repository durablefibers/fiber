-- Instance-level admin flag. Governs the global agent pool, listing every agent,
-- and user management. Nobody is promoted here: fiber-api promotes FIBER_ADMIN_USER
-- at boot while the instance has no admin (Store::ensure_admin_user), which keeps
-- the grant tied to an operator-controlled setting rather than to row age.
ALTER TABLE users
    ADD COLUMN IF NOT EXISTS is_admin BOOLEAN NOT NULL DEFAULT FALSE;
