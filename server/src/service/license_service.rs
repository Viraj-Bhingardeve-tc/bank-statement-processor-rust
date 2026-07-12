//! Business logic for license activation, validation, and deactivation
//! (`API_SPECIFICATION.md`'s `/activate-license`/`/validate-license`, plus
//! the additive `/deactivate-license` — see `license_protocol`'s doc
//! comment on `DeactivateLicenseRequest`).
//!
//! Every method here depends only on repository *traits*
//! (`Arc<dyn ...Repository>`), never a concrete `Pg*` implementation, so
//! the full activate/validate/deactivate flow is unit-tested against
//! hand-written in-memory mocks below — no real database involved, and no
//! HTTP framework type appears anywhere in this file (`PHASE4_DESIGN.md`
//! §1.2's "Services... independent of HTTP framework types").

use crate::domain::{Device, License, LicenseRecordStatus, NewDevice, PlanType};
use crate::repository::device::DeviceRepository;
use crate::repository::error::RepositoryError;
use crate::repository::license::LicenseRepository;
use crate::repository::subscription::SubscriptionRepository;
use chrono::{DateTime, Utc};
use std::fmt;
use std::sync::Arc;
use uuid::Uuid;

pub struct LicenseService {
    license_repository: Arc<dyn LicenseRepository>,
    device_repository: Arc<dyn DeviceRepository>,
    subscription_repository: Arc<dyn SubscriptionRepository>,
}

impl LicenseService {
    pub fn new(
        license_repository: Arc<dyn LicenseRepository>,
        device_repository: Arc<dyn DeviceRepository>,
        subscription_repository: Arc<dyn SubscriptionRepository>,
    ) -> Self {
        LicenseService {
            license_repository,
            device_repository,
            subscription_repository,
        }
    }

    pub async fn find_by_key(
        &self,
        license_key: &str,
    ) -> Result<Option<License>, LicenseOperationError> {
        Ok(self.license_repository.find_by_key(license_key).await?)
    }

    /// `POST /activate-license`. Binds a license key to a device
    /// (`LICENSE_SYSTEM_DESIGN.md` §4). Idempotent for a device that's
    /// already active on this license (a repeat call just refreshes
    /// `last_seen_at`, matching the desktop's own
    /// `repeated_check_status_calls_are_idempotent_on_device_identity`
    /// contract) — and reuses a previously-deactivated device's row rather
    /// than inserting a second one, since `(license_id, device_id)` is
    /// unique.
    pub async fn activate(
        &self,
        license_key: &str,
        device_id: Uuid,
        machine_fingerprint: &str,
        device_label: &str,
    ) -> Result<ActivationOutcome, LicenseOperationError> {
        let license = self
            .license_repository
            .find_by_key(license_key)
            .await?
            .ok_or(LicenseOperationError::LicenseNotFound)?;

        match license.status {
            LicenseRecordStatus::Revoked => return Err(LicenseOperationError::LicenseRevoked),
            LicenseRecordStatus::Expired => return Err(LicenseOperationError::LicenseExpired),
            // A suspended license may still be activated — validate-license
            // is what surfaces "suspended" to the caller as a non-error
            // status, per API_SPECIFICATION.md's error table only listing
            // NOT_FOUND/DEVICE_LIMIT/REVOKED/EXPIRED for this endpoint.
            LicenseRecordStatus::Active | LicenseRecordStatus::Suspended => {}
        }

        let existing = self
            .device_repository
            .find_by_license_and_device_id(license.id, device_id)
            .await?;

        match existing {
            Some(device) if device.deactivated_at.is_none() => {
                self.device_repository.touch_last_seen(device.id).await?;
            }
            Some(device) => {
                self.ensure_device_slot_available(&license).await?;
                self.device_repository.reactivate(device.id).await?;
            }
            None => {
                self.ensure_device_slot_available(&license).await?;
                self.device_repository
                    .insert(NewDevice {
                        license_id: license.id,
                        device_id,
                        machine_fingerprint: machine_fingerprint.to_string(),
                        device_label: Some(device_label.to_string()),
                    })
                    .await?;
            }
        }

        let subscription = self
            .subscription_repository
            .find_by_id(license.subscription_id)
            .await?
            .ok_or_else(|| {
                LicenseOperationError::Repository(RepositoryError::InvalidData(format!(
                    "license {} references missing subscription {}",
                    license.id, license.subscription_id
                )))
            })?;

        Ok(ActivationOutcome {
            customer_id: subscription.user_id,
            plan_type: subscription.plan_type,
            license,
        })
    }

