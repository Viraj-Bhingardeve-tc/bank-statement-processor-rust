//! Domain models — plain data types mirroring `LICENSE_DATABASE_SCHEMA.md`
//! §1's server-side schema (`payments`/`payment_webhook_events`
//! intentionally excluded — out of scope until the payment phase).
//!
//! Deliberately `sqlx`-agnostic: no type here derives `sqlx::FromRow` or
//! implements `sqlx::Type`. Each `repository::*` module owns the detail of
//! mapping a database row onto these types (including converting a stored
//! `TEXT` status column into the matching enum here), so a domain type
//! never needs to change just because a query's shape changes, and this
//! module has zero database dependency of its own.

pub mod device;
pub mod license;
pub mod session;
pub mod subscription;
pub mod user;

pub use device::{Device, NewDevice};
pub use license::{License, LicenseRecordStatus};
pub use session::{NewSession, Session};
pub use subscription::{PlanType, Subscription, SubscriptionStatus};
pub use user::{NewUser, User};
