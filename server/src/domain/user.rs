//! `User` — one row per customer organization/individual (`users` table,
//! `LICENSE_DATABASE_SCHEMA.md` §1). Server-owned; never replicated to the
//! desktop app (see that document's intro: "the desktop never stores
//! `users` or `payments` tables").

use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub id: i64,
    pub email: String,
    pub password_hash: String,
    pub full_name: Option<String>,
    pub company_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Fields needed to create a new `User` row — no `id`/timestamps, since
/// those are database-generated.
#[derive(Debug, Clone, PartialEq)]
pub struct NewUser {
    pub email: String,
    pub password_hash: String,
    pub full_name: Option<String>,
    pub company_name: Option<String>,
}
