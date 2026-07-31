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

use crate::domain::{
    Device, License, LicenseRecordStatus, PlanType, Subscription, ValidationLogResult,
};
use crate::repository::device::{DeviceActivationOutcome, DeviceRepository};
use crate::repository::error::RepositoryError;
use crate::repository::license::LicenseRepository;
use crate::repository::subscription::SubscriptionRepository;
use crate::service::AuditService;
use chrono::{DateTime, Utc};
use std::fmt;
use std::sync::Arc;
use uuid::Uuid;

pub struct LicenseService {
    license_repository: Arc<dyn LicenseRepository>,
    device_repository: Arc<dyn DeviceRepository>,
    subscription_repository: Arc<dyn SubscriptionRepository>,
    /// Audit-log writes (`license_validation_logs`, migration `0006`) — see
    /// `activate`/`validate`/`heartbeat`'s own doc comments for exactly
    /// which outcomes call this. Fire-and-forget
    /// (`AuditService::record_validation` only hands a future to
    /// `tokio::spawn`), so holding this never changes any of these
    /// methods' own async/error-propagation shape.
    audit_service: Arc<AuditService>,
}

/// Maps a resolved [`LicenseRecordStatus`] onto the narrower
/// [`ValidationLogResult`] `license_validation_logs.result` accepts — the
/// two enums agree on every case (`Active`→`Valid`, `Expired`→`Expired`,
/// `Suspended`→`Suspended`, `Revoked`→`Revoked`), so this is a pure
/// relabeling, not a decision.
fn as_validation_log_result(status: LicenseRecordStatus) -> ValidationLogResult {
    match status {
        LicenseRecordStatus::Active => ValidationLogResult::Valid,
        LicenseRecordStatus::Expired => ValidationLogResult::Expired,
        LicenseRecordStatus::Suspended => ValidationLogResult::Suspended,
        LicenseRecordStatus::Revoked => ValidationLogResult::Revoked,
    }
}

/// The license's status as it should be *reported*, correcting for a
/// natural time-based expiry the stored `licenses.status` column was
/// never updated to reflect (Phase 4L.3, production validation, HIGH).
///
/// Nothing in this codebase proactively flips `status` to `Expired` when
/// `expires_at` passes — only an explicit webhook-driven transition
/// (revoke/refund/dispute) or a fresh `Insert`/`Extend` at activation-time
/// ever writes `licenses.status`. If a subscription's renewal webhooks
/// simply stop arriving (Razorpay outage, a cancelled auto-renew with no
/// further billing, ...), a license stored as `Active` would otherwise be
/// reported `Active` by `/validate-license`/`/heartbeat` forever, even
/// though `expires_at` — already computed, already stored, already
/// returned in the response — says otherwise. Only overrides the `Active`
/// case: `Suspended`/`Revoked`/an already-stored `Expired` are already
/// correctly non-active for their own reasons and must not be
/// second-guessed here. Read-only — never writes `licenses.status` itself,
/// so this can't race with or duplicate a webhook's own transition.
fn effective_status(license: &License) -> LicenseRecordStatus {
    if license.status == LicenseRecordStatus::Active {
        if let Some(expires_at) = license.expires_at {
            if expires_at <= Utc::now() {
                return LicenseRecordStatus::Expired;
            }
        }
    }
    license.status
}

