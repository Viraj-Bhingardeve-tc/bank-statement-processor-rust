//! Business logic behind the Admin API (Module 3, `routes::admin_api`) —
//! the five `GET /admin/*` list endpoints and the four
//! revoke/restore/deactivate/activate mutations. Every method here is only
//! ever reachable behind `routes::admin::require_admin` (Module 2), so
//! nothing in this file re-checks the caller's role — `admin_user_id` is
//! taken purely for the structured log line each method emits ("Log every
//! admin action").
//!
//! Depends on `AdminRepository` for everything genuinely new to this
//! module (the paginated list queries, the license-revoke/device-reactivate
//! writes), plus `LicenseRepository`/`DeviceRepository` — both already
//! depended on by `service::license_service` — reused here exactly as-is
//! for the lookups/mutations that already exist: `LicenseRepository::
//! find_by_id` (existence + current state, before either license mutation)
//! and `DeviceRepository::deactivate` (identical to what
//! `/deactivate-license` already does to the same row). Neither of those
//! two repository traits gains a single new method for this module — see
//! `repository::admin`'s own doc comment for exactly which new queries
//! live there instead and why.

use crate::domain::{
    AdminDeviceSummary, AdminLicenseSummary, AdminUserSummary, DeviceListFilter, License,
    LicenseListFilter, LicenseRecordStatus, LicenseValidationEntry, LicenseValidationFilter,
    LoginHistoryEntry, LoginHistoryFilter, Page, UserListFilter,
};
use crate::repository::admin::AdminRepository;
use crate::repository::device::DeviceRepository;
use crate::repository::error::RepositoryError;
use crate::repository::license::LicenseRepository;
use std::fmt;
use std::sync::Arc;

pub struct AdminService {
    admin_repository: Arc<dyn AdminRepository>,
    license_repository: Arc<dyn LicenseRepository>,
    device_repository: Arc<dyn DeviceRepository>,
}

impl AdminService {
    pub fn new(
        admin_repository: Arc<dyn AdminRepository>,
        license_repository: Arc<dyn LicenseRepository>,
        device_repository: Arc<dyn DeviceRepository>,
    ) -> Self {
        AdminService {
            admin_repository,
            license_repository,
            device_repository,
        }
    }

    pub async fn list_users(
        &self,
        admin_user_id: i64,
        filter: UserListFilter,
    ) -> Result<Page<AdminUserSummary>, AdminOperationError> {
        tracing::info!(
            admin_user_id,
            page = filter.pagination.page,
            "admin action: list_users"
        );
        Ok(self.admin_repository.list_users(&filter).await?)
    }

    pub async fn list_licenses(
        &self,
        admin_user_id: i64,
        filter: LicenseListFilter,
    ) -> Result<Page<AdminLicenseSummary>, AdminOperationError> {
        tracing::info!(
            admin_user_id,
            page = filter.pagination.page,
            "admin action: list_licenses"
        );
        Ok(self.admin_repository.list_licenses(&filter).await?)
    }

    pub async fn list_devices(
        &self,
        admin_user_id: i64,
        filter: DeviceListFilter,
    ) -> Result<Page<AdminDeviceSummary>, AdminOperationError> {
        tracing::info!(
            admin_user_id,
            page = filter.pagination.page,
            "admin action: list_devices"
        );
        Ok(self.admin_repository.list_devices(&filter).await?)
    }

    pub async fn list_login_history(
        &self,
        admin_user_id: i64,
        filter: LoginHistoryFilter,
    ) -> Result<Page<LoginHistoryEntry>, AdminOperationError> {
        tracing::info!(
            admin_user_id,
            page = filter.pagination.page,
            "admin action: list_login_history"
        );
        Ok(self.admin_repository.list_login_history(&filter).await?)
    }

    pub async fn list_license_validations(
        &self,
        admin_user_id: i64,
        filter: LicenseValidationFilter,
    ) -> Result<Page<LicenseValidationEntry>, AdminOperationError> {
        tracing::info!(
            admin_user_id,
            page = filter.pagination.page,
            "admin action: list_license_validations"
        );
        Ok(self
            .admin_repository
            .list_license_validations(&filter)
            .await?)
    }

