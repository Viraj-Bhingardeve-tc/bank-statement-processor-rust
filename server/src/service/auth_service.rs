//! Business logic for the server-account (`users` table) lookups behind
//! `POST /login` — unrelated to the desktop's own `auth::validate_credentials`
//! monthly-password gate (`LICENSE_SYSTEM_DESIGN.md` §1).
//!
//! Phase 4C.2 scaffolding only: a thin pass-through proving the layering —
//! see this module's tests, which substitute a mock `UserRepository`. Real
//! login business logic (password verification, session issuance) lands in
//! a later phase alongside the actual `/login` handler.

use crate::domain::User;
use crate::repository::user::UserRepository;
use crate::service::error::ServiceError;
use std::sync::Arc;

pub struct AuthService {
    user_repository: Arc<dyn UserRepository>,
}

impl AuthService {
    pub fn new(user_repository: Arc<dyn UserRepository>) -> Self {
        AuthService { user_repository }
    }

    pub async fn find_by_email(&self, email: &str) -> Result<Option<User>, ServiceError> {
        Ok(self.user_repository.find_by_email(email).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::NewUser;
    use crate::repository::error::RepositoryError;
    use async_trait::async_trait;
    use chrono::Utc;
    use std::sync::Mutex;

    /// A minimal hand-written stand-in for `UserRepository`, in the same
    /// spirit as the desktop app's own `MockClient`
    /// (`src/license/mod.rs`) — not a real database.
    struct MockUserRepository {
        by_email: Mutex<Option<User>>,
    }

    #[async_trait]
    impl UserRepository for MockUserRepository {
        async fn find_by_email(&self, _email: &str) -> Result<Option<User>, RepositoryError> {
            Ok(self.by_email.lock().unwrap().clone())
        }

        async fn insert(&self, _new_user: NewUser) -> Result<User, RepositoryError> {
            unimplemented!("not exercised by these tests")
        }
    }

    fn sample_user() -> User {
        User {
            id: 1,
            email: "customer@example.com".to_string(),
            password_hash: "hashed".to_string(),
            full_name: None,
            company_name: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn find_by_email_returns_what_the_repository_returns() {
        let repo = Arc::new(MockUserRepository {
            by_email: Mutex::new(Some(sample_user())),
        });
        let service = AuthService::new(repo);

        let found = service.find_by_email("customer@example.com").await.unwrap();
        assert_eq!(found.unwrap().email, "customer@example.com");
    }

    #[tokio::test]
    async fn find_by_email_returns_none_when_the_repository_has_nothing() {
        let repo = Arc::new(MockUserRepository {
            by_email: Mutex::new(None),
        });
        let service = AuthService::new(repo);

        let found = service.find_by_email("nobody@example.com").await.unwrap();
        assert!(found.is_none());
    }
}
