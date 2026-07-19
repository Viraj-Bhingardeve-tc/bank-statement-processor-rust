// lib.rs — Public library surface for the Bank Statement Processor.

#[cfg(feature = "ai")]
pub mod ai_classifier;

pub mod analytics;
pub mod auth;
pub mod classifier;
mod credential_store;
pub mod db;
pub mod export;
pub mod gst_engine;
pub mod license;
pub mod migration;
pub mod narration_cleaner;
pub mod parser;
pub mod reconciliation;
pub mod settings;
pub mod tally_group_engine;
pub mod text_safety;
pub mod ui;
