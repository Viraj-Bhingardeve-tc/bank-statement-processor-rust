//! Shared error type for every repository implementation.

use std::fmt;

#[derive(Debug)]
pub enum RepositoryError {
    /// A `sqlx` failure — connection, query syntax, constraint violation.
    Database(sqlx::Error),
    /// The query succeeded but a stored value doesn't map onto a domain
    /// type (e.g. a `status` column holding a string no longer recognized —
    /// a version skew between the running binary and the schema) —
    /// distinct from a `sqlx::Error` since the query itself was fine.
    InvalidData(String),
    /// Production Hardening, Finding H2: more than one `payments` row
    /// shares the given `provider_ref` — `repository::payment::
    /// PgPaymentRepository::find_by_provider_ref` used to mask this by
    /// silently picking the most recently created match; it now surfaces
    /// the ambiguity explicitly instead of guessing which row a webhook
    /// concerns. Migration `0008`'s partial `UNIQUE` index makes this
    /// unreachable against a freshly migrated schema — this variant exists
    /// for defense-in-depth against pre-migration data and in case that
    /// constraint is ever dropped, not because it's expected to fire in
    /// normal operation.
    DuplicateProviderReference(String),
    /// End-to-end payment testing pass (Phase 4N): the exact same class of
    /// bug Finding H2 fixed for `provider_ref`, found to still be present
    /// for `gateway_payment_id` — `repository::payment::
    /// PgPaymentRepository::find_by_gateway_payment_id` used to mask a
    /// collision by silently picking the most recently created matching
    /// row (`ORDER BY created_at DESC LIMIT 1`) instead of erroring, which
    /// is exactly the H2 footgun applied to the column
    /// `refund.*`/`payment.dispute.*` correlation reads. Migration `0009`'s
    /// partial `UNIQUE` index makes this unreachable against a freshly
    /// migrated schema, same defense-in-depth reasoning as
    /// `DuplicateProviderReference` above.
    DuplicateGatewayPaymentId(String),
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepositoryError::Database(e) => write!(f, "database error: {e}"),
            RepositoryError::InvalidData(msg) => write!(f, "invalid stored data: {msg}"),
            RepositoryError::DuplicateProviderReference(provider_ref) => write!(
                f,
                "multiple payments rows share provider_ref {provider_ref:?}"
            ),
            RepositoryError::DuplicateGatewayPaymentId(gateway_payment_id) => write!(
                f,
                "multiple payments rows share gateway_payment_id {gateway_payment_id:?}"
            ),
        }
    }
}

impl std::error::Error for RepositoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RepositoryError::Database(e) => Some(e),
            RepositoryError::InvalidData(_) => None,
            RepositoryError::DuplicateProviderReference(_) => None,
            RepositoryError::DuplicateGatewayPaymentId(_) => None,
        }
    }
}

impl From<sqlx::Error> for RepositoryError {
    fn from(e: sqlx::Error) -> Self {
        RepositoryError::Database(e)
    }
}
