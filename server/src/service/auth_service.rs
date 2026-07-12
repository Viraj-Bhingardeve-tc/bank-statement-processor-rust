//! Business logic for server-account authentication: `POST /login`
//! (password verification + session issuance), session validation (the
//! authentication middleware's one real dependency —
//! `routes::auth::require_session`), and `POST /logout` (session
//! invalidation).
//!
//! Unrelated to the desktop's own `auth::validate_credentials`
//! monthly-password gate (`LICENSE_SYSTEM_DESIGN.md` §1) — this is the
//! *server* account (`users` table), a different credential entirely.
//!
//! Depends only on repository *traits*, never a concrete `Pg*`
//! implementation, so the full login/validate/logout flow is unit-tested
//! against hand-written in-memory mocks below — no real database and no
//! HTTP framework type anywhere in this file, same pattern
//! `service::license_service` already established in Phase 4D.

use crate::auth::password::verify_password;
use crate::auth::token::{generate_session_token, hash_token};
use crate::domain::{NewSession, Session, User};
use crate::repository::error::RepositoryError;
use crate::repository::session::SessionRepository;
use crate::repository::user::UserRepository;
use crate::service::error::ServiceError;
use chrono::{DateTime, Duration, Utc};
use std::fmt;
use std::sync::Arc;

/// Session lifetime from issuance — matches the ~30-day example in
/// `API_SPECIFICATION.md`'s `/login` response. Not user-configurable in
/// this phase; revisit if a "remember me" / shorter-lived-session product
/// requirement ever appears.
const SESSION_LIFETIME_DAYS: i64 = 30;

pub struct AuthService {
    user_repository: Arc<dyn UserRepository>,
    session_repository: Arc<dyn SessionRepository>,
}

impl AuthService {
    pub fn new(
        user_repository: Arc<dyn UserRepository>,
        session_repository: Arc<dyn SessionRepository>,
    ) -> Self {
        AuthService {
            user_repository,
            session_repository,
        }
    }

    pub async fn find_by_email(&self, email: &str) -> Result<Option<User>, ServiceError> {
        Ok(self.user_repository.find_by_email(email).await?)
    }

    /// `POST /login`. Verifies the account password (Argon2) and, on
    /// success, issues a fresh session token — returned to the caller
    /// exactly once; only its SHA-256 hash is ever persisted
    /// (`PHASE4_DESIGN.md` §1.3/§6). Deliberately returns the same
    /// `InvalidCredentials` error whether the email is unknown or the
    /// password is wrong — distinguishing the two in the response would
    /// let a caller enumerate registered emails.
    pub async fn login(&self, email: &str, password: &str) -> Result<LoginOutcome, AuthError> {
        let user = self
            .user_repository
            .find_by_email(email)
            .await?
            .ok_or(AuthError::InvalidCredentials)?;

        let matches = verify_password(password, &user.password_hash)
            .map_err(|e| AuthError::Repository(RepositoryError::InvalidData(e.to_string())))?;
        if !matches {
            return Err(AuthError::InvalidCredentials);
        }

        let token = generate_session_token();
        let expires_at = Utc::now() + Duration::days(SESSION_LIFETIME_DAYS);
        let session = self
            .session_repository
            .insert(NewSession {
                user_id: user.id,
                token_hash: hash_token(&token),
                expires_at,
            })
            .await?;

        Ok(LoginOutcome {
            session_token: token,
            user_id: user.id,
            expires_at: session.expires_at,
        })
    }

    /// Resolves a bearer token to its still-valid `Session` — used by
    /// `routes::auth::require_session`. Fails closed: an unknown, expired,
    /// or revoked token all resolve to the same `Unauthorized`, never a
    /// distinct reason — telling those apart would let a caller learn
    /// whether a guessed token was ever real, same reasoning `login`
    /// already applies to unknown-email vs. wrong-password.
    pub async fn validate_session(&self, token: &str) -> Result<Session, AuthError> {
        let session = self
            .session_repository
            .find_by_token_hash(&hash_token(token))
            .await?
            .ok_or(AuthError::Unauthorized)?;

        if session.revoked_at.is_some() || session.expires_at <= Utc::now() {
            return Err(AuthError::Unauthorized);
        }

        Ok(session)
    }

