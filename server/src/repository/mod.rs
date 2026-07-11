//! `repository/` — the data-access layer (`PHASE4_DESIGN.md` §1.2's "Data
//! access" layer). Every module here exposes a trait first (what a service
//! depends on) and a Postgres implementation second (`Pg*`, what `main.rs`
//! will eventually construct) — services never call `sqlx` directly.
//!
//! Phase 4C.2 covers the non-payment domain: `user`, `subscription`,
//! `license`, `device`, `session`. `payment`/`payment_webhook_events`
//! (`LICENSE_DATABASE_SCHEMA.md` §1, `PHASE4_DESIGN.md` §7) are
//! intentionally absent — out of scope until the payment phase.
//!
//! None of these Postgres implementations have a matching migration yet:
//! `db.rs`'s `migrations/` is still empty (Phase 4C.1 deliberately deferred
//! the first real migration). They compile and are logically correct
//! against `LICENSE_DATABASE_SCHEMA.md` §1's documented schema, but cannot
//! succeed against a real database until a migration creates these tables —
//! that migration is intentionally not part of this phase either.

pub mod device;
pub mod error;
pub mod health;
pub mod license;
pub mod session;
pub mod subscription;
pub mod user;
