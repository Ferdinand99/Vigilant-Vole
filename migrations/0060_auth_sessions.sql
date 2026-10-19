-- Auth agent: session invalidation and TOTP replay protection.
--
-- session_version: embedded in every session cookie; bumping it (password
--   change, "sign out other sessions", disable, 2FA reset) invalidates all
--   cookies issued before.
-- totp_last_step: the last TOTP time step accepted for this user, so a code
--   can never be used twice (RFC 6238 §5.2).
ALTER TABLE users ADD COLUMN session_version INTEGER NOT NULL DEFAULT 1;
ALTER TABLE users ADD COLUMN totp_last_step INTEGER;
