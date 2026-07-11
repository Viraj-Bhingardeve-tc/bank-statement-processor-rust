//! Server configuration, loaded from environment variables.
//!
//! `PHASE4_DESIGN.md` §6 ("Authentication and secret management") lists the
//! fuller secret set (`RAZORPAY_KEY_ID`/`_SECRET`, `RAZORPAY_WEBHOOK_SECRET`)
//! this will grow to cover once those subsystems land in later phases —
//! deliberately not read here yet, since nothing in this phase uses them.

use std::env;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub bind_addr: SocketAddr,
    pub log_filter: String,
    pub database_url: String,
    pub database_max_connections: u32,
}

impl AppConfig {
    /// Reads `HOST` (default `0.0.0.0`), `PORT` (default `8080`),
    /// `RUST_LOG` (default `license_server=info,tower_http=info`),
    /// `DATABASE_URL` (**required** — no default, since silently falling
    /// back to some hardcoded local connection string risks a production
    /// deploy quietly pointing at nothing, or at a developer's own machine),
    /// and `DATABASE_MAX_CONNECTIONS` (default `5`) from the real process
    /// environment. A variable that's simply unset falls back to its
    /// default (or errors, for `DATABASE_URL`); a variable that's set but
    /// fails to parse is a real misconfiguration and fails startup loudly
    /// (`main.rs` exits non-zero) rather than silently substituting a
    /// default the operator didn't ask for.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(|key| env::var(key).ok())
    }

    /// The actual parsing logic, parameterized over a variable lookup
    /// function instead of reading `std::env` directly — lets tests supply
    /// fixed values without mutating real process environment state (which
    /// would otherwise race across `cargo test`'s parallel test threads).
    fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let host = match get("HOST") {
            Some(v) => v
                .parse::<IpAddr>()
                .map_err(|_| ConfigError::InvalidHost(v))?,
            None => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        };
        let port = match get("PORT") {
            Some(v) => v.parse::<u16>().map_err(|_| ConfigError::InvalidPort(v))?,
            None => 8080,
        };
        let log_filter =
            get("RUST_LOG").unwrap_or_else(|| "license_server=info,tower_http=info".to_string());
        let database_url = get("DATABASE_URL").ok_or(ConfigError::MissingDatabaseUrl)?;
        let database_max_connections = match get("DATABASE_MAX_CONNECTIONS") {
            Some(v) => v
                .parse::<u32>()
                .map_err(|_| ConfigError::InvalidDatabaseMaxConnections(v))?,
            None => 5,
        };

        Ok(AppConfig {
            bind_addr: SocketAddr::new(host, port),
            log_filter,
            database_url,
            database_max_connections,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    InvalidHost(String),
    InvalidPort(String),
    MissingDatabaseUrl,
    InvalidDatabaseMaxConnections(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::InvalidHost(v) => {
                write!(f, "invalid HOST value {v:?} (expected an IP address)")
            }
            ConfigError::InvalidPort(v) => {
                write!(f, "invalid PORT value {v:?} (expected a number 0-65535)")
            }
            ConfigError::MissingDatabaseUrl => {
                write!(
                    f,
                    "DATABASE_URL is not set (required — no default connection string)"
                )
            }
            ConfigError::InvalidDatabaseMaxConnections(v) => {
                write!(
                    f,
                    "invalid DATABASE_MAX_CONNECTIONS value {v:?} (expected a positive number)"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    const DB_URL: &str = "postgres://user:pass@localhost:5432/license_server";

    fn vars(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn defaults_apply_when_only_database_url_is_set() {
        let config = AppConfig::from_vars(vars(&[("DATABASE_URL", DB_URL)])).unwrap();
        assert_eq!(config.bind_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.log_filter, "license_server=info,tower_http=info");
        assert_eq!(config.database_url, DB_URL);
        assert_eq!(config.database_max_connections, 5);
    }

    #[test]
    fn explicit_host_and_port_are_honored() {
        let config = AppConfig::from_vars(vars(&[
            ("HOST", "127.0.0.1"),
            ("PORT", "9090"),
            ("DATABASE_URL", DB_URL),
        ]))
        .unwrap();
        assert_eq!(config.bind_addr, "127.0.0.1:9090".parse().unwrap());
    }

    #[test]
    fn explicit_rust_log_is_honored() {
        let config =
            AppConfig::from_vars(vars(&[("RUST_LOG", "debug"), ("DATABASE_URL", DB_URL)])).unwrap();
        assert_eq!(config.log_filter, "debug");
    }

    #[test]
    fn explicit_database_max_connections_is_honored() {
        let config = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("DATABASE_MAX_CONNECTIONS", "20"),
        ]))
        .unwrap();
        assert_eq!(config.database_max_connections, 20);
    }

    #[test]
    fn invalid_host_is_a_config_error_not_a_silent_fallback() {
        let result = AppConfig::from_vars(vars(&[("HOST", "not-an-ip"), ("DATABASE_URL", DB_URL)]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::InvalidHost("not-an-ip".to_string())
        );
    }

    #[test]
    fn invalid_port_is_a_config_error_not_a_silent_fallback() {
        let result =
            AppConfig::from_vars(vars(&[("PORT", "not-a-number"), ("DATABASE_URL", DB_URL)]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::InvalidPort("not-a-number".to_string())
        );
    }

    #[test]
    fn port_out_of_u16_range_is_a_config_error() {
        let result = AppConfig::from_vars(vars(&[("PORT", "99999"), ("DATABASE_URL", DB_URL)]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::InvalidPort("99999".to_string())
        );
    }

    #[test]
    fn missing_database_url_is_a_config_error_not_a_default_connection_string() {
        let result = AppConfig::from_vars(vars(&[]));
        assert_eq!(result.unwrap_err(), ConfigError::MissingDatabaseUrl);
    }

    #[test]
    fn invalid_database_max_connections_is_a_config_error() {
        let result = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("DATABASE_MAX_CONNECTIONS", "not-a-number"),
        ]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::InvalidDatabaseMaxConnections("not-a-number".to_string())
        );
    }

    #[test]
    fn config_error_display_mentions_the_offending_value() {
        let err = ConfigError::InvalidPort("xyz".to_string());
        assert!(err.to_string().contains("xyz"));
    }
}
