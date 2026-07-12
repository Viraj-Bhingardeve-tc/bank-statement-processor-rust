//! `repository/` — the data-access layer (`PHASE4_DESIGN.md` §1.2's "Data
//! access" layer). Every module here exposes a trait first (what a service
//! depends on) and a Postgres implementation second (`Pg*`, what `main.rs`
//! constructs) — services never call `sqlx` directly.
//!
//! Phase 4F adds `payment` and `payment_webhook_event`, backed by
//! migration `0002_create_payment_schema.sql` — completing the full
//! non-payment-and-payment domain from `LICENSE_DATABASE_SCHEMA.md` §1 and
//! `PHASE4_DESIGN.md` §7. `login_history`/`license_validation_logs`
//! (audit-only tables) remain out of scope, not needed by any endpoint
//! implemented so far.

pub mod device;
pub mod error;
pub mod health;
pub mod license;
pub mod payment;
pub mod payment_webhook_event;
pub mod session;
pub mod subscription;
pub mod user;
