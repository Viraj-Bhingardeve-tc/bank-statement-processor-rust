//! Business logic for license lookups.
//!
//! Phase 4C.2 scaffolding only: a thin pass-through proving the handler →
//! service → repository layering (`PHASE4_DESIGN.md` §1.2) actually works
//! and is independently testable — see this module's tests, which
//! substitute a mock `LicenseRepository`, no real database involved. Real
//! activation/validation business logic (device-limit enforcement, status
//! derivation against `LICENSE_SYSTEM_DESIGN.md` §4's flow) lands in a
//! later phase alongside the actual `/activate-license`/`/validate-license`
//! handlers that will call it.

use crate::domain::License;
use crate::repository::license::LicenseRepository;
use crate::service::error::ServiceError;
use std::sync::Arc;

pub struct LicenseService {
    license_repository: Arc<dyn LicenseRepository>,
}

impl LicenseService {
    pub fn new(license_repository: Arc<dyn LicenseRepository>) -> Self {
        LicenseService { license_repository }
    }

    pub async fn find_by_key(&self, license_key: &str) -> Result<Option<License>, ServiceError> {
        Ok(self.license_repository.find_by_key(license_key).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::LicenseRecordStatus;
    use crate::repository::error::RepositoryError;
    use async_trait::async_trait;
    use chrono::Utc;
    use std::sync::Mutex;

    /// A minimal hand-written stand-in for `LicenseRepository`, in the same
    /// spirit as the desktop app's own `MockClient`
    /// (`src/license/mod.rs`) — not a real database.
    struct MockLicenseRepository {
        by_key: Mutex<Option<License>>,
    }

    #[async_trait]
    impl LicenseRepository for MockLicenseRepository {
        async fn find_by_key(
            &self,
            _license_key: &str,
        ) -> Result<Option<License>, RepositoryError> {
            Ok(self.by_key.lock().unwrap().clone())
        }

        async fn find_by_id(&self, _id: i64) -> Result<Option<License>, RepositoryError> {
            unimplemented!("not exercised by these tests")
        }
    }

    fn sample_license() -> License {
        License {
            id: 1,
            subscription_id: 1,
            license_key: "TEST-KEY".to_string(),
            status: LicenseRecordStatus::Active,
            expires_at: None,
            max_devices: 1,
            grace_period_days: 7,
            issued_at: Utc::now(),
            revoked_at: None,
            revoked_reason: None,
        }
    }

    #[tokio::test]
    async fn find_by_key_returns_what_the_repository_returns() {
        let repo = Arc::new(MockLicenseRepository {
            by_key: Mutex::new(Some(sample_license())),
        });
        let service = LicenseService::new(repo);

        let found = service.find_by_key("TEST-KEY").await.unwrap();
        assert_eq!(found.unwrap().license_key, "TEST-KEY");
    }

    #[tokio::test]
    async fn find_by_key_returns_none_when_the_repository_has_nothing() {
        let repo = Arc::new(MockLicenseRepository {
            by_key: Mutex::new(None),
        });
        let service = LicenseService::new(repo);

        let found = service.find_by_key("NOPE").await.unwrap();
        assert!(found.is_none());
    }
}
