//! `users` table access (`LICENSE_DATABASE_SCHEMA.md` §1). Trait first,
//! Postgres implementation second — services depend on the trait, never on
//! `PgUserRepository` directly, so tests can substitute a mock (see
//! `service::auth_service`'s tests for an example).

use crate::domain::{NewUser, User};
use crate::repository::error::RepositoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

#[async_trait]
pub trait UserRepository: Send + Sync {
    async fn find_by_email(&self, email: &str) -> Result<Option<User>, RepositoryError>;
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
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<UserRow> for User {
    fn from(row: UserRow) -> Self {
        User {
            id: row.id,
            email: row.email,
            password_hash: row.password_hash,
            full_name: row.full_name,
            company_name: row.company_name,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

#[async_trait]
impl UserRepository for PgUserRepository {
    async fn find_by_email(&self, email: &str) -> Result<Option<User>, RepositoryError> {
        let row = sqlx::query_as::<_, UserRow>(
            "SELECT id, email, password_hash, full_name, company_name, created_at, updated_at \
             FROM users WHERE email = $1",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(User::from))
    }

    async fn insert(&self, new_user: NewUser) -> Result<User, RepositoryError> {
        let row = sqlx::query_as::<_, UserRow>(
            "INSERT INTO users (email, password_hash, full_name, company_name) \
             VALUES ($1, $2, $3, $4) \
             RETURNING id, email, password_hash, full_name, company_name, created_at, updated_at",
        )
        .bind(&new_user.email)
        .bind(&new_user.password_hash)
        .bind(&new_user.full_name)
        .bind(&new_user.company_name)
        .fetch_one(&self.pool)
        .await?;

        Ok(User::from(row))
    }
}
