-- zz_reset_tokens_hashed.sql — a reset token at rest is a DIGEST, not the mailed value (2026-11-19).
--
-- WHY. `password_resets.token` held the exact string the reset link carries, so anyone who could
-- read the table — a backup, a replica, a support export, an injection — held a live credential
-- for that account for the rest of the window. ADASwift already stores `token_hash`; this brings
-- the same rule here without a column rename: `handlers::auth_handler::{forgot_password,
-- reset_password}` and `email_settings_handler`'s operator "sign-in help" mail now write the
-- lowercase hex SHA-256 of the token (see `src/security/reset_token.rs`), and the lookup hashes
-- what the caller presents before comparing. The mailed link still carries the raw value.
--
-- WHAT THIS FILE DOES. It retires any row that is still holding a raw uuid token, so a value
-- already sitting in the clear cannot be replayed against an account after the change lands. The
-- account owner is not locked out: a new link is one "forgot password" away, and that one is
-- stored as a digest.
--
-- Idempotent by construction: after the first run no unused row has a non-digest token, so a
-- second execution matches zero rows (this runner re-executes nothing it has recorded, and the
-- statement is a no-op on re-run regardless).
UPDATE password_resets
   SET used = true
 WHERE used = false
   AND length(token) <> 64;
