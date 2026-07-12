//! `auth/` — password and session-token crypto utilities. Pure functions,
//! no database, no business decisions (when a password check should
//! actually block a login, how long a session lives — that's
//! `service::auth_service`, per `PHASE4_DESIGN.md` §1.2's layering).
//!
//! Unrelated to the desktop's own `src/auth/monthly_password.rs` — that
//! module is documented there as "a licensing/anti-piracy gate, not an
//! access-control boundary" for the desktop app itself; this module is
//! this server's real account-credential storage, a different threat
//! model entirely (`LICENSE_SYSTEM_DESIGN.md` §1 draws the same
//! distinction for the two systems as a whole).

pub mod password;
pub mod token;
