// settings.rs — Application settings persistence layer.

use anyhow::Result;
use rusqlite::Connection;
use crate::db;

pub const KEY_AI_PROVIDER: &str  = "ai_provider";
pub const KEY_AI_KEY:      &str  = "ai_api_key";
pub const KEY_AI_ENABLED:  &str  = "ai_enabled";
pub const KEY_LAST_CLIENT: &str  = "last_client_id";

#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub ai_provider:    String,   // "openai" | "claude" | "gemini"
    pub ai_api_key:     String,
    pub ai_enabled:     bool,
    pub last_client_id: Option<i64>,
}

impl Settings {
    pub fn load(conn: &Connection) -> Self {
        let get = |k: &str| db::get_setting(conn, k).ok().flatten().unwrap_or_default();
        let provider    = get(KEY_AI_PROVIDER);
        let key         = get(KEY_AI_KEY);
        let enabled_str = get(KEY_AI_ENABLED);
        let last_str    = get(KEY_LAST_CLIENT);
        Settings {
            ai_provider:    if provider.is_empty() { "openai".to_string() } else { provider },
            ai_api_key:     key,
            ai_enabled:     enabled_str == "1" || enabled_str == "true",
            last_client_id: last_str.parse::<i64>().ok(),
        }
    }

    pub fn save(&self, conn: &Connection) -> Result<()> {
        db::set_setting(conn, KEY_AI_PROVIDER, &self.ai_provider)?;
        db::set_setting(conn, KEY_AI_KEY,      &self.ai_api_key)?;
        db::set_setting(conn, KEY_AI_ENABLED,  if self.ai_enabled { "1" } else { "0" })?;
        if let Some(id) = self.last_client_id {
            db::set_setting(conn, KEY_LAST_CLIENT, &id.to_string())?;
        }
        Ok(())
    }
}
