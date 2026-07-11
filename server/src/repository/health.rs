//! The one query this phase actually needs: "is the database reachable and
//! answering real queries," backing `/readyz`.

use sqlx::PgPool;

/// Runs a trivial `SELECT 1` — proves the pool can acquire a connection and
/// the server round-trips a real query against Postgres, not merely that a
/// `PgPool` value exists in memory (which `connect_lazy` alone would not
/// prove, per `db::build_pool`'s doc comment).
pub async fn ping(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(pool)
        .await?;
    Ok(())
}
