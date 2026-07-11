//! `subscriptions` table access (`LICENSE_DATABASE_SCHEMA.md` §1).

use crate::domain::{PlanType, Subscription, SubscriptionStatus};
use crate::repository::error::RepositoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::str::FromStr;

#[async_trait]
pub trait SubscriptionRepository: Send + Sync {
    async fn find_by_id(&self, id: i64) -> Result<Option<Subscription>, RepositoryError>;
    /// Most recent `active` subscription for a user — a user can have more
    /// than one subscription over time (renewals, upgrades), but only ever
    /// one that's currently active (`LICENSE_DATABASE_SCHEMA.md` §1's
    /// comment on this table).
    async fn find_active_by_user(
        &self,
        user_id: i64,
    ) -> Result<Option<Subscription>, RepositoryError>;
}

pub struct PgSubscriptionRepository {
    pool: PgPool,
}

impl PgSubscriptionRepository {
    pub fn new(pool: PgPool) -> Self {
        PgSubscriptionRepository { pool }
    }
}

#[derive(sqlx::FromRow)]
struct SubscriptionRow {
    id: i64,
    user_id: i64,
    plan_type: String,
    status: String,
    started_at: DateTime<Utc>,
    current_period_end: Option<DateTime<Utc>>,
    auto_renew: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<SubscriptionRow> for Subscription {
    type Error = RepositoryError;

    fn try_from(row: SubscriptionRow) -> Result<Self, Self::Error> {
        Ok(Subscription {
            id: row.id,
            user_id: row.user_id,
            plan_type: PlanType::from_str(&row.plan_type).map_err(RepositoryError::InvalidData)?,
            status: SubscriptionStatus::from_str(&row.status)
                .map_err(RepositoryError::InvalidData)?,
            started_at: row.started_at,
            current_period_end: row.current_period_end,
            auto_renew: row.auto_renew,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[async_trait]
impl SubscriptionRepository for PgSubscriptionRepository {
    async fn find_by_id(&self, id: i64) -> Result<Option<Subscription>, RepositoryError> {
        let row = sqlx::query_as::<_, SubscriptionRow>(
            "SELECT id, user_id, plan_type, status, started_at, current_period_end, auto_renew, created_at, updated_at \
             FROM subscriptions WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(Subscription::try_from).transpose()
    }

    async fn find_active_by_user(
        &self,
        user_id: i64,
    ) -> Result<Option<Subscription>, RepositoryError> {
        let row = sqlx::query_as::<_, SubscriptionRow>(
            "SELECT id, user_id, plan_type, status, started_at, current_period_end, auto_renew, created_at, updated_at \
             FROM subscriptions WHERE user_id = $1 AND status = 'active' \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(Subscription::try_from).transpose()
    }
}
