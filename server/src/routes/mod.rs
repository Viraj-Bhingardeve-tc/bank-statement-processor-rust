//! Route modules — one per endpoint group. License/payment/webhook routes
//! land in later phases (`PHASE4_DESIGN.md` §3), each as its own module
//! here, following this same pattern.

pub mod health;
pub mod ready;
