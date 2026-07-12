//! `service/` — the business-logic layer (`PHASE4_DESIGN.md` §1.2). Each
//! service depends on repository/client *traits* (`Arc<dyn ...>`), never a
//! concrete `Pg*`/`HttpRazorpayClient` implementation directly, so it's
//! testable against a hand-written mock without a real database or
//! network call — see each module's own tests.
//!
//! Phase 4D implemented `LicenseService`'s real
//! activate/validate/deactivate business logic. Phase 4E did the same for
//! `AuthService` (login/session validation/logout). Phase 4F adds
//! `PaymentService`: checkout-session creation and Razorpay webhook
//! processing, behind `routes::payment`'s handlers.

pub mod auth_service;
pub mod error;
pub mod license_service;
pub mod payment_service;

pub use auth_service::{AuthError, AuthService, LoginOutcome};
pub use license_service::{
    ActivationOutcome, DeactivationOutcome, LicenseOperationError, LicenseService,
    ValidationOutcome,
};
pub use payment_service::{CheckoutOutcome, PaymentOperationError, PaymentService};
