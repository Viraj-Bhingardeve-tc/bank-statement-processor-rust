//! `users` table access (`LICENSE_DATABASE_SCHEMA.md` §1). Trait first,
//! Postgres implementation second — services depend on the trait, never on
//! `PgUserRepository` directly, so tests can substitute a mock (see
//! `service::auth_service`'s tests for an example).

use crate::domain::{NewUser, User, UserRole};
use crate::repository::error::RepositoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::str::FromStr;

#[async_trait]
pub trait UserRepository: Send + Sync {
    async fn find_by_email(&self, email: &str) -> Result<Option<User>, RepositoryError>;
    /// Resolves a session's `user_id` back to its account — added for
    /// Module 2's `AuthService::require_admin`, which needs the caller's
    /// `role` and has nothing but that id to look it up with (`Session`
    /// itself carries no role). No other call site needs this yet.
    async fn find_by_id(&self, id: i64) -> Result<Option<User>, RepositoryError>;
    async fn insert(&self, new_user: NewUser) -> Result<User, RepositoryError>;
}

pub struct PgUserRepository {
    pool: PgPool,
}

impl PgUserRepository {
    pub fn new(pool: PgPool) -> Self {
        PgUserRepository { pool }
    }
}

#[derive(sqlx::FromRow)]
struct UserRow {
    id: i64,
    email: String,
    password_hash: String,
    full_name: Option<String>,
    company_name: Option<String>,
    role: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<UserRow> for User {
    type Error = RepositoryError;

    fn try_from(row: UserRow) -> Result<Self, Self::Error> {
        Ok(User {
            id: row.id,
            email: row.email,
            password_hash: row.password_hash,
            full_name: row.full_name,
            company_name: row.company_name,
            role: UserRole::from_str(&row.role).map_err(RepositoryError::InvalidData)?,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

const USER_COLUMNS: &str =
    "id, email, password_hash, full_name, company_name, role, created_at, updated_at";

#[async_trait]
impl UserRepository for PgUserRepository {
    async fn find_by_email(&self, email: &str) -> Result<Option<User>, RepositoryError> {
        let row = sqlx::query_as::<_, UserRow>(&format!(
            "SELECT {USER_COLUMNS} FROM users WHERE email = $1"
        ))
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;

        row.map(User::try_from).transpose()
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<User>, RepositoryError> {
        let row = sqlx::query_as::<_, UserRow>(&format!(
            "SELECT {USER_COLUMNS} FROM users WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(User::try_from).transpose()
    }

    async fn insert(&self, new_user: NewUser) -> Result<User, RepositoryError> {
        let row = sqlx::query_as::<_, UserRow>(&format!(
            "INSERT INTO users (email, password_hash, full_name, company_name) \
             VALUES ($1, $2, $3, $4) \
             RETURNING {USER_COLUMNS}"
        ))
        .bind(&new_user.email)
        .bind(&new_user.password_hash)
        .bind(&new_user.full_name)
        .bind(&new_user.company_name)
        .fetch_one(&self.pool)
        .await?;

        User::try_from(row)
    }
}
