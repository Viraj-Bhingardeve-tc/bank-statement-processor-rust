//! `User` — one row per customer organization/individual (`users` table,
//! `LICENSE_DATABASE_SCHEMA.md` §1). Server-owned; never replicated to the
//! desktop app (see that document's intro: "the desktop never stores
//! `users` or `payments` tables").

use chrono::{DateTime, Utc};
use std::fmt;
use std::str::FromStr;

/// The `users.role` column (migration `0007`, Module 2). Every account is
/// `Customer` unless explicitly promoted — see that migration's doc
/// comment for why there is no code path in this crate that ever creates
/// an `Admin` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserRole {
    Customer,
    Admin,
}

impl UserRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            UserRole::Customer => "customer",
            UserRole::Admin => "admin",
        }
    }
}

impl fmt::Display for UserRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for UserRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "customer" => Ok(UserRole::Customer),
            "admin" => Ok(UserRole::Admin),
            other => Err(format!("unrecognized user role {other:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub id: i64,
    pub email: String,
    pub password_hash: String,
    pub full_name: Option<String>,
    pub company_name: Option<String>,
    pub role: UserRole,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Fields needed to create a new `User` row — no `id`/timestamps, since
/// those are database-generated. No `role` field: every row this creates
/// gets `users.role`'s `'customer'` default (migration `0007`) — see that
/// migration's doc comment for why promoting an account to `Admin` is
/// deliberately left out of this struct entirely, not merely defaulted.
#[derive(Debug, Clone, PartialEq)]
pub struct NewUser {
    pub email: String,
    pub password_hash: String,
    pub full_name: Option<String>,
    pub company_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_role_round_trips_through_its_string_form() {
        for role in [UserRole::Customer, UserRole::Admin] {
            assert_eq!(UserRole::from_str(role.as_str()).unwrap(), role);
        }
    }

    #[test]
    fn user_role_rejects_an_unrecognized_string() {
        assert!(UserRole::from_str("superuser").is_err());
    }
}