impl LicenseService {
    pub fn new(
        license_repository: Arc<dyn LicenseRepository>,
        device_repository: Arc<dyn DeviceRepository>,
        subscription_repository: Arc<dyn SubscriptionRepository>,
        audit_service: Arc<AuditService>,
    ) -> Self {
        LicenseService {
            license_repository,
            device_repository,
            subscription_repository,
            audit_service,
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
    ///
    /// **Phase 4J.3 fix (production readiness audit, HIGH finding #3):**
    /// the device-count check and the insert/reactivate used to be two
    /// separate round trips — two concurrent activations for *different*
    /// `device_id`s on the same license could both see a free slot before
    /// either had written anything, together exceeding `max_devices`.
    /// `DeviceRepository::activate_device` now performs that whole
    /// decide-then-mutate step atomically in one transaction; see its own
    /// doc comment for how.
    ///
    /// **Audit logging (Module 1):** appends one fire-and-forget
    /// `license_validation_logs` write (`AuditService::record_validation`,
    /// migration `0006`) for the outcomes that fit that table's `result`
    /// taxonomy — a revoked/expired rejection, or a full success. Rejected
    /// before a license was even found (`LicenseNotFound`) and
    /// `DeviceLimitReached` (a capacity error, not a license-state result)
    /// are intentionally not logged here — see migration `0006`'s doc
    /// comment.
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

        match effective_status(&license) {
            LicenseRecordStatus::Revoked => {
                self.audit_service.record_validation(
                    license.id,
                    device_id,
                    ValidationLogResult::Revoked,
                );
                return Err(LicenseOperationError::LicenseRevoked);
            }
            LicenseRecordStatus::Expired => {
                self.audit_service.record_validation(
                    license.id,
                    device_id,
                    ValidationLogResult::Expired,
                );
                return Err(LicenseOperationError::LicenseExpired);
            }
            // A suspended license may still be activated — validate-license
            // is what surfaces "suspended" to the caller as a non-error
            // status, per API_SPECIFICATION.md's error table only listing
            // NOT_FOUND/DEVICE_LIMIT/REVOKED/EXPIRED for this endpoint.
            LicenseRecordStatus::Active | LicenseRecordStatus::Suspended => {}
        }

        match self
            .device_repository
            .activate_device(
                license.id,
                license.max_devices,
                device_id,
                machine_fingerprint,
                device_label,
            )
            .await?
        {
            DeviceActivationOutcome::Activated => {}
            DeviceActivationOutcome::LimitReached(existing) => {
                return Err(LicenseOperationError::DeviceLimitReached(existing));
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

        self.audit_service
            .record_validation(license.id, device_id, ValidationLogResult::Valid);

        Ok(ActivationOutcome {
            customer_id: subscription.user_id,
            plan_type: subscription.plan_type,
            license,
        })
    }

    /// `POST /validate-license`. Called on every online app launch
    /// (`LICENSE_SYSTEM_DESIGN.md` §4).
    ///
    /// **Production Hardening, Finding C1(backward-compatible):**
    /// `requesting_user_id` is `Some` only when the caller presented a
    /// *valid* session (`routes::license`'s handlers resolve this from an
    /// optional `Authorization: Bearer` header before calling in) — the
    /// desktop's `HttpLicenseClient` has no login flow and sends none
    /// today, so this is `None` for every real call in production right
    /// now, and this method's behavior for that caller is unchanged except
    /// for the fingerprint check below. When a session *is* presented,
    /// [`find_active_device`] additionally verifies it actually owns the
    /// subscription behind `license_id`, rejecting otherwise.
    ///
    /// A fingerprint mismatch used to be reported back
    /// (`fingerprint_matched: false`) without rejecting the call
    /// (`LICENSE_SECURITY_REVIEW.md` §5's original "logged signal, not an
    /// automatic block" framing). It now rejects outright
    /// (`DeviceMismatch`) — the strictest anonymous-path enforcement
    /// available without a wire-contract change, and consistent with how
    /// every *other* invalid-device/license condition here already
    /// behaves. `fingerprint_matched` stays on [`ValidationOutcome`]/the
    /// wire response for shape stability; it is simply always `true` now
    /// that a mismatch is a distinct rejected outcome instead.
    ///
    /// **Audit logging (Module 1):** appends one fire-and-forget
    /// `license_validation_logs` write — `device_mismatch` on a rejected
    /// fingerprint mismatch, otherwise the license's own status.
    /// `DeviceNotActivated` (the error path, via `find_active_device`,
    /// which also now covers an ownership mismatch) is not logged — it has
    /// no case in `license_validation_logs.result`'s taxonomy — see
    /// migration `0006`'s doc comment.
    ///
    /// [`find_active_device`]: Self::find_active_device
    pub async fn validate(
        &self,
        license_id: i64,
        device_id: Uuid,
        machine_fingerprint: &str,
        requesting_user_id: Option<i64>,
    ) -> Result<ValidationOutcome, LicenseOperationError> {
        let (license, device) = self
            .find_active_device(license_id, device_id, requesting_user_id)
            .await?;
        self.device_repository.touch_last_seen(device.id).await?;

        let status = effective_status(&license);

        if device.machine_fingerprint != machine_fingerprint {
            self.audit_service.record_validation(
                license.id,
                device_id,
                ValidationLogResult::DeviceMismatch,
            );
            tracing::warn!(
                license_id = license.id,
                device_id = %device_id,
                "machine fingerprint mismatch on validate-license; rejecting"
            );
            return Err(LicenseOperationError::DeviceMismatch);
        }

        self.audit_service.record_validation(
            license.id,
            device_id,
            as_validation_log_result(status),
        );

        Ok(ValidationOutcome {
            status,
            expires_at: license.expires_at,
            grace_period_days: license.grace_period_days,
            fingerprint_matched: true,
        })
    }

    /// `POST /heartbeat`. A lightweight liveness ping meant to be called
    /// periodically *while the app is running*, not just at startup
    /// (`API_SPECIFICATION.md`) — reuses [`find_active_device`], the exact
    /// same "does this device_id have a live activation on this license_id"
    /// lookup `validate` depends on, since a heartbeat's only job is to
    /// surface a license that became non-active mid-session sooner than
    /// the next full validation; it has no fingerprint/expiry payload of
    /// its own to compute, hence the narrower [`HeartbeatOutcome`].
    ///
    /// **Production Hardening, Finding C1 (backward-compatible):** same
    /// optional-session ownership check as `validate` — see its doc
    /// comment. `HeartbeatRequest` carries no `machine_fingerprint` on the
    /// wire, so unlike `validate` there is nothing further to check when
    /// no session is presented; the existing device-activation check
    /// (via `find_active_device`) is already this method's strictest
    /// available anonymous-path enforcement.
    ///
    /// [`find_active_device`]: Self::find_active_device
    ///
    /// **Audit logging (Module 1):** same `license_validation_logs` write
    /// as `validate`, minus the `device_mismatch` case — heartbeat has no
    /// fingerprint of its own to check.
    pub async fn heartbeat(
        &self,
        license_id: i64,
        device_id: Uuid,
        requesting_user_id: Option<i64>,
    ) -> Result<HeartbeatOutcome, LicenseOperationError> {
        let (license, device) = self
            .find_active_device(license_id, device_id, requesting_user_id)
            .await?;
        self.device_repository.touch_last_seen(device.id).await?;

        let status = effective_status(&license);
        self.audit_service.record_validation(
            license.id,
            device_id,
            as_validation_log_result(status),
        );

        Ok(HeartbeatOutcome { status })
    }

    /// Shared lookup behind `validate` and `heartbeat`: resolves
    /// `license_id` to a `License` and confirms `device_id` has a
    /// still-active (`deactivated_at IS NULL`) row on it, both rejecting
    /// with the same `DeviceNotActivated` `API_SPECIFICATION.md` documents
    /// for `/validate-license` ("this device_id was never activated
    /// against this license") — `/heartbeat`'s own spec says to treat a
    /// failure the same way, so there's no separate error case to
    /// distinguish here either.
    ///
    /// **Production Hardening, Finding C1:** when `requesting_user_id` is
    /// `Some` (a valid session was presented — see `validate`'s doc
    /// comment), also rejects if that session doesn't own the subscription
    /// behind this license, mapped onto the *same* `DeviceNotActivated`
    /// rather than a distinguishable error — an authenticated-but-wrong-
    /// owner caller must not be able to tell "not yours" apart from
    /// "this device_id was never activated here" any more than an
    /// anonymous caller guessing wrong already can.
    async fn find_active_device(
        &self,
        license_id: i64,
        device_id: Uuid,
        requesting_user_id: Option<i64>,
    ) -> Result<(License, Device), LicenseOperationError> {
        let license = self
            .license_repository
            .find_by_id(license_id)
            .await?
            .ok_or(LicenseOperationError::DeviceNotActivated)?;

        if !self
            .owns_subscription(license.subscription_id, requesting_user_id)
            .await?
        {
            tracing::warn!(
                license_id = license.id,
                "session presented does not own this license; rejecting as device not activated"
            );
            return Err(LicenseOperationError::DeviceNotActivated);
        }

        let device = self
            .device_repository
            .find_by_license_and_device_id(license.id, device_id)
            .await?
            .filter(|d| d.deactivated_at.is_none())
            .ok_or(LicenseOperationError::DeviceNotActivated)?;

        Ok((license, device))
    }

    /// Whether `requesting_user_id` (if any) owns the subscription behind
    /// `subscription_id` — `true` when no session was presented at all
    /// (today's only real caller, per `validate`'s doc comment) or when it
    /// matches; `false` on a genuine mismatch. Shared by `find_active_device`
    /// and `deactivate`, each of which maps a `false` onto whichever
    /// "not found"-shaped error they already return for a nonexistent
    /// license/device, rather than a new, distinguishable error — Finding
    /// C1's masking requirement applies identically to both.
    async fn owns_subscription(
        &self,
        subscription_id: i64,
        requesting_user_id: Option<i64>,
    ) -> Result<bool, LicenseOperationError> {
        let Some(user_id) = requesting_user_id else {
            return Ok(true);
        };

        let owner = self
            .subscription_repository
            .find_by_id(subscription_id)
            .await?
            .map(|s| s.user_id);

        Ok(owner == Some(user_id))
    }

    /// `POST /deactivate-license`. Frees a device slot — the customer-
    /// facing counterpart to the admin-surface `POST /devices/{id}/deactivate`
    /// `API_SPECIFICATION.md` mentions but doesn't specify (out of scope
    /// for that document's list of 7). Soft-delete only, never a row
    /// removal, same as every other status transition in this schema.
    ///
    /// **Production Hardening, Finding C1 (backward-compatible):** same
    /// optional-session ownership check as `validate`/`heartbeat` (see
    /// `find_active_device`'s doc comment; a mismatch here maps to
    /// `LicenseNotFound`, matching this method's own existing "missing
    /// license" case rather than `find_active_device`'s `DeviceNotActivated`
    /// masking, since that's the error this method already uses for that
    /// case). `machine_fingerprint` is now a required parameter and is
    /// checked strictly, rejecting on any mismatch — safe to make
    /// mandatory precisely because nothing calls this endpoint from the
    /// desktop client today (no `deactivate_license` method exists on
    /// `LicenseApiClient`), unlike `validate`/`heartbeat`, where a wire
    /// change would break the live client.
    pub async fn deactivate(
        &self,
        license_id: i64,
        device_id: Uuid,
        machine_fingerprint: &str,
        requesting_user_id: Option<i64>,
    ) -> Result<DeactivationOutcome, LicenseOperationError> {
        let license = self
            .license_repository
            .find_by_id(license_id)
            .await?
            .ok_or(LicenseOperationError::LicenseNotFound)?;

        if !self
            .owns_subscription(license.subscription_id, requesting_user_id)
            .await?
        {
            tracing::warn!(
                license_id = license.id,
                "session presented does not own this license; rejecting as not found"
            );
            return Err(LicenseOperationError::LicenseNotFound);
        }

        let device = self
            .device_repository
            .find_by_license_and_device_id(license.id, device_id)
            .await?
            .filter(|d| d.deactivated_at.is_none())
            .ok_or(LicenseOperationError::DeviceNotActivated)?;

        if device.machine_fingerprint != machine_fingerprint {
            tracing::warn!(
                license_id = license.id,
                device_id = %device_id,
                "machine fingerprint mismatch on deactivate-license; rejecting"
            );
            return Err(LicenseOperationError::DeviceMismatch);
        }

        self.device_repository.deactivate(device.id).await?;
        let devices_active = self
            .device_repository
            .count_active_by_license(license.id)
            .await?;

        Ok(DeactivationOutcome { devices_active })
    }

    /// `GET /subscription`. Fetches the logged-in account's current
    /// subscription/billing summary (`API_SPECIFICATION.md`) — reuses the
    /// same three repositories `activate`/`validate` already depend on
    /// rather than introducing a separate service for this one read-only
    /// aggregation. `licenses` reuses
    /// `LicenseRepository::find_latest_by_subscription` — the same "current
    /// license for this subscription" query `service::payment_service`
    /// already relies on to decide "extend vs. issue fresh" on a renewal —
    /// so this needs no new repository query; it's always 0 or 1 entries,
    /// matching that method's own "most recent non-revoked license" scope,
    /// never more (avoiding any N+1 device-count query beyond the single
    /// license this returns).
    ///
    /// A user with no currently-`active` subscription row is a state
    /// `API_SPECIFICATION.md` doesn't document a specific error code for —
    /// treated the same way `activate`'s "license references a missing
    /// subscription" already is (an unexpected referential state, not a
    /// documented client error), rather than inventing a new wire-level
    /// error code out of scope for this phase.
    pub async fn subscription_summary(
        &self,
        user_id: i64,
    ) -> Result<SubscriptionSummaryOutcome, LicenseOperationError> {
        let subscription = self
            .subscription_repository
            .find_latest_by_user(user_id)
            .await?
            .ok_or_else(|| {
                LicenseOperationError::Repository(RepositoryError::InvalidData(format!(
                    "user {user_id} has no subscription"
                )))
            })?;

        let license = self
            .license_repository
            .find_latest_by_subscription(subscription.id)
            .await?;

        let mut licenses = Vec::new();
        if let Some(license) = license {
            let devices_active = self
                .device_repository
                .count_active_by_license(license.id)
                .await?;
            licenses.push(LicenseSummaryOutcome {
                license_id: license.id,
                status: license.status,
                devices_active,
                max_devices: license.max_devices,
                license_key: license.license_key,
            });
        }

        Ok(SubscriptionSummaryOutcome {
            subscription,
            licenses,
        })
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
pub struct HeartbeatOutcome {
    pub status: LicenseRecordStatus,
}

#[derive(Debug)]
pub struct SubscriptionSummaryOutcome {
    pub subscription: Subscription,
    pub licenses: Vec<LicenseSummaryOutcome>,
}

#[derive(Debug)]
pub struct LicenseSummaryOutcome {
    pub license_id: i64,
    pub status: LicenseRecordStatus,
    pub devices_active: i64,
    pub max_devices: i32,
    pub license_key: String,
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
    /// Production Hardening, Finding C1: the caller-supplied
    /// `machine_fingerprint` doesn't match the device's stored one —
    /// `validate`/`refresh-license` (already carried `machine_fingerprint`
    /// on the wire) and `deactivate` (which now requires it too, see that
    /// method's own doc comment) both reject outright on a mismatch rather
    /// than the old "report `fingerprint_matched: false` but still succeed"
    /// behavior — bringing this in line with how every *other* invalid-
    /// device/license condition here is already handled (as a rejection,
    /// not a soft 200).
    DeviceMismatch,
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
            LicenseOperationError::DeviceMismatch => {
                write!(f, "machine fingerprint does not match the activated device")
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

        async fn find_latest_by_subscription(
            &self,
            subscription_id: i64,
        ) -> Result<Option<License>, RepositoryError> {
            Ok(self
                .licenses
                .lock()
                .unwrap()
                .iter()
                .filter(|l| {
                    l.subscription_id == subscription_id && l.status != LicenseRecordStatus::Revoked
                })
                .max_by_key(|l| l.issued_at)
                .cloned())
        }

        async fn insert(
            &self,
            _new_license: crate::domain::NewLicense,
        ) -> Result<License, RepositoryError> {
            unimplemented!(
                "not exercised by these tests — see service::payment_service for coverage"
            )
        }

        async fn extend(
            &self,
            _id: i64,
            _status: LicenseRecordStatus,
            _expires_at: Option<chrono::DateTime<Utc>>,
        ) -> Result<(), RepositoryError> {
            unimplemented!(
                "not exercised by these tests — see service::payment_service for coverage"
            )
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

        async fn touch_last_seen(&self, id: i64) -> Result<(), RepositoryError> {
            if let Some(d) = self.devices.lock().unwrap().iter_mut().find(|d| d.id == id) {
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

        /// Simulates `activate_device`'s atomic decide-then-mutate
        /// contract entirely in memory: the whole method body runs under
        /// one `Mutex` acquisition (no `.await` point in between any of
        /// the reads and the eventual write), so two calls racing on the
        /// same `license_id` with different `device_id`s can't both
        /// observe a free slot before either writes — exactly the
        /// guarantee the real `SELECT ... FOR UPDATE` transaction gives.
        async fn activate_device(
            &self,
            license_id: i64,
            max_devices: i32,
            device_id: Uuid,
            machine_fingerprint: &str,
            device_label: &str,
        ) -> Result<DeviceActivationOutcome, RepositoryError> {
            let mut devices = self.devices.lock().unwrap();

            let existing_match = devices
                .iter()
                .find(|d| d.license_id == license_id && d.device_id == device_id)
                .map(|d| (d.id, d.deactivated_at.is_some()));

            if let Some((id, was_deactivated)) = existing_match {
                if !was_deactivated {
                    if let Some(d) = devices.iter_mut().find(|d| d.id == id) {
                        d.last_seen_at = Utc::now();
                    }
                    return Ok(DeviceActivationOutcome::Activated);
                }

                let active_count = devices
                    .iter()
                    .filter(|d| d.license_id == license_id && d.deactivated_at.is_none())
                    .count() as i32;
                if active_count >= max_devices {
                    let active = devices
                        .iter()
                        .filter(|d| d.license_id == license_id && d.deactivated_at.is_none())
                        .cloned()
                        .collect();
                    return Ok(DeviceActivationOutcome::LimitReached(active));
                }

                if let Some(d) = devices.iter_mut().find(|d| d.id == id) {
                    d.deactivated_at = None;
                    d.last_seen_at = Utc::now();
                }
                return Ok(DeviceActivationOutcome::Activated);
            }

            let active_count = devices
                .iter()
                .filter(|d| d.license_id == license_id && d.deactivated_at.is_none())
                .count() as i32;
            if active_count >= max_devices {
                let active = devices
                    .iter()
                    .filter(|d| d.license_id == license_id && d.deactivated_at.is_none())
                    .cloned()
                    .collect();
                return Ok(DeviceActivationOutcome::LimitReached(active));
            }

            let mut next_id = self.next_id.lock().unwrap();
            let id = *next_id;
            *next_id += 1;
            let now = Utc::now();
            devices.push(Device {
                id,
                license_id,
                device_id,
                machine_fingerprint: machine_fingerprint.to_string(),
                device_label: Some(device_label.to_string()),
                first_seen_at: now,
                last_seen_at: now,
                deactivated_at: None,
            });
            Ok(DeviceActivationOutcome::Activated)
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

        async fn find_latest_by_user(
            &self,
            user_id: i64,
        ) -> Result<Option<Subscription>, RepositoryError> {
            Ok(self
                .subscriptions
                .lock()
                .unwrap()
                .iter()
                .find(|s| s.user_id == user_id)
                .cloned())
        }

        async fn insert(
            &self,
            _new_subscription: crate::domain::NewSubscription,
        ) -> Result<Subscription, RepositoryError> {
            unimplemented!(
                "not exercised by these tests — see service::payment_service for coverage"
            )
        }

        async fn update_status(
            &self,
            _id: i64,
            _status: SubscriptionStatus,
            _current_period_end: Option<chrono::DateTime<Utc>>,
        ) -> Result<(), RepositoryError> {
            unimplemented!(
                "not exercised by these tests — see service::payment_service for coverage"
            )
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
            Arc::new(AuditService::new(Arc::new(
                crate::repository::audit::NoopAuditRepository,
            ))),
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

    /// Phase 4L.3 (production validation, HIGH): same `effective_status`
    /// correction as `/validate-license`/`/heartbeat` — a *new* device
    /// activation must also be rejected once `expires_at` has passed, not
    /// only once some other process has gotten around to writing
    /// `status = Expired`.
    #[tokio::test]
    async fn activate_a_license_whose_expires_at_has_passed_is_rejected_even_though_stored_status_is_still_active(
    ) {
        let license = License {
            expires_at: Some(Utc::now() - chrono::Duration::days(1)),
            ..sample_license(LicenseRecordStatus::Active, 1)
        };
        let service = service_with(vec![license], vec![], vec![]);

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
            .validate(999, Uuid::new_v4(), "fp", None)
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

        let err = service
            .validate(1, Uuid::new_v4(), "fp", None)
            .await
            .unwrap_err();
        assert!(matches!(err, LicenseOperationError::DeviceNotActivated));
    }

    /// Production Hardening, Finding C1: was
    /// `validate_reports_a_fingerprint_mismatch_without_rejecting` — the
    /// exact "soft" behavior this finding fixes. A mismatch now rejects
    /// outright instead of succeeding with `fingerprint_matched: false`.
    #[tokio::test]
    async fn validate_rejects_a_fingerprint_mismatch_with_device_mismatch() {
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

        let err = service
            .validate(1, device_id, "different-fp", None)
            .await
            .unwrap_err();
        assert!(matches!(err, LicenseOperationError::DeviceMismatch));
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

        let outcome = service.deactivate(1, device_id, "fp", None).await.unwrap();
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

        let err = service
            .deactivate(1, device_id, "fp", None)
            .await
            .unwrap_err();
        assert!(matches!(err, LicenseOperationError::DeviceNotActivated));
    }

    /// Production Hardening, Finding C1: `/deactivate-license` now requires
    /// `machine_fingerprint` and rejects a mismatch — safe to make
    /// mandatory since no live client calls this endpoint (see
    /// `LicenseService::deactivate`'s doc comment).
    #[tokio::test]
    async fn deactivate_rejects_a_fingerprint_mismatch_with_device_mismatch() {
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

        let err = service
            .deactivate(1, device_id, "different-fp", None)
            .await
            .unwrap_err();
        assert!(matches!(err, LicenseOperationError::DeviceMismatch));
    }

    // ── Production Hardening, Finding C1: optional-session ownership ────
    //
    // `sample_subscription()` is `id: 10, user_id: 100`; `sample_license()`
    // is `subscription_id: 10` — so `Some(100)` owns it and any other id
    // does not.

    #[tokio::test]
    async fn validate_succeeds_when_the_presented_session_owns_the_license() {
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
        let service = service_with(vec![license], vec![device], vec![sample_subscription()]);

        let outcome = service
            .validate(1, device_id, "fp", Some(100))
            .await
            .unwrap();
        assert_eq!(outcome.status, LicenseRecordStatus::Active);
    }

    #[tokio::test]
    async fn validate_rejects_when_the_presented_session_does_not_own_the_license() {
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
        let service = service_with(vec![license], vec![device], vec![sample_subscription()]);

        let err = service
            .validate(1, device_id, "fp", Some(999))
            .await
            .unwrap_err();
        assert!(matches!(err, LicenseOperationError::DeviceNotActivated));
    }

    #[tokio::test]
    async fn heartbeat_succeeds_when_the_presented_session_owns_the_license() {
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
        let service = service_with(vec![license], vec![device], vec![sample_subscription()]);

        let outcome = service.heartbeat(1, device_id, Some(100)).await.unwrap();
        assert_eq!(outcome.status, LicenseRecordStatus::Active);
    }

    #[tokio::test]
    async fn heartbeat_rejects_when_the_presented_session_does_not_own_the_license() {
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
        let service = service_with(vec![license], vec![device], vec![sample_subscription()]);

        let err = service
            .heartbeat(1, device_id, Some(999))
            .await
            .unwrap_err();
        assert!(matches!(err, LicenseOperationError::DeviceNotActivated));
    }

    #[tokio::test]
    async fn deactivate_succeeds_when_the_presented_session_owns_the_license() {
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
        let service = service_with(vec![license], vec![device], vec![sample_subscription()]);

        let outcome = service
            .deactivate(1, device_id, "fp", Some(100))
            .await
            .unwrap();
        assert_eq!(outcome.devices_active, 0);
    }

    #[tokio::test]
    async fn deactivate_rejects_when_the_presented_session_does_not_own_the_license() {
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
        let service = service_with(vec![license], vec![device], vec![sample_subscription()]);

        let err = service
            .deactivate(1, device_id, "fp", Some(999))
            .await
            .unwrap_err();
        assert!(matches!(err, LicenseOperationError::LicenseNotFound));
    }

    // ── Phase 4J.7: /heartbeat, /refresh-license, /subscription ─────────

    #[tokio::test]
    async fn heartbeat_reports_the_current_license_status_for_an_active_device() {
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

        let outcome = service.heartbeat(1, device_id, None).await.unwrap();
        assert_eq!(outcome.status, LicenseRecordStatus::Active);
    }

    /// Phase 4L.3 (production validation, HIGH): nothing else in this
    /// codebase ever flips a license's stored `status` from `Active` to
    /// `Expired` when `expires_at` passes on its own — only an explicit
    /// webhook-driven transition or a fresh activation ever writes
    /// `licenses.status`. If a subscription's renewal webhooks simply stop
    /// arriving, this proves `/heartbeat` (and `/validate-license`, same
    /// `effective_status` helper) still correctly reports `Expired` by
    /// comparing `expires_at` against now, rather than trusting the stale
    /// stored `Active` status forever.
    #[tokio::test]
    async fn heartbeat_reports_expired_when_expires_at_has_passed_even_though_stored_status_is_still_active(
    ) {
        let device_id = Uuid::new_v4();
        let license = License {
            expires_at: Some(Utc::now() - chrono::Duration::days(1)),
            ..sample_license(LicenseRecordStatus::Active, 1)
        };
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

        let outcome = service.heartbeat(1, device_id, None).await.unwrap();
        assert_eq!(outcome.status, LicenseRecordStatus::Expired);
    }

    #[tokio::test]
    async fn validate_reports_expired_when_expires_at_has_passed_even_though_stored_status_is_still_active(
    ) {
        let device_id = Uuid::new_v4();
        let license = License {
            expires_at: Some(Utc::now() - chrono::Duration::days(1)),
            ..sample_license(LicenseRecordStatus::Active, 1)
        };
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

        let outcome = service.validate(1, device_id, "fp", None).await.unwrap();
        assert_eq!(outcome.status, LicenseRecordStatus::Expired);
    }

    #[tokio::test]
    async fn heartbeat_still_reports_active_when_expires_at_is_in_the_future() {
        let device_id = Uuid::new_v4();
        let license = License {
            expires_at: Some(Utc::now() + chrono::Duration::days(30)),
            ..sample_license(LicenseRecordStatus::Active, 1)
        };
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

        let outcome = service.heartbeat(1, device_id, None).await.unwrap();
        assert_eq!(outcome.status, LicenseRecordStatus::Active);
    }

    #[tokio::test]
    async fn heartbeat_does_not_override_a_suspended_license_even_if_expires_at_has_passed() {
        // A dispute-suspended license shouldn't be reclassified as merely
        // "expired" — Suspended already correctly signals non-active for
        // its own (different, more urgent) reason.
        let device_id = Uuid::new_v4();
        let license = License {
            expires_at: Some(Utc::now() - chrono::Duration::days(1)),
            ..sample_license(LicenseRecordStatus::Suspended, 1)
        };
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

        let outcome = service.heartbeat(1, device_id, None).await.unwrap();
        assert_eq!(outcome.status, LicenseRecordStatus::Suspended);
    }

    #[tokio::test]
    async fn heartbeat_reports_an_expired_license_status_without_erroring() {
        // Same contract as `/validate-license`: a non-active status is
        // returned as data, never rejected as an error — only a device
        // that was never activated (or has since been deactivated) is a
        // `DeviceNotActivated` error.
        let device_id = Uuid::new_v4();
        let license = sample_license(LicenseRecordStatus::Expired, 1);
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

        let outcome = service.heartbeat(1, device_id, None).await.unwrap();
        assert_eq!(outcome.status, LicenseRecordStatus::Expired);
    }

    #[tokio::test]
    async fn heartbeat_with_a_device_never_activated_on_this_license_returns_device_not_activated()
    {
        let service = service_with(
            vec![sample_license(LicenseRecordStatus::Active, 1)],
            vec![],
            vec![],
        );

        let err = service
            .heartbeat(1, Uuid::new_v4(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, LicenseOperationError::DeviceNotActivated));
    }

    #[tokio::test]
    async fn heartbeat_with_an_unknown_license_id_returns_device_not_activated() {
        let service = service_with(vec![], vec![], vec![]);

        let err = service
            .heartbeat(999, Uuid::new_v4(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, LicenseOperationError::DeviceNotActivated));
    }

    #[tokio::test]
    async fn validate_and_heartbeat_agree_on_status_for_the_same_active_device() {
        // `/refresh-license` reuses `validate` outright (identical
        // request/response shape per `API_SPECIFICATION.md`), so this also
        // stands in as proof that `/refresh-license` and `/heartbeat` never
        // disagree about a device's current license status.
        let device_id = Uuid::new_v4();
        let license = sample_license(LicenseRecordStatus::Suspended, 1);
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

        let validate_outcome = service.validate(1, device_id, "fp", None).await.unwrap();
        let heartbeat_outcome = service.heartbeat(1, device_id, None).await.unwrap();
        assert_eq!(validate_outcome.status, heartbeat_outcome.status);
    }

    #[tokio::test]
    async fn subscription_summary_returns_the_active_subscription_and_its_current_license() {
        let device_id = Uuid::new_v4();
        let license = sample_license(LicenseRecordStatus::Active, 3);
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
        let service = service_with(vec![license], vec![device], vec![sample_subscription()]);

        let outcome = service.subscription_summary(100).await.unwrap();
        assert_eq!(outcome.subscription.user_id, 100);
        assert_eq!(outcome.licenses.len(), 1);
        assert_eq!(outcome.licenses[0].license_id, 1);
        assert_eq!(outcome.licenses[0].status, LicenseRecordStatus::Active);
        assert_eq!(outcome.licenses[0].devices_active, 1);
        assert_eq!(outcome.licenses[0].max_devices, 3);
        assert_eq!(outcome.licenses[0].license_key, "TEST-KEY");
    }

    /// Phase 4M (auto-checkout): a desktop client polling right after
    /// starting checkout must get a clean, current status back — not the
    /// error meant for "this user has never had a subscription at all."
    #[tokio::test]
    async fn subscription_summary_for_a_user_with_a_pending_payment_subscription_returns_it_without_erroring(
    ) {
        let mut pending = sample_subscription();
        pending.status = SubscriptionStatus::PendingPayment;
        let service = service_with(vec![], vec![], vec![pending]);

        let outcome = service.subscription_summary(100).await.unwrap();
        assert_eq!(outcome.subscription.status, SubscriptionStatus::PendingPayment);
        assert!(
            outcome.licenses.is_empty(),
            "no license exists yet before payment completes"
        );
    }

    #[tokio::test]
    async fn subscription_summary_for_a_user_with_no_active_subscription_is_a_repository_error() {
        let service = service_with(vec![], vec![], vec![]);

        let err = service.subscription_summary(100).await.unwrap_err();
        assert!(matches!(err, LicenseOperationError::Repository(_)));
    }
}
