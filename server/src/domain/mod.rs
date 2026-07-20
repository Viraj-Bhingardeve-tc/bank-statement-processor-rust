//! Domain models — plain data types mirroring `LICENSE_DATABASE_SCHEMA.md`
//! §1's server-side schema, plus `PHASE4_DESIGN.md` §7's
//! `payment_webhook_events` addition.
//!
//! Deliberately `sqlx`-agnostic: no type here derives `sqlx::FromRow` or
//! implements `sqlx::Type`. Each `repository::*` module owns the detail of
//! mapping a database row onto these types (including converting a stored
//! `TEXT` status column into the matching enum here), so a domain type
//! never needs to change just because a query's shape changes, and this
//! module has zero database dependency of its own.

pub mod audit;
pub mod device;
pub mod license;
pub mod payment;
pub mod payment_webhook_event;
pub mod session;
pub mod subscription;
pub mod user;

pub use audit::{NewLicenseValidationLogEntry, NewLoginHistoryEntry, ValidationLogResult};
pub use device::Device;
pub use license::{License, LicenseRecordStatus, NewLicense};
pub use payment::{NewPayment, Payment, PaymentStatus};
pub use payment_webhook_event::{NewPaymentWebhookEvent, PaymentWebhookEvent};
pub use session::{NewSession, Session};
pub use subscription::{NewSubscription, PlanType, Subscription, SubscriptionStatus};
pub use user::{NewUser, User};
