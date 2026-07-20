//! Audit-log writes for `login_history`/`license_validation_logs`
//! (`repository::audit`). The one deliberate exception to this codebase's
//! usual "service methods are plain `async fn`, caller awaits them" shape
//! (`service::auth_service`/`service::license_service`): every method here
//! is synchronous and spawns its own write via `tokio::spawn`, so calling
//! one can never add latency to, block, or fail whatever request is
//! recording it — an audit write is a side effect of a request that has
//! already succeeded or failed on its own terms, never a precondition for
//! it. A write that itself fails (database unreachable, constraint
//! violation) is logged at `error` level and otherwise dropped; there is
//! no retry and nothing propagates back to the caller.

use crate::domain::{NewLicenseValidationLogEntry, NewLoginHistoryEntry, ValidationLogResult};
use crate::repository::audit::AuditRepository;
use std::sync::Arc;
use uuid::Uuid;

pub struct AuditService {
    repository: Arc<dyn AuditRepository>,
}

impl AuditService {
    pub fn new(repository: Arc<dyn AuditRepository>) -> Self {
        AuditService { repository }
    }

    /// Records one `/login` attempt. Callers must only pass a real,
    /// already-resolved `user_id` — see `AuditRepository::record_login`'s
    /// doc comment for why an attempt against an unrecognized email has
    /// nothing valid to record and must not call this at all.
    pub fn record_login(&self, user_id: i64, device_id: Option<Uuid>, success: bool) {
        let repository = Arc::clone(&self.repository);
        tokio::spawn(async move {
            let entry = NewLoginHistoryEntry {
                user_id,
                device_id,
                success,
            };
            if let Err(error) = repository.record_login(entry).await {
                tracing::error!(
                    error = %error,
                    user_id,
                    success,
                    "failed to record login_history audit entry"
                );
            }
        });
    }

    /// Records one `/activate-license`, `/validate-license`, or
    /// `/heartbeat` outcome. `result` is deliberately typed as
    /// [`ValidationLogResult`], not the full `LicenseOperationError` —
    /// callers only invoke this for outcomes that actually fit the
    /// `license_validation_logs.result` column's `CHECK` constraint (see
    /// that migration's doc comment); there is no "other" case to handle
    /// here.
    pub fn record_validation(&self, license_id: i64, device_id: Uuid, result: ValidationLogResult) {
        let repository = Arc::clone(&self.repository);
        tokio::spawn(async move {
            let entry = NewLicenseValidationLogEntry {
                license_id,
                device_id,
                result,
                client_clock: None,
            };
            if let Err(error) = repository.record_validation(entry).await {
                tracing::error!(
                    error = %error,
                    license_id,
                    device_id = %device_id,
                    result = result.as_str(),
                    "failed to record license_validation_logs audit entry"
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::error::RepositoryError;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::time::{sleep, Duration};

    /// Records every call it receives (rather than being a true no-op like
    /// `repository::audit::NoopAuditRepository`) so these tests can assert
    /// the spawned write actually happened, and a second variant that
    /// always fails so the "log and continue" contract is exercised too.
    struct RecordingAuditRepository {
        logins: Mutex<Vec<NewLoginHistoryEntry>>,
        validations: Mutex<Vec<NewLicenseValidationLogEntry>>,
        fail: bool,
        call_count: AtomicUsize,
    }

    impl RecordingAuditRepository {
        fn new(fail: bool) -> Self {
            RecordingAuditRepository {
                logins: Mutex::new(Vec::new()),
                validations: Mutex::new(Vec::new()),
                fail,
                call_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl AuditRepository for RecordingAuditRepository {
        async fn record_login(&self, entry: NewLoginHistoryEntry) -> Result<(), RepositoryError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(RepositoryError::InvalidData(
                    "simulated failure".to_string(),
                ));
            }
            self.logins.lock().unwrap().push(entry);
            Ok(())
        }

        async fn record_validation(
            &self,
            entry: NewLicenseValidationLogEntry,
        ) -> Result<(), RepositoryError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(RepositoryError::InvalidData(
                    "simulated failure".to_string(),
                ));
            }
            self.validations.lock().unwrap().push(entry);
            Ok(())
        }
    }

    /// Spawned tasks run on the same multi-threaded test runtime but aren't
    /// guaranteed to have completed the instant `record_login`/
    /// `record_validation` returns — a short yield is enough on a runtime
    /// with worker threads free to pick the task up immediately.
    async fn let_spawned_task_run() {
        sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_login_writes_the_entry_via_the_repository() {
        let repository = Arc::new(RecordingAuditRepository::new(false));
        let service = AuditService::new(repository.clone());

        service.record_login(42, None, true);
        let_spawned_task_run().await;

        let logins = repository.logins.lock().unwrap();
        assert_eq!(logins.len(), 1);
        assert_eq!(logins[0].user_id, 42);
        assert!(logins[0].success);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_login_swallows_a_repository_failure_without_panicking() {
        let repository = Arc::new(RecordingAuditRepository::new(true));
        let service = AuditService::new(repository.clone());

        service.record_login(42, None, false);
        let_spawned_task_run().await;

        assert_eq!(repository.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_validation_writes_the_entry_via_the_repository() {
        let repository = Arc::new(RecordingAuditRepository::new(false));
        let service = AuditService::new(repository.clone());
        let device_id = Uuid::new_v4();

        service.record_validation(7, device_id, ValidationLogResult::Valid);
        let_spawned_task_run().await;

        let validations = repository.validations.lock().unwrap();
        assert_eq!(validations.len(), 1);
        assert_eq!(validations[0].license_id, 7);
        assert_eq!(validations[0].device_id, device_id);
        assert_eq!(validations[0].result, ValidationLogResult::Valid);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_validation_swallows_a_repository_failure_without_panicking() {
        let repository = Arc::new(RecordingAuditRepository::new(true));
        let service = AuditService::new(repository.clone());

        service.record_validation(7, Uuid::new_v4(), ValidationLogResult::Revoked);
        let_spawned_task_run().await;

        assert_eq!(repository.call_count.load(Ordering::SeqCst), 1);
    }
}
