// settings.rs — Application settings persistence layer.

use anyhow::Result;
use rusqlite::Connection;
use crate::db;

pub const KEY_AI_PROVIDER:           &str = "ai_provider";
pub const KEY_AI_KEY:                &str = "ai_api_key";
pub const KEY_AI_ENABLED:            &str = "ai_enabled";
pub const KEY_LAST_CLIENT:           &str = "last_client_id";
pub const KEY_NARR_ENABLED:          &str = "narr_enabled";
pub const KEY_NARR_TITLE_CASE:       &str = "narr_title_case";
pub const KEY_NARR_PRESERVE:         &str = "narr_preserve";
pub const KEY_GST_ENABLED:           &str = "gst_enabled";
pub const KEY_GST_AUTO_LEDGERS:      &str = "gst_auto_ledgers";
pub const KEY_RECON_DAYS:            &str = "recon_days";
pub const KEY_RECON_PCT:             &str = "recon_pct";
pub const KEY_LOG_LEVEL:             &str = "log_level";

#[derive(Debug, Clone)]
pub struct Settings {
    pub ai_provider:        String,   // "openai" | "claude" | "gemini"
    pub ai_api_key:         String,
    pub ai_enabled:         bool,
    pub last_client_id:     Option<i64>,
    // Narration cleaner
    pub narr_enabled:       bool,
    pub narr_title_case:    bool,
    pub narr_preserve:      bool,
    // GST engine
    pub gst_enabled:        bool,
    pub gst_auto_ledgers:   bool,
    // Reconciliation
    pub recon_days:         i32,    // ±N days for date fuzzy matching
    pub recon_pct:          f64,    // % tolerance for amount matching
    // Logging
    pub log_level:          String, // "INFO" | "DEBUG" | "WARN" | "ERROR"
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ai_provider:        "openai".to_string(),
            ai_api_key:         String::new(),
            ai_enabled:         false,
            last_client_id:     None,
            narr_enabled:       true,
            narr_title_case:    true,
            narr_preserve:      false,
            gst_enabled:        true,
            gst_auto_ledgers:   true,
            recon_days:         3,
            recon_pct:          0.5,
            log_level:          "INFO".to_string(),
        }
    }
}

fn b(s: &str) -> bool { s == "1" || s == "true" }

impl Settings {
    pub fn load(conn: &Connection) -> Self {
        let get = |k: &str| db::get_setting(conn, k).ok().flatten().unwrap_or_default();
        let provider    = get(KEY_AI_PROVIDER);
        Settings {
            ai_provider:        if provider.is_empty() { "openai".to_string() } else { provider },
            ai_api_key:         get(KEY_AI_KEY),
            ai_enabled:         b(&get(KEY_AI_ENABLED)),
            last_client_id:     get(KEY_LAST_CLIENT).parse::<i64>().ok(),
            narr_enabled:       { let v = get(KEY_NARR_ENABLED);    if v.is_empty() { true  } else { b(&v) } },
            narr_title_case:    { let v = get(KEY_NARR_TITLE_CASE); if v.is_empty() { true  } else { b(&v) } },
            narr_preserve:      b(&get(KEY_NARR_PRESERVE)),
            gst_enabled:        { let v = get(KEY_GST_ENABLED);      if v.is_empty() { true  } else { b(&v) } },
            gst_auto_ledgers:   { let v = get(KEY_GST_AUTO_LEDGERS); if v.is_empty() { true  } else { b(&v) } },
            recon_days:         get(KEY_RECON_DAYS).parse::<i32>().unwrap_or(3),
            recon_pct:          get(KEY_RECON_PCT).parse::<f64>().unwrap_or(0.5),
            log_level:          { let v = get(KEY_LOG_LEVEL); if v.is_empty() { "INFO".to_string() } else { v } },
        }
    }

    pub fn save(&self, conn: &Connection) -> Result<()> {
        db::set_setting(conn, KEY_AI_PROVIDER,      &self.ai_provider)?;
        db::set_setting(conn, KEY_AI_KEY,           &self.ai_api_key)?;
        db::set_setting(conn, KEY_AI_ENABLED,       if self.ai_enabled { "1" } else { "0" })?;
        db::set_setting(conn, KEY_NARR_ENABLED,     if self.narr_enabled { "1" } else { "0" })?;
        db::set_setting(conn, KEY_NARR_TITLE_CASE,  if self.narr_title_case { "1" } else { "0" })?;
        db::set_setting(conn, KEY_NARR_PRESERVE,    if self.narr_preserve { "1" } else { "0" })?;
        db::set_setting(conn, KEY_GST_ENABLED,      if self.gst_enabled { "1" } else { "0" })?;
        db::set_setting(conn, KEY_GST_AUTO_LEDGERS, if self.gst_auto_ledgers { "1" } else { "0" })?;
        db::set_setting(conn, KEY_RECON_DAYS,       &self.recon_days.to_string())?;
        db::set_setting(conn, KEY_RECON_PCT,        &self.recon_pct.to_string())?;
        db::set_setting(conn, KEY_LOG_LEVEL,        &self.log_level)?;
        if let Some(id) = self.last_client_id {
            db::set_setting(conn, KEY_LAST_CLIENT, &id.to_string())?;
        }
        Ok(())
    }
}
