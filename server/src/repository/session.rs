//! `sessions` table access (`PHASE4_DESIGN.md` §7).

use crate::domain::{NewSession, Session};
use crate::repository::error::RepositoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

#[async_trait]
pub trait SessionRepository: Send + Sync {
    async fn insert(&self, new_session: NewSession) -> Result<Session, RepositoryError>;
    async fn find_by_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<Session>, RepositoryError>;
    /// `POST /logout` — sets `revoked_at`, never deletes the row (kept for
    /// audit, same reasoning as `license_validation_logs` being append-only).
    async fn revoke(&self, id: i64) -> Result<(), RepositoryError>;
}

pub struct PgSessionRepository {
    pool: PgPool,
}

impl PgSessionRepository {
    pub fn new(pool: PgPool) -> Self {
        PgSessionRepository { pool }
    }
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: i64,
    user_id: i64,
    token_hash: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
}

impl From<SessionRow> for Session {
    fn from(row: SessionRow) -> Self {
        Session {
            id: row.id,
            user_id: row.user_id,
            token_hash: row.token_hash,
            created_at: row.created_at,
            expires_at: row.expires_at,
            revoked_at: row.revoked_at,
        }
    }
}

#[async_trait]
impl SessionRepository for PgSessionRepository {
    async fn insert(&self, new_session: NewSession) -> Result<Session, RepositoryError> {
        let row = sqlx::query_as::<_, SessionRow>(
            "INSERT INTO sessions (user_id, token_hash, expires_at) \
             VALUES ($1, $2, $3) \
             RETURNING id, user_id, token_hash, created_at, expires_at, revoked_at",
        )
        .bind(new_session.user_id)
        .bind(&new_session.token_hash)
        .bind(new_session.expires_at)
        .fetch_one(&self.pool)
        .await?;

        Ok(Session::from(row))
    }

    async fn find_by_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<Session>, RepositoryError> {
        let row = sqlx::query_as::<_, SessionRow>(
            "SELECT id, user_id, token_hash, created_at, expires_at, revoked_at \
             FROM sessions WHERE token_hash = $1",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Session::from))
    }

    async fn revoke(&self, id: i64) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE sessions SET revoked_at = now() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }
}
