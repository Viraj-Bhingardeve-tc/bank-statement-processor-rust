//! `service/` — the business-logic layer (`PHASE4_DESIGN.md` §1.2). Each
//! service depends on repository *traits* (`Arc<dyn ...Repository>`), never
//! a concrete `Pg*` implementation directly, so it's testable against a
//! hand-written mock without a real database — see each module's own
//! tests.
//!
//! Phase 4D implemented `LicenseService`'s real
//! activate/validate/deactivate business logic. Phase 4E does the same for
//! `AuthService`: real Argon2-backed login, session validation, and
//! logout/revocation, behind `routes::auth`'s handlers and
//! `require_session` middleware.

pub mod auth_service;
pub mod error;
pub mod license_service;

pub use auth_service::{AuthError, AuthService, LoginOutcome};
pub use license_service::{
    ActivationOutcome, DeactivationOutcome, LicenseOperationError, LicenseService,
    ValidationOutcome,
};