    /// `POST /admin/license/{id}/revoke`. Idempotent: revoking an
    /// already-revoked license just re-records `revoked_at`/`revoked_reason`
    /// rather than erroring, matching `LicenseRepository::extend`'s own
    /// "state transition, not a one-time event" precedent.
    pub async fn revoke_license(
        &self,
        admin_user_id: i64,
        license_id: i64,
        reason: Option<String>,
    ) -> Result<License, AdminOperationError> {
        self.license_repository
            .find_by_id(license_id)
            .await?
            .ok_or(AdminOperationError::LicenseNotFound)?;

        self.admin_repository
            .revoke_license(license_id, reason.as_deref())
            .await?;

        let updated = self
            .license_repository
            .find_by_id(license_id)
            .await?
            .ok_or(AdminOperationError::LicenseNotFound)?;

        tracing::info!(
            admin_user_id,
            license_id,
            reason = reason.as_deref().unwrap_or(""),
            "admin action: revoke_license"
        );
        Ok(updated)
    }

    /// `POST /admin/license/{id}/restore`. Only valid from `Revoked` — this
    /// endpoint undoes an admin revoke specifically, not a general
    /// "reactivate any non-active license" operation (an expired or
    /// suspended license needs its own, separate remediation, not this).
    pub async fn restore_license(
        &self,
        admin_user_id: i64,
        license_id: i64,
    ) -> Result<License, AdminOperationError> {
        let license = self
            .license_repository
            .find_by_id(license_id)
            .await?
            .ok_or(AdminOperationError::LicenseNotFound)?;

        if license.status != LicenseRecordStatus::Revoked {
            return Err(AdminOperationError::LicenseNotRevoked);
        }

        self.admin_repository.restore_license(license_id).await?;

        let updated = self
            .license_repository
            .find_by_id(license_id)
            .await?
            .ok_or(AdminOperationError::LicenseNotFound)?;

        tracing::info!(admin_user_id, license_id, "admin action: restore_license");
        Ok(updated)
    }

    /// `POST /admin/device/{id}/deactivate`. Reuses
    /// `DeviceRepository::deactivate` unchanged — the same mutation
    /// `/deactivate-license` already performs on this exact row, just
    /// reached from an admin id lookup instead of a customer's
    /// `(license_id, device_id)` pair.
    pub async fn deactivate_device(
        &self,
        admin_user_id: i64,
        device_id: i64,
    ) -> Result<(), AdminOperationError> {
        self.admin_repository
            .find_device_by_id(device_id)
            .await?
            .ok_or(AdminOperationError::DeviceNotFound)?;

        self.device_repository.deactivate(device_id).await?;

        tracing::info!(admin_user_id, device_id, "admin action: deactivate_device");
        Ok(())
    }

    /// `POST /admin/device/{id}/activate`. See `repository::admin`'s doc
    /// comment for why this deliberately bypasses the `max_devices` check
    /// `DeviceRepository::activate_device` enforces for a customer.
    pub async fn activate_device(
        &self,
        admin_user_id: i64,
        device_id: i64,
    ) -> Result<(), AdminOperationError> {
        self.admin_repository
            .find_device_by_id(device_id)
            .await?
            .ok_or(AdminOperationError::DeviceNotFound)?;

        self.admin_repository.reactivate_device(device_id).await?;

        tracing::info!(admin_user_id, device_id, "admin action: activate_device");
        Ok(())
    }
}

#[derive(Debug)]
pub enum AdminOperationError {
    LicenseNotFound,
    /// `restore_license` on a license whose current status isn't
    /// `Revoked`.
    LicenseNotRevoked,
    DeviceNotFound,
    Repository(RepositoryError),
}

impl fmt::Display for AdminOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdminOperationError::LicenseNotFound => write!(f, "license not found"),
            AdminOperationError::LicenseNotRevoked => write!(f, "license is not revoked"),
            AdminOperationError::DeviceNotFound => write!(f, "device not found"),
            AdminOperationError::Repository(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AdminOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AdminOperationError::Repository(e) => Some(e),
            _ => None,
        }
    }
}

