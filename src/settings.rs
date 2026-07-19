// settings.rs — Application settings persistence layer.

use crate::db;
use anyhow::Result;
use rusqlite::Connection;

// The AI provider API key is sensitive (it authenticates the user's own
// OpenAI/Claude/Gemini account) and is deliberately NOT stored in the
// `settings` SQLite table, which is plaintext on disk. It lives in the OS
// credential store instead (Windows Credential Manager / macOS Keychain /
// Linux Secret Service via the `keyring` crate).
const KEYRING_SERVICE: &str = "bank-statement-processor";
// Tests run against `credential_store::store()`'s in-memory backend (see
// that module), never the real OS keyring, so no separate test username is
// needed here to avoid colliding with a real user's saved credential.
const KEYRING_USERNAME: &str = "ai_api_key";
/// Legacy plaintext settings-table key, retained only to find and purge
/// pre-existing values left over from before this fix.
const LEGACY_DB_KEY_AI_KEY: &str = "ai_api_key";

/// Reads the AI API key from the OS credential store. Returns an empty
/// string if no key has been set yet or the platform store is unavailable —
/// matching the previous plaintext default exactly.
fn load_ai_key() -> String {
    crate::credential_store::store()
        .get_password(KEYRING_SERVICE, KEYRING_USERNAME)
        .unwrap_or_default()
}

/// One-time migration: if an older version of the app left a plaintext AI
/// key in the `settings` table, move it into the OS credential store and
/// delete the plaintext row so it no longer sits on disk in cleartext.
/// Safe to call on every load — it's a no-op once the legacy row is gone.
fn migrate_legacy_plaintext_ai_key(conn: &Connection) -> String {
    let legacy = db::get_setting(conn, LEGACY_DB_KEY_AI_KEY)
        .ok()
        .flatten()
        .unwrap_or_default();
    if legacy.is_empty() {
        return load_ai_key();
    }
    log::info!(
        "[Settings] migrating AI API key out of plaintext storage into the OS credential store"
    );
    save_ai_key(&legacy);
    if let Err(e) = db::delete_setting(conn, LEGACY_DB_KEY_AI_KEY) {
        log::error!(
            "[Settings] failed to remove legacy plaintext AI key row: {}",
            e
        );
    }
    load_ai_key()
}

/// Persists the AI API key to the OS credential store. A failure here is
/// logged but not propagated as a hard error — mirrors how every other
/// individual setting in `Settings::save` is best-effort.
fn save_ai_key(key: &str) {
    use crate::credential_store::CredentialError;
    let store = crate::credential_store::store();
    let result = if key.is_empty() {
        // An empty key means "cleared" — delete rather than store an empty secret.
        store
            .delete_credential(KEYRING_SERVICE, KEYRING_USERNAME)
            .or_else(|e| match e {
                CredentialError::NoEntry => Ok(()),
                other => Err(other),
            })
    } else {
        store.set_password(KEYRING_SERVICE, KEYRING_USERNAME, key)
    };
    if let Err(e) = result {
        log::error!(
            "[Settings] failed to persist AI API key to OS credential store: {}",
            e
        );
    }
}

pub const KEY_AI_PROVIDER: &str = "ai_provider";
pub const KEY_AI_ENABLED: &str = "ai_enabled";
pub const KEY_LAST_CLIENT: &str = "last_client_id";
pub const KEY_NARR_ENABLED: &str = "narr_enabled";
pub const KEY_NARR_TITLE_CASE: &str = "narr_title_case";
pub const KEY_NARR_PRESERVE: &str = "narr_preserve";
pub const KEY_GST_ENABLED: &str = "gst_enabled";
pub const KEY_GST_AUTO_LEDGERS: &str = "gst_auto_ledgers";
pub const KEY_RECON_DAYS: &str = "recon_days";
pub const KEY_RECON_PCT: &str = "recon_pct";
pub const KEY_LOG_LEVEL: &str = "log_level";
pub const KEY_DEFAULT_STATE_IDX: &str = "default_state_idx";

