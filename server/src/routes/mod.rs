//! Route modules — one per endpoint group. Payment/webhook routes land in
//! a later phase (`PHASE4_DESIGN.md` §3), following this same pattern.

pub mod auth;
pub mod error;
pub mod health;
pub mod license;
pub mod ready;
