//! `service/` — the business-logic layer (`PHASE4_DESIGN.md` §1.2). Each
//! service depends on repository *traits* (`Arc<dyn ...Repository>`), never
//! a concrete `Pg*` implementation directly, so it's testable against a
//! hand-written mock without a real database — see each module's own
//! tests.
//!
//! Phase 4C.2 scaffolding only: thin pass-throughs proving the layering,
//! not yet the real activation/validation/login workflows (device-limit
//! checks, status derivation, password verification) those endpoints will
//! need. Those, and the handlers that call them, land in a later,
//! separately approved phase.

pub mod auth_service;
pub mod error;
pub mod license_service;

pub use auth_service::AuthService;
pub use license_service::LicenseService;
