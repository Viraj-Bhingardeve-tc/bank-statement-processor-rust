//! `Device` — one row per (license, physical machine) activation (`devices`
//! table, `LICENSE_DATABASE_SCHEMA.md` §1). `device_id` mirrors the same
//! client-generated UUID the desktop's `src/license/fingerprint.rs`
//! produces once per installation and never regenerates.

use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: i64,
    pub license_id: i64,
    pub device_id: Uuid,
    pub machine_fingerprint: String,
    pub device_label: Option<String>,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub deactivated_at: Option<DateTime<Utc>>,
}
