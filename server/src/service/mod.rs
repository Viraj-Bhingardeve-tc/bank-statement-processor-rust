//! `service/` — the business-logic layer (`PHASE4_DESIGN.md` §1.2). Each
//! service depends on repository *traits* (`Arc<dyn ...Repository>`), never
//! a concrete `Pg*` implementation directly, so it's testable against a
//! hand-written mock without a real database — see each module's own
//! tests.
//!
//! Phase 4D: `LicenseService` now implements the real
//! activate/validate/deactivate business logic behind
//! `routes::license`'s handlers. `AuthService` remains Phase 4C.2
//! scaffolding (a thin pass-through) — the real `/login` workflow
//! (password verification, session issuance) lands in a later,
//! separately approved phase.

pub mod auth_service;
pub mod error;
pub mod license_service;

pub use auth_service::AuthService;
pub use license_service::{
    ActivationOutcome, DeactivationOutcome, LicenseOperationError, LicenseService,
    ValidationOutcome,
};
