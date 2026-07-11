//! `repository/` — the data-access layer (`PHASE4_DESIGN.md` §1.2's "Data
//! access" layer). Every module here takes `&PgPool` directly rather than
//! the full `AppState`, so each stays testable against a pool alone rather
//! than needing the whole application wired up.
//!
//! Phase 4C.1 scaffolding only: `health` is the one real module (backs
//! `/readyz`). License/payment/session repositories land as new sibling
//! modules in later phases (e.g. `repository::license`,
//! `repository::payment`), following this same pattern — handlers never
//! call `sqlx` directly, only through a `repository::*` function.

pub mod health;