    /// `POST /logout`. Takes the already-authenticated session's id — the
    /// middleware has already resolved and validated the bearer token by
    /// the time a handler can call this, so there's no reason to re-parse
    /// it here, and no way for this to disagree with what the middleware
    /// just confirmed.
    pub async fn logout(&self, session_id: i64) -> Result<(), AuthError> {
        self.session_repository.revoke(session_id).await?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct LoginOutcome {
    pub session_token: String,
    pub user_id: i64,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug)]
pub enum AuthError {
    InvalidCredentials,
    Unauthorized,
    Repository(RepositoryError),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::InvalidCredentials => write!(f, "invalid credentials"),
            AuthError::Unauthorized => write!(f, "unauthorized"),
            AuthError::Repository(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AuthError::Repository(e) => Some(e),
            _ => None,
        }
    }
}

impl From<RepositoryError> for AuthError {
    fn from(e: RepositoryError) -> Self {
        AuthError::Repository(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::password::hash_password;
    use crate::domain::NewUser;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// In-memory stand-ins for the repository traits, same spirit as
    /// `service::license_service`'s mocks — real enough to exercise
    /// multi-step flows (find user, then insert a session, then find that
    /// session again) without a real database.
    struct MockUserRepository {
        users: Mutex<Vec<User>>,
    }

    impl MockUserRepository {
        fn with(users: Vec<User>) -> Self {
            MockUserRepository {
                users: Mutex::new(users),
            }
        }
    }

    #[async_trait]
    impl UserRepository for MockUserRepository {
        async fn find_by_email(&self, email: &str) -> Result<Option<User>, RepositoryError> {
            Ok(self
                .users
                .lock()
                .unwrap()
                .iter()
                .find(|u| u.email == email)
                .cloned())
        }

        async fn insert(&self, _new_user: NewUser) -> Result<User, RepositoryError> {
            unimplemented!("not exercised by these tests")
        }
    }

    struct MockSessionRepository {
        sessions: Mutex<Vec<Session>>,
        next_id: Mutex<i64>,
    }

    impl MockSessionRepository {
        fn with(sessions: Vec<Session>) -> Self {
            let next_id = sessions.iter().map(|s| s.id).max().unwrap_or(0) + 1;
            MockSessionRepository {
                sessions: Mutex::new(sessions),
                next_id: Mutex::new(next_id),
            }
        }
    }

    #[async_trait]
    impl SessionRepository for MockSessionRepository {
        async fn insert(&self, new_session: NewSession) -> Result<Session, RepositoryError> {
            let mut next_id = self.next_id.lock().unwrap();
            let id = *next_id;
            *next_id += 1;
            let session = Session {
                id,
                user_id: new_session.user_id,
                token_hash: new_session.token_hash,
                created_at: Utc::now(),
                expires_at: new_session.expires_at,
                revoked_at: None,
            };
            self.sessions.lock().unwrap().push(session.clone());
            Ok(session)
        }

        async fn find_by_token_hash(
            &self,
            token_hash: &str,
        ) -> Result<Option<Session>, RepositoryError> {
            Ok(self
                .sessions
                .lock()
                .unwrap()
                .iter()
                .find(|s| s.token_hash == token_hash)
                .cloned())
        }

        async fn revoke(&self, id: i64) -> Result<(), RepositoryError> {
            if let Some(s) = self
                .sessions
                .lock()
                .unwrap()
                .iter_mut()
                .find(|s| s.id == id)
            {
                s.revoked_at = Some(Utc::now());
            }
            Ok(())
        }
    }

    fn sample_user(email: &str, password: &str) -> User {
        User {
            id: 1,
            email: email.to_string(),
            password_hash: hash_password(password).unwrap(),
            full_name: None,
            company_name: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn service_with(users: Vec<User>, sessions: Vec<Session>) -> AuthService {
        AuthService::new(
            Arc::new(MockUserRepository::with(users)),
            Arc::new(MockSessionRepository::with(sessions)),
        )
    }

    #[tokio::test]
    async fn find_by_email_returns_what_the_repository_returns() {
        let service = service_with(vec![sample_user("customer@example.com", "pw")], vec![]);

        let found = service.find_by_email("customer@example.com").await.unwrap();
        assert_eq!(found.unwrap().email, "customer@example.com");
    }

    #[tokio::test]
    async fn find_by_email_returns_none_when_the_repository_has_nothing() {
        let service = service_with(vec![], vec![]);

        let found = service.find_by_email("nobody@example.com").await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn login_with_correct_credentials_issues_a_session() {
        let service = service_with(
            vec![sample_user("customer@example.com", "correct-password")],
            vec![],
        );

        let outcome = service
            .login("customer@example.com", "correct-password")
            .await
            .unwrap();
        assert_eq!(outcome.user_id, 1);
        assert_eq!(
            outcome.session_token.len(),
            64,
            "expected a 256-bit hex token"
        );
        assert!(outcome.expires_at > Utc::now());
    }

    #[tokio::test]
    async fn login_with_wrong_password_is_rejected() {
        let service = service_with(
            vec![sample_user("customer@example.com", "correct-password")],
            vec![],
        );

        let err = service
            .login("customer@example.com", "wrong-password")
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::InvalidCredentials));
    }

    #[tokio::test]
    async fn login_with_unknown_email_is_rejected_the_same_way_as_wrong_password() {
        let service = service_with(vec![], vec![]);

        let err = service
            .login("nobody@example.com", "anything")
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::InvalidCredentials));
    }

    #[tokio::test]
    async fn a_freshly_issued_session_validates_successfully() {
        let service = service_with(vec![sample_user("customer@example.com", "pw")], vec![]);
        let outcome = service.login("customer@example.com", "pw").await.unwrap();

        let session = service
            .validate_session(&outcome.session_token)
            .await
            .unwrap();
        assert_eq!(session.user_id, 1);
    }

    #[tokio::test]
    async fn validate_session_rejects_an_unknown_token() {
        let service = service_with(vec![], vec![]);

        let err = service
            .validate_session("not-a-real-token")
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Unauthorized));
    }

    #[tokio::test]
    async fn validate_session_rejects_an_expired_session() {
        let expired = Session {
            id: 1,
            user_id: 1,
            token_hash: hash_token("some-token"),
            created_at: Utc::now() - Duration::days(40),
            expires_at: Utc::now() - Duration::days(10),
            revoked_at: None,
        };
        let service = service_with(vec![], vec![expired]);

        let err = service.validate_session("some-token").await.unwrap_err();
        assert!(matches!(err, AuthError::Unauthorized));
    }

    #[tokio::test]
    async fn validate_session_rejects_a_revoked_session() {
        let revoked = Session {
            id: 1,
            user_id: 1,
            token_hash: hash_token("some-token"),
            created_at: Utc::now() - Duration::days(1),
            expires_at: Utc::now() + Duration::days(29),
            revoked_at: Some(Utc::now()),
        };
        let service = service_with(vec![], vec![revoked]);

        let err = service.validate_session("some-token").await.unwrap_err();
        assert!(matches!(err, AuthError::Unauthorized));
    }

    #[tokio::test]
    async fn logout_revokes_the_session_so_it_no_longer_validates() {
        let service = service_with(vec![sample_user("customer@example.com", "pw")], vec![]);
        let outcome = service.login("customer@example.com", "pw").await.unwrap();
        let session = service
            .validate_session(&outcome.session_token)
            .await
            .unwrap();

        service.logout(session.id).await.unwrap();

        let err = service
            .validate_session(&outcome.session_token)
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Unauthorized));
    }
}
