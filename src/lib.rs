// lib.rs — Public library surface for the Bank Statement Processor.
pub mod text_safety;
pub mod parser;
pub mod analytics;
pub mod classifier;
pub mod export;
pub mod db;
pub mod narration_cleaner;
pub mod tally_group_engine;
pub mod gst_engine;
pub mod settings;
pub mod reconciliation;
pub mod auth;
pub mod ui;
#[cfg(feature = "ai")]
pub mod ai_classifier;
pub mod migration;
pub mod license;
