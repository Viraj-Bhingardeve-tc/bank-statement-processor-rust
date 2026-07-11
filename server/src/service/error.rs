//! Shared error type for every service.

use crate::repository::error::RepositoryError;
use std::fmt;

#[derive(Debug)]
pub enum ServiceError {
    Repository(RepositoryError),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServiceError::Repository(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ServiceError::Repository(e) => Some(e),
        }
    }
}

impl From<RepositoryError> for ServiceError {
    fn from(e: RepositoryError) -> Self {
        ServiceError::Repository(e)
    }
}