    async fn ensure_device_slot_available(
        &self,
        license: &License,
    ) -> Result<(), LicenseOperationError> {
        let active_count = self
            .device_repository
            .count_active_by_license(license.id)
            .await?;
        if active_count >= i64::from(license.max_devices) {
            let existing = self
                .device_repository
                .list_active_by_license(license.id)
                .await?;
            return Err(LicenseOperationError::DeviceLimitReached(existing));
        }
        Ok(())
    }

    /// `POST /validate-license`. Called on every online app launch
    /// (`LICENSE_SYSTEM_DESIGN.md` §4). A fingerprint mismatch is reported
    /// back (`fingerprint_matched: false`) but never itself rejects the
    /// call — `LICENSE_SECURITY_REVIEW.md` §5 is explicit that this stays a
    /// logged signal, not an automatic block, until a deliberate server-
    /// side policy exists to act on the pattern across devices.
    pub async fn validate(
        &self,
        license_id: i64,
        device_id: Uuid,
        machine_fingerprint: &str,
    ) -> Result<ValidationOutcome, LicenseOperationError> {
        let license = self
            .license_repository
            .find_by_id(license_id)
            .await?
            .ok_or(LicenseOperationError::DeviceNotActivated)?;

        let device = self
            .device_repository
            .find_by_license_and_device_id(license.id, device_id)
            .await?
            .filter(|d| d.deactivated_at.is_none())
            .ok_or(LicenseOperationError::DeviceNotActivated)?;

        self.device_repository.touch_last_seen(device.id).await?;

        Ok(ValidationOutcome {
            status: license.status,
            expires_at: license.expires_at,
            grace_period_days: license.grace_period_days,
            fingerprint_matched: device.machine_fingerprint == machine_fingerprint,
        })
    }

    /// `POST /deactivate-license`. Frees a device slot — the customer-
    /// facing counterpart to the admin-surface `POST /devices/{id}/deactivate`
    /// `API_SPECIFICATION.md` mentions but doesn't specify (out of scope
    /// for that document's list of 7). Soft-delete only, never a row
    /// removal, same as every other status transition in this schema.
    pub async fn deactivate(
        &self,
        license_id: i64,
        device_id: Uuid,
    ) -> Result<DeactivationOutcome, LicenseOperationError> {
        let license = self
            .license_repository
            .find_by_id(license_id)
            .await?
            .ok_or(LicenseOperationError::LicenseNotFound)?;

        let device = self
            .device_repository
            .find_by_license_and_device_id(license.id, device_id)
            .await?
            .filter(|d| d.deactivated_at.is_none())
            .ok_or(LicenseOperationError::DeviceNotActivated)?;

        self.device_repository.deactivate(device.id).await?;
        let devices_active = self
            .device_repository
            .count_active_by_license(license.id)
            .await?;

        Ok(DeactivationOutcome { devices_active })
    }
}

#[derive(Debug)]
pub struct ActivationOutcome {
    pub license: License,
    pub customer_id: i64,
    pub plan_type: PlanType,
}

#[derive(Debug)]
pub struct ValidationOutcome {
    pub status: LicenseRecordStatus,
    pub expires_at: Option<DateTime<Utc>>,
    pub grace_period_days: i32,
    pub fingerprint_matched: bool,
}

#[derive(Debug)]
pub struct DeactivationOutcome {
    pub devices_active: i64,
}

#[derive(Debug)]
pub enum LicenseOperationError {
    LicenseNotFound,
    LicenseRevoked,
    LicenseExpired,
    DeviceNotActivated,
    /// Carries the existing active-device list, per `API_SPECIFICATION.md`'s
    /// `409 DEVICE_LIMIT_REACHED` documentation ("response includes the
    /// existing device list so the customer/admin can deactivate one").
    DeviceLimitReached(Vec<Device>),
    Repository(RepositoryError),
}

impl fmt::Display for LicenseOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LicenseOperationError::LicenseNotFound => write!(f, "license not found"),
            LicenseOperationError::LicenseRevoked => write!(f, "license has been revoked"),
            LicenseOperationError::LicenseExpired => write!(f, "license has expired"),
            LicenseOperationError::DeviceNotActivated => {
                write!(f, "device not activated for this license")
            }
            LicenseOperationError::DeviceLimitReached(_) => {
                write!(f, "device limit reached for this license")
            }
            LicenseOperationError::Repository(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LicenseOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LicenseOperationError::Repository(e) => Some(e),
            _ => None,
        }
    }
}

