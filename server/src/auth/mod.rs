//! `auth/` — password, session-token, and webhook-signature crypto
//! utilities. Pure functions, no database, no business decisions (when a
//! password check should actually block a login, how long a session
//! lives, what a webhook event does once verified — that's
//! `service::auth_service`/`service::payment_service`, per
//! `PHASE4_DESIGN.md` §1.2's layering).
//!
//! Unrelated to the desktop's own `src/auth/monthly_password.rs` — that
//! module is documented there as "a licensing/anti-piracy gate, not an
//! access-control boundary" for the desktop app itself; this module is
//! this server's real account-credential and payment-webhook trust
//! machinery, a different threat model entirely (`LICENSE_SYSTEM_DESIGN.md`
//! §1 draws the same distinction for the two systems as a whole).

pub mod password;
pub mod token;
pub mod webhook_signature;
