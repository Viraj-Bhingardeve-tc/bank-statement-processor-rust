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
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepositoryError::Database(e) => write!(f, "database error: {e}"),
            RepositoryError::InvalidData(msg) => write!(f, "invalid stored data: {msg}"),
        }
    }
}

impl std::error::Error for RepositoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RepositoryError::Database(e) => Some(e),
            RepositoryError::InvalidData(_) => None,
        }
    }
}

impl From<sqlx::Error> for RepositoryError {
    fn from(e: sqlx::Error) -> Self {
        RepositoryError::Database(e)
    }
}