#[derive(Debug, Clone)]
pub struct Settings {
    pub ai_provider: String, // "openai" | "claude" | "gemini"
    pub ai_api_key: String,
    pub ai_enabled: bool,
    pub last_client_id: Option<i64>,
    // Narration cleaner
    pub narr_enabled: bool,
    pub narr_title_case: bool,
    pub narr_preserve: bool,
    // GST engine
    pub gst_enabled: bool,
    pub gst_auto_ledgers: bool,
    // Reconciliation
    pub recon_days: i32, // ±N days for date fuzzy matching
    pub recon_pct: f64,  // % tolerance for amount matching
    // Logging
    pub log_level: String, // "INFO" | "DEBUG" | "WARN" | "ERROR"
    // GST engine — default state used to pre-fill the Export Wizard's state
    // selector (index into the same 9-entry state list the wizard itself uses).
    pub default_state_idx: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ai_provider: "openai".to_string(),
            ai_api_key: String::new(),
            ai_enabled: false,
            last_client_id: None,
            narr_enabled: true,
            narr_title_case: true,
            narr_preserve: false,
            gst_enabled: true,
            gst_auto_ledgers: true,
            recon_days: 3,
            recon_pct: 0.5,
            log_level: "INFO".to_string(),
            default_state_idx: 0,
        }
    }
}

fn b(s: &str) -> bool {
    s == "1" || s == "true"
}

impl Settings {
    pub fn load(conn: &Connection) -> Self {
        let get = |k: &str| db::get_setting(conn, k).ok().flatten().unwrap_or_default();
        let provider = get(KEY_AI_PROVIDER);
        Settings {
            ai_provider: if provider.is_empty() {
                "openai".to_string()
            } else {
                provider
            },
            ai_api_key: migrate_legacy_plaintext_ai_key(conn),
            ai_enabled: b(&get(KEY_AI_ENABLED)),
            last_client_id: get(KEY_LAST_CLIENT).parse::<i64>().ok(),
            narr_enabled: {
                let v = get(KEY_NARR_ENABLED);
                if v.is_empty() {
                    true
                } else {
                    b(&v)
                }
            },
            narr_title_case: {
                let v = get(KEY_NARR_TITLE_CASE);
                if v.is_empty() {
                    true
                } else {
                    b(&v)
                }
            },
            narr_preserve: b(&get(KEY_NARR_PRESERVE)),
            gst_enabled: {
                let v = get(KEY_GST_ENABLED);
                if v.is_empty() {
                    true
                } else {
                    b(&v)
                }
            },
            gst_auto_ledgers: {
                let v = get(KEY_GST_AUTO_LEDGERS);
                if v.is_empty() {
                    true
                } else {
                    b(&v)
                }
            },
            recon_days: get(KEY_RECON_DAYS).parse::<i32>().unwrap_or(3),
            recon_pct: get(KEY_RECON_PCT).parse::<f64>().unwrap_or(0.5),
            log_level: {
                let v = get(KEY_LOG_LEVEL);
                if v.is_empty() {
                    "INFO".to_string()
                } else {
                    v
                }
            },
            default_state_idx: get(KEY_DEFAULT_STATE_IDX).parse::<i32>().unwrap_or(0),
        }
    }

