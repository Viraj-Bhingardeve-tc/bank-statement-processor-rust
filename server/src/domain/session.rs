//! `Session` — a server-account bearer-token session (`sessions` table,
//! `PHASE4_DESIGN.md` §7 — an addition beyond `LICENSE_DATABASE_SCHEMA.md`
//! §1, added once payment/auth needed real, revocable session storage
//! rather than a stateless token).

use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: i64,
    pub user_id: i64,
    /// SHA-256 of the bearer token — the raw token itself is never stored
    /// (`PHASE4_DESIGN.md` §1.3).
    pub token_hash: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// Set by `POST /logout`.
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Fields needed to create a new session — no `id`/`created_at`, since
/// those are database-generated. `token_hash` is computed by the caller
/// (the service layer, once it exists) before this ever reaches the
/// repository — this layer never sees the raw bearer token.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSession {
    pub user_id: i64,
    pub token_hash: String,
    pub expires_at: DateTime<Utc>,
}
