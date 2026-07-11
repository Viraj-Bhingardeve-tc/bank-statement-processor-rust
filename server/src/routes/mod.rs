//! Route modules — one per endpoint group. Phase 4B has only `health`;
//! license/payment/webhook routes land in later phases (`PHASE4_DESIGN.md`
//! §3), each as its own module here, following this same pattern.

pub mod health;
