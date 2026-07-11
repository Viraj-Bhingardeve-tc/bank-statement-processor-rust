//! Server configuration, loaded from environment variables.
//!
//! Phase 4B only needs what the process itself requires to bind a socket
//! and configure logging. `PHASE4_DESIGN.md` §6 ("Authentication and secret
//! management") lists the fuller secret set (`DATABASE_URL`,
//! `RAZORPAY_KEY_ID`/`_SECRET`, `RAZORPAY_WEBHOOK_SECRET`) this will grow to
//! cover once those subsystems land in later phases — deliberately not
//! read here yet, since nothing in this skeleton uses them.

use std::env;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub bind_addr: SocketAddr,
    pub log_filter: String,
}

impl AppConfig {
    /// Reads `HOST` (default `0.0.0.0`), `PORT` (default `8080`), and
    /// `RUST_LOG` (default `license_server=info,tower_http=info`) from the
    /// real process environment. A variable that's simply unset falls back
    /// to its default; a variable that's set but fails to parse is a real
    /// misconfiguration and fails startup loudly (`main.rs` exits non-zero)
    /// rather than silently substituting a default the operator didn't ask
    /// for.
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

        Ok(AppConfig {
            bind_addr: SocketAddr::new(host, port),
            log_filter,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    InvalidHost(String),
    InvalidPort(String),
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
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn defaults_apply_when_nothing_is_set() {
        let config = AppConfig::from_vars(vars(&[])).unwrap();
        assert_eq!(config.bind_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.log_filter, "license_server=info,tower_http=info");
    }

    #[test]
    fn explicit_host_and_port_are_honored() {
        let config =
            AppConfig::from_vars(vars(&[("HOST", "127.0.0.1"), ("PORT", "9090")])).unwrap();
        assert_eq!(config.bind_addr, "127.0.0.1:9090".parse().unwrap());
    }

    #[test]
    fn explicit_rust_log_is_honored() {
        let config = AppConfig::from_vars(vars(&[("RUST_LOG", "debug")])).unwrap();
        assert_eq!(config.log_filter, "debug");
    }

    #[test]
    fn invalid_host_is_a_config_error_not_a_silent_fallback() {
        let result = AppConfig::from_vars(vars(&[("HOST", "not-an-ip")]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::InvalidHost("not-an-ip".to_string())
        );
    }

    #[test]
    fn invalid_port_is_a_config_error_not_a_silent_fallback() {
        let result = AppConfig::from_vars(vars(&[("PORT", "not-a-number")]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::InvalidPort("not-a-number".to_string())
        );
    }

    #[test]
    fn port_out_of_u16_range_is_a_config_error() {
        let result = AppConfig::from_vars(vars(&[("PORT", "99999")]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::InvalidPort("99999".to_string())
        );
    }

    #[test]
    fn config_error_display_mentions_the_offending_value() {
        let err = ConfigError::InvalidPort("xyz".to_string());
        assert!(err.to_string().contains("xyz"));
    }
}
