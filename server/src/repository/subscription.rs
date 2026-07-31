//! `subscriptions` table access (`LICENSE_DATABASE_SCHEMA.md` §1).

use crate::domain::{NewSubscription, PlanType, Subscription, SubscriptionStatus};
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
    /// Most recent subscription for a user regardless of status — unlike
    /// `find_active_by_user`, this also returns e.g. a `pending_payment`
    /// row (a checkout session was created but payment hasn't completed
    /// yet) or the most recent `cancelled`/`expired` one if that's all a
    /// user has. `GET /subscription` (`LicenseService::subscription_summary`)
    /// uses this so a caller checking in right after starting checkout gets
    /// a real, current status back instead of an error meant for "this
    /// user has never had a subscription at all."
    async fn find_latest_by_user(
        &self,
        user_id: i64,
    ) -> Result<Option<Subscription>, RepositoryError>;
    /// `POST /create-checkout-session` — a new row per checkout attempt,
    /// never a mutation of a past one (see `NewSubscription`'s doc
    /// comment).
    async fn insert(
        &self,
        new_subscription: NewSubscription,
    ) -> Result<Subscription, RepositoryError>;
    /// Transitions a subscription's status on a payment/webhook outcome
    /// (e.g. `pending_payment` → `active` on `payment.captured`,
    /// → `cancelled`/`suspended` on `subscription.cancelled`/`.halted`).
    async fn update_status(
        &self,
        id: i64,
        status: SubscriptionStatus,
        current_period_end: Option<DateTime<Utc>>,
    ) -> Result<(), RepositoryError>;
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

    async fn find_latest_by_user(
        &self,
        user_id: i64,
    ) -> Result<Option<Subscription>, RepositoryError> {
        let row = sqlx::query_as::<_, SubscriptionRow>(
            "SELECT id, user_id, plan_type, status, started_at, current_period_end, auto_renew, created_at, updated_at \
             FROM subscriptions WHERE user_id = $1 \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(Subscription::try_from).transpose()
    }

    async fn insert(
        &self,
        new_subscription: NewSubscription,
    ) -> Result<Subscription, RepositoryError> {
        let row = sqlx::query_as::<_, SubscriptionRow>(
            "INSERT INTO subscriptions (user_id, plan_type, status, started_at, current_period_end, auto_renew) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             RETURNING id, user_id, plan_type, status, started_at, current_period_end, auto_renew, created_at, updated_at",
        )
        .bind(new_subscription.user_id)
        .bind(new_subscription.plan_type.as_str())
        .bind(new_subscription.status.as_str())
        .bind(new_subscription.started_at)
        .bind(new_subscription.current_period_end)
        .bind(new_subscription.auto_renew)
        .fetch_one(&self.pool)
        .await?;

        Subscription::try_from(row)
    }

    async fn update_status(
        &self,
        id: i64,
        status: SubscriptionStatus,
        current_period_end: Option<DateTime<Utc>>,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE subscriptions SET status = $2, current_period_end = $3, updated_at = now() \
             WHERE id = $1",
        )
        .bind(id)
        .bind(status.as_str())
        .bind(current_period_end)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}