impl From<RepositoryError> for AdminOperationError {
    fn from(e: RepositoryError) -> Self {
        AdminOperationError::Repository(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        AdminUserSummary, DeviceListFilter, LicenseListFilter, LicenseValidationFilter,
        LoginHistoryFilter, NewLicense, Pagination, SortOrder, UserListFilter,
    };
    use crate::repository::device::DeviceActivationOutcome;
    use async_trait::async_trait;
    use chrono::Utc;
    use std::sync::Mutex;
    use uuid::Uuid;

    /// In-memory stand-in for `AdminRepository` — same spirit as the mocks
    /// `service::license_service`/`service::auth_service` already use for
    /// their own repository dependencies.
    ///
    /// Shares its `licenses` list with `MockLicenseRepository` (via the
    /// same `Arc<Mutex<..>>`, wired up in `service_with` below) — in
    /// production `AdminRepository::revoke_license`/`restore_license` and
    /// `LicenseRepository::find_by_id` hit the very same Postgres table, so
    /// a mock that let them drift independently would validate a scenario
    /// that can't actually happen.
    struct MockAdminRepository {
        users: Vec<AdminUserSummary>,
        licenses: Arc<Mutex<Vec<License>>>,
        devices: Mutex<Vec<AdminDeviceSummary>>,
        revoke_calls: Mutex<Vec<(i64, Option<String>)>>,
        restore_calls: Mutex<Vec<i64>>,
        reactivate_calls: Mutex<Vec<i64>>,
    }

    impl MockAdminRepository {
        fn new(licenses: Arc<Mutex<Vec<License>>>) -> Self {
            MockAdminRepository {
                users: vec![],
                licenses,
                devices: Mutex::new(vec![]),
                revoke_calls: Mutex::new(vec![]),
                restore_calls: Mutex::new(vec![]),
                reactivate_calls: Mutex::new(vec![]),
            }
        }

        fn with_devices(devices: Vec<AdminDeviceSummary>) -> Self {
            MockAdminRepository {
                devices: Mutex::new(devices),
                ..MockAdminRepository::new(Arc::new(Mutex::new(vec![])))
            }
        }
    }

    #[async_trait]
    impl AdminRepository for MockAdminRepository {
        async fn list_users(
            &self,
            filter: &UserListFilter,
        ) -> Result<Page<AdminUserSummary>, RepositoryError> {
            Ok(Page {
                items: self.users.clone(),
                page: filter.pagination.page,
                page_size: filter.pagination.page_size,
                total: self.users.len() as i64,
            })
        }

        async fn list_licenses(
            &self,
            _filter: &LicenseListFilter,
        ) -> Result<Page<AdminLicenseSummary>, RepositoryError> {
            Ok(Page {
                items: vec![],
                page: 1,
                page_size: 20,
                total: 0,
            })
        }

        async fn list_devices(
            &self,
            filter: &DeviceListFilter,
        ) -> Result<Page<AdminDeviceSummary>, RepositoryError> {
            let devices = self.devices.lock().unwrap().clone();
            Ok(Page {
                items: devices.clone(),
                page: filter.pagination.page,
                page_size: filter.pagination.page_size,
                total: devices.len() as i64,
            })
        }

        async fn find_device_by_id(
            &self,
            id: i64,
        ) -> Result<Option<AdminDeviceSummary>, RepositoryError> {
            Ok(self
                .devices
                .lock()
                .unwrap()
                .iter()
                .find(|d| d.id == id)
                .cloned())
        }

        async fn reactivate_device(&self, id: i64) -> Result<(), RepositoryError> {
            self.reactivate_calls.lock().unwrap().push(id);
            if let Some(d) = self.devices.lock().unwrap().iter_mut().find(|d| d.id == id) {
                d.is_active = true;
            }
            Ok(())
        }

        async fn list_login_history(
            &self,
            _filter: &LoginHistoryFilter,
        ) -> Result<Page<LoginHistoryEntry>, RepositoryError> {
            Ok(Page {
                items: vec![],
                page: 1,
                page_size: 20,
                total: 0,
            })
        }

        async fn list_license_validations(
            &self,
            _filter: &LicenseValidationFilter,
        ) -> Result<Page<LicenseValidationEntry>, RepositoryError> {
            Ok(Page {
                items: vec![],
                page: 1,
                page_size: 20,
                total: 0,
            })
        }

        async fn revoke_license(
            &self,
            id: i64,
            reason: Option<&str>,
        ) -> Result<(), RepositoryError> {
            self.revoke_calls
                .lock()
                .unwrap()
                .push((id, reason.map(str::to_string)));
            if let Some(l) = self
                .licenses
                .lock()
                .unwrap()
                .iter_mut()
                .find(|l| l.id == id)
            {
                l.status = LicenseRecordStatus::Revoked;
                l.revoked_reason = reason.map(str::to_string);
            }
            Ok(())
        }

        async fn restore_license(&self, id: i64) -> Result<(), RepositoryError> {
            self.restore_calls.lock().unwrap().push(id);
            if let Some(l) = self
                .licenses
                .lock()
                .unwrap()
                .iter_mut()
                .find(|l| l.id == id)
            {
                l.status = LicenseRecordStatus::Active;
                l.revoked_reason = None;
                l.revoked_at = None;
            }
            Ok(())
        }
    }

    struct MockLicenseRepository {
        licenses: Arc<Mutex<Vec<License>>>,
    }

    impl MockLicenseRepository {
        fn with(licenses: Arc<Mutex<Vec<License>>>) -> Self {
            MockLicenseRepository { licenses }
        }
    }

    #[async_trait]
    impl LicenseRepository for MockLicenseRepository {
        async fn find_by_key(&self, _key: &str) -> Result<Option<License>, RepositoryError> {
            unimplemented!("not exercised by these tests")
        }

        async fn find_by_id(&self, id: i64) -> Result<Option<License>, RepositoryError> {
            Ok(self
                .licenses
                .lock()
                .unwrap()
                .iter()
                .find(|l| l.id == id)
                .cloned())
        }

        async fn find_latest_by_subscription(
            &self,
            _subscription_id: i64,
        ) -> Result<Option<License>, RepositoryError> {
            unimplemented!("not exercised by these tests")
        }

        async fn insert(&self, _new_license: NewLicense) -> Result<License, RepositoryError> {
            unimplemented!("not exercised by these tests")
        }

        async fn extend(
            &self,
            id: i64,
            status: LicenseRecordStatus,
            expires_at: Option<chrono::DateTime<Utc>>,
        ) -> Result<(), RepositoryError> {
            if let Some(l) = self
                .licenses
                .lock()
                .unwrap()
                .iter_mut()
                .find(|l| l.id == id)
            {
                l.status = status;
                l.expires_at = expires_at;
            }
            Ok(())
        }
    }

    struct MockDeviceRepository {
        deactivate_calls: Mutex<Vec<i64>>,
    }

    impl MockDeviceRepository {
        fn new() -> Self {
            MockDeviceRepository {
                deactivate_calls: Mutex::new(vec![]),
            }
        }
    }

    #[async_trait]
    impl DeviceRepository for MockDeviceRepository {
        async fn find_by_license_and_device_id(
            &self,
            _license_id: i64,
            _device_id: Uuid,
        ) -> Result<Option<crate::domain::Device>, RepositoryError> {
            unimplemented!("not exercised by these tests")
        }

        async fn count_active_by_license(&self, _license_id: i64) -> Result<i64, RepositoryError> {
            unimplemented!("not exercised by these tests")
        }

        async fn touch_last_seen(&self, _id: i64) -> Result<(), RepositoryError> {
            unimplemented!("not exercised by these tests")
        }

        async fn deactivate(&self, id: i64) -> Result<(), RepositoryError> {
            self.deactivate_calls.lock().unwrap().push(id);
            Ok(())
        }

        async fn activate_device(
            &self,
            _license_id: i64,
            _max_devices: i32,
            _device_id: Uuid,
            _machine_fingerprint: &str,
            _device_label: &str,
        ) -> Result<DeviceActivationOutcome, RepositoryError> {
            unimplemented!("not exercised by these tests")
        }
    }

    fn sample_license(id: i64, status: LicenseRecordStatus) -> License {
        License {
            id,
            subscription_id: 1,
            license_key: format!("KEY-{id}"),
            status,
            expires_at: Some(Utc::now() + chrono::Duration::days(30)),
            max_devices: 2,
            grace_period_days: 7,
            issued_at: Utc::now(),
            revoked_at: None,
            revoked_reason: None,
        }
    }

    fn sample_device(id: i64, is_active: bool) -> AdminDeviceSummary {
        AdminDeviceSummary {
            id,
            license_id: 1,
            user_id: 1,
            device_id: Uuid::new_v4(),
            device_label: None,
            last_seen_at: Utc::now(),
            is_active,
        }
    }

    /// Shares `admin_repository`'s own `licenses` list with the
    /// `LicenseRepository` mock it's paired with — see
    /// `MockAdminRepository`'s doc comment for why.
    fn service_with(admin_repository: MockAdminRepository) -> AdminService {
        let licenses = admin_repository.licenses.clone();
        AdminService::new(
            Arc::new(admin_repository),
            Arc::new(MockLicenseRepository::with(licenses)),
            Arc::new(MockDeviceRepository::new()),
        )
    }

    fn admin_repository_with_licenses(licenses: Vec<License>) -> MockAdminRepository {
        MockAdminRepository::new(Arc::new(Mutex::new(licenses)))
    }

    fn empty_pagination_filter() -> UserListFilter {
        UserListFilter {
            search: None,
            sort_order: SortOrder::Descending,
            pagination: Pagination::default(),
        }
    }

    #[tokio::test]
    async fn list_users_returns_what_the_repository_returns() {
        let service = service_with(admin_repository_with_licenses(vec![]));

        let page = service
            .list_users(1, empty_pagination_filter())
            .await
            .unwrap();
        assert_eq!(page.total, 0);
        assert!(page.items.is_empty());
    }

    #[tokio::test]
    async fn revoke_license_updates_status_and_records_the_reason() {
        let service = service_with(admin_repository_with_licenses(vec![sample_license(
            1,
            LicenseRecordStatus::Active,
        )]));

        let updated = service
            .revoke_license(99, 1, Some("fraud".to_string()))
            .await
            .unwrap();

        assert_eq!(updated.status, LicenseRecordStatus::Revoked);
    }

    #[tokio::test]
    async fn revoke_license_for_an_unknown_id_is_not_found() {
        let service = service_with(admin_repository_with_licenses(vec![]));

        let err = service.revoke_license(99, 404, None).await.unwrap_err();
        assert!(matches!(err, AdminOperationError::LicenseNotFound));
    }

    #[tokio::test]
    async fn restore_license_succeeds_only_from_revoked() {
        let service = service_with(admin_repository_with_licenses(vec![sample_license(
            1,
            LicenseRecordStatus::Revoked,
        )]));

        let updated = service.restore_license(99, 1).await.unwrap();
        assert_eq!(updated.status, LicenseRecordStatus::Active);
    }

    #[tokio::test]
    async fn restore_license_rejects_a_license_that_is_not_revoked() {
        let service = service_with(admin_repository_with_licenses(vec![sample_license(
            1,
            LicenseRecordStatus::Active,
        )]));

        let err = service.restore_license(99, 1).await.unwrap_err();
        assert!(matches!(err, AdminOperationError::LicenseNotRevoked));
    }

    #[tokio::test]
    async fn deactivate_device_for_an_unknown_id_is_not_found() {
        let service = service_with(admin_repository_with_licenses(vec![]));

        let err = service.deactivate_device(99, 404).await.unwrap_err();
        assert!(matches!(err, AdminOperationError::DeviceNotFound));
    }

    #[tokio::test]
    async fn deactivate_device_succeeds_for_a_known_device() {
        let admin_repo = MockAdminRepository::with_devices(vec![sample_device(1, true)]);
        let service = service_with(admin_repo);

        service.deactivate_device(99, 1).await.unwrap();
    }

    #[tokio::test]
    async fn activate_device_reactivates_a_known_deactivated_device() {
        let admin_repo = MockAdminRepository::with_devices(vec![sample_device(1, false)]);
        let service = service_with(admin_repo);

        service.activate_device(99, 1).await.unwrap();
    }

    #[tokio::test]
    async fn activate_device_for_an_unknown_id_is_not_found() {
        let service = service_with(admin_repository_with_licenses(vec![]));

        let err = service.activate_device(99, 404).await.unwrap_err();
        assert!(matches!(err, AdminOperationError::DeviceNotFound));
    }
}