impl From<RepositoryError> for LicenseOperationError {
    fn from(e: RepositoryError) -> Self {
        LicenseOperationError::Repository(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Subscription, SubscriptionStatus};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// In-memory stand-ins for the repository traits, in the same spirit as
    /// the desktop app's own `MockClient` (`src/license/mod.rs`) — real
    /// enough to exercise multi-step flows (find, then insert, then find
    /// again) without a real database.
    struct MockLicenseRepository {
        licenses: Mutex<Vec<License>>,
    }

    impl MockLicenseRepository {
        fn with(licenses: Vec<License>) -> Self {
            MockLicenseRepository {
                licenses: Mutex::new(licenses),
            }
        }
    }

    #[async_trait]
    impl LicenseRepository for MockLicenseRepository {
        async fn find_by_key(&self, license_key: &str) -> Result<Option<License>, RepositoryError> {
            Ok(self
                .licenses
                .lock()
                .unwrap()
                .iter()
                .find(|l| l.license_key == license_key)
                .cloned())
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
    }

    struct MockDeviceRepository {
        devices: Mutex<Vec<Device>>,
        next_id: Mutex<i64>,
    }

    impl MockDeviceRepository {
        fn with(devices: Vec<Device>) -> Self {
            let next_id = devices.iter().map(|d| d.id).max().unwrap_or(0) + 1;
            MockDeviceRepository {
                devices: Mutex::new(devices),
                next_id: Mutex::new(next_id),
            }
        }
    }

    #[async_trait]
    impl DeviceRepository for MockDeviceRepository {
        async fn find_by_license_and_device_id(
            &self,
            license_id: i64,
            device_id: Uuid,
        ) -> Result<Option<Device>, RepositoryError> {
            Ok(self
                .devices
                .lock()
                .unwrap()
                .iter()
                .find(|d| d.license_id == license_id && d.device_id == device_id)
                .cloned())
        }

        async fn count_active_by_license(&self, license_id: i64) -> Result<i64, RepositoryError> {
            Ok(self
                .devices
                .lock()
                .unwrap()
                .iter()
                .filter(|d| d.license_id == license_id && d.deactivated_at.is_none())
                .count() as i64)
        }

        async fn list_active_by_license(
            &self,
            license_id: i64,
        ) -> Result<Vec<Device>, RepositoryError> {
            Ok(self
                .devices
                .lock()
                .unwrap()
                .iter()
                .filter(|d| d.license_id == license_id && d.deactivated_at.is_none())
                .cloned()
                .collect())
        }

        async fn insert(&self, new_device: NewDevice) -> Result<Device, RepositoryError> {
            let mut next_id = self.next_id.lock().unwrap();
            let id = *next_id;
            *next_id += 1;
            let now = Utc::now();
            let device = Device {
                id,
                license_id: new_device.license_id,
                device_id: new_device.device_id,
                machine_fingerprint: new_device.machine_fingerprint,
                device_label: new_device.device_label,
                first_seen_at: now,
                last_seen_at: now,
                deactivated_at: None,
            };
            self.devices.lock().unwrap().push(device.clone());
            Ok(device)
        }

        async fn touch_last_seen(&self, id: i64) -> Result<(), RepositoryError> {
            if let Some(d) = self.devices.lock().unwrap().iter_mut().find(|d| d.id == id) {
                d.last_seen_at = Utc::now();
            }
            Ok(())
        }

        async fn reactivate(&self, id: i64) -> Result<(), RepositoryError> {
            if let Some(d) = self.devices.lock().unwrap().iter_mut().find(|d| d.id == id) {
                d.deactivated_at = None;
                d.last_seen_at = Utc::now();
            }
            Ok(())
        }

        async fn deactivate(&self, id: i64) -> Result<(), RepositoryError> {
            if let Some(d) = self.devices.lock().unwrap().iter_mut().find(|d| d.id == id) {
                d.deactivated_at = Some(Utc::now());
            }
            Ok(())
        }
    }

    struct MockSubscriptionRepository {
        subscriptions: Mutex<Vec<Subscription>>,
    }

    impl MockSubscriptionRepository {
        fn with(subscriptions: Vec<Subscription>) -> Self {
            MockSubscriptionRepository {
                subscriptions: Mutex::new(subscriptions),
            }
        }
    }

    #[async_trait]
    impl SubscriptionRepository for MockSubscriptionRepository {
        async fn find_by_id(&self, id: i64) -> Result<Option<Subscription>, RepositoryError> {
            Ok(self
                .subscriptions
                .lock()
                .unwrap()
                .iter()
                .find(|s| s.id == id)
                .cloned())
        }

        async fn find_active_by_user(
            &self,
            user_id: i64,
        ) -> Result<Option<Subscription>, RepositoryError> {
            Ok(self
                .subscriptions
                .lock()
                .unwrap()
                .iter()
                .find(|s| s.user_id == user_id && s.status == SubscriptionStatus::Active)
                .cloned())
        }
    }

    fn sample_license(status: LicenseRecordStatus, max_devices: i32) -> License {
        License {
            id: 1,
            subscription_id: 10,
            license_key: "TEST-KEY".to_string(),
            status,
            expires_at: None,
            max_devices,
            grace_period_days: 7,
            issued_at: Utc::now(),
            revoked_at: None,
            revoked_reason: None,
        }
    }

    fn sample_subscription() -> Subscription {
        Subscription {
            id: 10,
            user_id: 100,
            plan_type: PlanType::Yearly,
            status: SubscriptionStatus::Active,
            started_at: Utc::now(),
            current_period_end: None,
            auto_renew: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn service_with(
        licenses: Vec<License>,
        devices: Vec<Device>,
        subscriptions: Vec<Subscription>,
    ) -> LicenseService {
        LicenseService::new(
            Arc::new(MockLicenseRepository::with(licenses)),
            Arc::new(MockDeviceRepository::with(devices)),
            Arc::new(MockSubscriptionRepository::with(subscriptions)),
        )
    }

    #[tokio::test]
    async fn find_by_key_returns_what_the_repository_returns() {
        let service = service_with(
            vec![sample_license(LicenseRecordStatus::Active, 1)],
            vec![],
            vec![],
        );

        let found = service.find_by_key("TEST-KEY").await.unwrap();
        assert_eq!(found.unwrap().license_key, "TEST-KEY");
    }

    #[tokio::test]
    async fn find_by_key_returns_none_when_the_repository_has_nothing() {
        let service = service_with(vec![], vec![], vec![]);

        let found = service.find_by_key("NOPE").await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn activate_with_unknown_key_returns_license_not_found() {
        let service = service_with(vec![], vec![], vec![]);

        let err = service
            .activate("NOPE", Uuid::new_v4(), "fp", "label")
            .await
            .unwrap_err();
        assert!(matches!(err, LicenseOperationError::LicenseNotFound));
    }

    #[tokio::test]
    async fn activate_a_revoked_license_is_rejected() {
        let service = service_with(
            vec![sample_license(LicenseRecordStatus::Revoked, 1)],
            vec![],
            vec![],
        );

        let err = service
            .activate("TEST-KEY", Uuid::new_v4(), "fp", "label")
            .await
            .unwrap_err();
        assert!(matches!(err, LicenseOperationError::LicenseRevoked));
    }

    #[tokio::test]
    async fn activate_an_expired_license_is_rejected() {
        let service = service_with(
            vec![sample_license(LicenseRecordStatus::Expired, 1)],
            vec![],
            vec![],
        );

        let err = service
            .activate("TEST-KEY", Uuid::new_v4(), "fp", "label")
            .await
            .unwrap_err();
        assert!(matches!(err, LicenseOperationError::LicenseExpired));
    }

    #[tokio::test]
    async fn activate_a_fresh_device_succeeds_and_returns_subscription_terms() {
        let service = service_with(
            vec![sample_license(LicenseRecordStatus::Active, 1)],
            vec![],
            vec![sample_subscription()],
        );

        let outcome = service
            .activate("TEST-KEY", Uuid::new_v4(), "fp", "label")
            .await
            .unwrap();
        assert_eq!(outcome.customer_id, 100);
        assert_eq!(outcome.plan_type, PlanType::Yearly);
        assert_eq!(outcome.license.license_key, "TEST-KEY");
    }

    #[tokio::test]
    async fn activate_is_idempotent_for_an_already_active_device() {
        let device_id = Uuid::new_v4();
        let license = sample_license(LicenseRecordStatus::Active, 1);
        let existing_device = Device {
            id: 1,
            license_id: license.id,
            device_id,
            machine_fingerprint: "fp".to_string(),
            device_label: None,
            first_seen_at: Utc::now(),
            last_seen_at: Utc::now(),
            deactivated_at: None,
        };
        let service = service_with(
            vec![license],
            vec![existing_device],
            vec![sample_subscription()],
        );

        // Would fail with DeviceLimitReached (max_devices = 1) if this
        // incorrectly tried to insert a second device row instead of
        // recognizing the already-active one.
        let outcome = service
            .activate("TEST-KEY", device_id, "fp", "label")
            .await
            .unwrap();
        assert_eq!(outcome.license.id, 1);
    }

    #[tokio::test]
    async fn activate_beyond_max_devices_returns_device_limit_reached_with_the_device_list() {
        let license = sample_license(LicenseRecordStatus::Active, 1);
        let existing_device = Device {
            id: 1,
            license_id: license.id,
            device_id: Uuid::new_v4(),
            machine_fingerprint: "fp-1".to_string(),
            device_label: Some("existing".to_string()),
            first_seen_at: Utc::now(),
            last_seen_at: Utc::now(),
            deactivated_at: None,
        };
        let service = service_with(
            vec![license],
            vec![existing_device],
            vec![sample_subscription()],
        );

        let err = service
            .activate("TEST-KEY", Uuid::new_v4(), "fp-2", "new-device")
            .await
            .unwrap_err();
        match err {
            LicenseOperationError::DeviceLimitReached(devices) => {
                assert_eq!(devices.len(), 1);
                assert_eq!(devices[0].device_label.as_deref(), Some("existing"));
            }
            other => panic!("expected DeviceLimitReached, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn activate_reuses_a_previously_deactivated_devices_row() {
        let device_id = Uuid::new_v4();
        let license = sample_license(LicenseRecordStatus::Active, 1);
        let deactivated_device = Device {
            id: 1,
            license_id: license.id,
            device_id,
            machine_fingerprint: "old-fp".to_string(),
            device_label: None,
            first_seen_at: Utc::now(),
            last_seen_at: Utc::now(),
            deactivated_at: Some(Utc::now()),
        };
        let service = service_with(
            vec![license],
            vec![deactivated_device],
            vec![sample_subscription()],
        );

        let outcome = service
            .activate("TEST-KEY", device_id, "new-fp", "label")
            .await
            .unwrap();
        assert_eq!(outcome.license.id, 1);
    }

    #[tokio::test]
    async fn validate_with_unknown_license_id_returns_device_not_activated() {
        let service = service_with(vec![], vec![], vec![]);

        let err = service
            .validate(999, Uuid::new_v4(), "fp")
            .await
            .unwrap_err();
        assert!(matches!(err, LicenseOperationError::DeviceNotActivated));
    }

    #[tokio::test]
    async fn validate_with_a_device_never_activated_on_this_license_returns_device_not_activated() {
        let service = service_with(
            vec![sample_license(LicenseRecordStatus::Active, 1)],
            vec![],
            vec![],
        );

        let err = service.validate(1, Uuid::new_v4(), "fp").await.unwrap_err();
        assert!(matches!(err, LicenseOperationError::DeviceNotActivated));
    }

    #[tokio::test]
    async fn validate_reports_a_fingerprint_mismatch_without_rejecting() {
        let device_id = Uuid::new_v4();
        let license = sample_license(LicenseRecordStatus::Active, 1);
        let device = Device {
            id: 1,
            license_id: license.id,
            device_id,
            machine_fingerprint: "original-fp".to_string(),
            device_label: None,
            first_seen_at: Utc::now(),
            last_seen_at: Utc::now(),
            deactivated_at: None,
        };
        let service = service_with(vec![license], vec![device], vec![]);

        let outcome = service
            .validate(1, device_id, "different-fp")
            .await
            .unwrap();
        assert!(!outcome.fingerprint_matched);
        assert_eq!(outcome.status, LicenseRecordStatus::Active);
    }

    #[tokio::test]
    async fn deactivate_frees_a_device_slot() {
        let device_id = Uuid::new_v4();
        let license = sample_license(LicenseRecordStatus::Active, 1);
        let device = Device {
            id: 1,
            license_id: license.id,
            device_id,
            machine_fingerprint: "fp".to_string(),
            device_label: None,
            first_seen_at: Utc::now(),
            last_seen_at: Utc::now(),
            deactivated_at: None,
        };
        let service = service_with(vec![license], vec![device], vec![]);

        let outcome = service.deactivate(1, device_id).await.unwrap();
        assert_eq!(outcome.devices_active, 0);
    }

    #[tokio::test]
    async fn deactivate_an_already_inactive_device_returns_device_not_activated() {
        let device_id = Uuid::new_v4();
        let license = sample_license(LicenseRecordStatus::Active, 1);
        let device = Device {
            id: 1,
            license_id: license.id,
            device_id,
            machine_fingerprint: "fp".to_string(),
            device_label: None,
            first_seen_at: Utc::now(),
            last_seen_at: Utc::now(),
            deactivated_at: Some(Utc::now()),
        };
        let service = service_with(vec![license], vec![device], vec![]);

        let err = service.deactivate(1, device_id).await.unwrap_err();
        assert!(matches!(err, LicenseOperationError::DeviceNotActivated));
    }
}