    pub fn save(&self, conn: &Connection) -> Result<()> {
        db::set_setting(conn, KEY_AI_PROVIDER, &self.ai_provider)?;
        save_ai_key(&self.ai_api_key);
        db::set_setting(
            conn,
            KEY_AI_ENABLED,
            if self.ai_enabled { "1" } else { "0" },
        )?;
        db::set_setting(
            conn,
            KEY_NARR_ENABLED,
            if self.narr_enabled { "1" } else { "0" },
        )?;
        db::set_setting(
            conn,
            KEY_NARR_TITLE_CASE,
            if self.narr_title_case { "1" } else { "0" },
        )?;
        db::set_setting(
            conn,
            KEY_NARR_PRESERVE,
            if self.narr_preserve { "1" } else { "0" },
        )?;
        db::set_setting(
            conn,
            KEY_GST_ENABLED,
            if self.gst_enabled { "1" } else { "0" },
        )?;
        db::set_setting(
            conn,
            KEY_GST_AUTO_LEDGERS,
            if self.gst_auto_ledgers { "1" } else { "0" },
        )?;
        db::set_setting(conn, KEY_RECON_DAYS, &self.recon_days.to_string())?;
        db::set_setting(conn, KEY_RECON_PCT, &self.recon_pct.to_string())?;
        db::set_setting(conn, KEY_LOG_LEVEL, &self.log_level)?;
        db::set_setting(
            conn,
            KEY_DEFAULT_STATE_IDX,
            &self.default_state_idx.to_string(),
        )?;
        if let Some(id) = self.last_client_id {
            db::set_setting(conn, KEY_LAST_CLIENT, &id.to_string())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // All tests below share one real OS keyring entry (KEYRING_USERNAME).
    // Rust runs tests in parallel by default, so without serializing them
    // here they race on that shared entry and intermittently fail.
    static KEYRING_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Cleans up the test keyring entry so repeated test runs start fresh
    /// regardless of pass/fail/panic in a previous run.
    fn clear_test_entry() {
        let _ =
            crate::credential_store::store().delete_credential(KEYRING_SERVICE, KEYRING_USERNAME);
    }

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        // Recover from a poisoned lock (a previous test panicking mid-assert)
        // rather than letting that cascade into failing every later test too.
        KEYRING_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn ai_key_roundtrips_through_keyring_not_plaintext() {
        let _guard = lock();
        clear_test_entry();
        save_ai_key("sk-test-12345");
        assert_eq!(load_ai_key(), "sk-test-12345");
        clear_test_entry();
        assert_eq!(load_ai_key(), "", "key must be gone after delete");
    }

    #[test]
    fn empty_key_clears_existing_entry() {
        let _guard = lock();
        clear_test_entry();
        save_ai_key("sk-temp");
        assert_eq!(load_ai_key(), "sk-temp");
        save_ai_key("");
        assert_eq!(
            load_ai_key(),
            "",
            "saving an empty key must clear the stored secret"
        );
        clear_test_entry();
    }

    #[test]
    fn migrate_moves_legacy_plaintext_row_into_keyring_and_deletes_it() {
        let _guard = lock();
        clear_test_entry();
        let conn = db::open(":memory:").expect("open in-memory db");
        db::set_setting(&conn, LEGACY_DB_KEY_AI_KEY, "sk-legacy-plaintext").unwrap();

        let migrated = migrate_legacy_plaintext_ai_key(&conn);

        assert_eq!(migrated, "sk-legacy-plaintext");
        assert_eq!(
            load_ai_key(),
            "sk-legacy-plaintext",
            "key must now live in the keyring"
        );
        let leftover = db::get_setting(&conn, LEGACY_DB_KEY_AI_KEY).unwrap();
        assert!(
            leftover.is_none(),
            "plaintext row must be deleted after migration"
        );
        clear_test_entry();
    }

    #[test]
    fn migrate_is_a_noop_once_legacy_row_is_already_gone() {
        let _guard = lock();
        clear_test_entry();
        let conn = db::open(":memory:").expect("open in-memory db");
        // No legacy row inserted — should fall straight through to load_ai_key().
        assert_eq!(migrate_legacy_plaintext_ai_key(&conn), "");
    }

    #[test]
    fn defaults_match_current_shipped_behavior() {
        // These defaults are load-bearing: `apply_parse_result`/`classify_one`
        // key their "did the user actually change anything?" gating off them,
        // so a first-run (never-saved) Settings must reproduce the exact
        // pipeline behavior that shipped before those settings existed.
        let d = Settings::default();
        assert!(d.narr_enabled);
        assert!(d.narr_title_case);
        assert!(!d.narr_preserve);
        assert!(d.gst_enabled);
        assert!(d.gst_auto_ledgers);
        assert_eq!(d.default_state_idx, 0);
    }

    #[test]
    fn non_ai_settings_round_trip_through_save_and_load() {
        // `Settings::save` also calls `save_ai_key` (with the default empty
        // key, which clears the shared test keyring entry) — guard against
        // the same cross-test keyring race the AI-key tests above guard against.
        let _guard = lock();
        let conn = db::open(":memory:").expect("open in-memory db");
        let cfg = Settings {
            narr_enabled: false,
            narr_title_case: false,
            narr_preserve: true,
            gst_enabled: false,
            gst_auto_ledgers: false,
            recon_days: 7,
            recon_pct: 1.5,
            log_level: "DEBUG".to_string(),
            default_state_idx: 3,
            ..Default::default()
        };

        cfg.save(&conn).expect("save");
        let reloaded = Settings::load(&conn);

        assert!(!reloaded.narr_enabled);
        assert!(!reloaded.narr_title_case);
        assert!(reloaded.narr_preserve);
        assert!(!reloaded.gst_enabled);
        assert!(!reloaded.gst_auto_ledgers);
        assert_eq!(reloaded.recon_days, 7);
        assert_eq!(reloaded.recon_pct, 1.5);
        assert_eq!(reloaded.log_level, "DEBUG");
        assert_eq!(reloaded.default_state_idx, 3);
    }
}
