//! Server configuration — the one place this crate reads environment
//! variables from. Loads only from real process environment variables
//! (`AppConfig::from_env`, backed by `std::env::var`) or a developer's own
//! uncommitted `server/.env` sourced by their shell/tooling before running
//! (this crate does not load `.env` files itself — see
//! `server/README.md`'s "Running locally without Docker" section); nothing
//! is ever hardcoded here.
//!
//! **Phase 4J.9 (secrets management & configuration hardening):** every
//! actual secret value — [`DatabaseConfig::url`],
//! [`PaymentConfig::razorpay_key_id`]/[`razorpay_key_secret`]/
//! [`razorpay_webhook_secret`] — is wrapped in [`Secret`], whose `Debug`
//! impl always prints `***REDACTED***` regardless of the wrapped value.
//! Because `#[derive(Debug)]` on any struct recurses into each field's own
//! `Debug` impl, this means `AppConfig`/`ServerConfig`/`DatabaseConfig`/
//! `PaymentConfig` can all safely derive `Debug` themselves (useful for
//! e.g. a future startup log line) without any risk of a secret leaking
//! through it, now or if a field is ever added later — the redaction lives
//! in the type, not in a hand-maintained list of "don't log this field."
//! `Secret<T>` wraps an `Arc<T>` internally, so cloning one (needed because
//! `AppConfig` itself is `Clone` — `AppState` holds `Arc<AppConfig>`, and
//! `AppState::new` clones individual config sub-fields into service
//! constructors) is a refcount bump, never a copy of the actual secret
//! bytes.
//!
//! **Why there is no `JwtConfig`/`MailConfig` here:** this codebase has
//! neither. Session tokens are random 256-bit values from the OS CSPRNG
//! (`auth::token::generate_session_token`), deliberately *not* signed —
//! `PHASE4_DESIGN.md` §6: "no signing secret needed for random opaque
//! tokens, unlike JWT" — and there is no SMTP/mail sender anywhere in this
//! crate. Adding typed config structs for mechanisms that don't exist would
//! be exactly the "looks done but isn't" shape this project's own audits
//! have flagged elsewhere (computed-but-unused fields, UI wired to nothing).
//! If/when either is actually built, it gets its own `*Config` struct
//! following the same pattern as `DatabaseConfig`/`PaymentConfig` below —
//! not before.
//!
//! `SESSION_TOKEN_SECRET`/`ARGON2_SECRET` in `server/.env.example` are the
//! same "reserved for future use, not read by this module" placeholders
//! they've always been — Argon2 here uses a random per-password salt, not
//! a pepper/secret key, and session tokens need no signing secret at all
//! (both documented at their own definitions in `.env.example`) — so
//! neither is a currently-real secret this module has anything to load or
//! validate.

use std::env;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

/// Wraps a secret value so it can never be accidentally printed, logged,
/// or otherwise leaked via `Debug`/`{:?}` formatting — including
/// transitively, since a struct containing a `Secret` field that derives
/// `Debug` recurses into *this* impl, not the wrapped type's own. The only
/// way to read the real value is [`Secret::expose_secret`] — a
/// deliberate, grep-able call, never an automatic `Deref`/`Display`/
/// `AsRef`, which would let the value leak through an unlabeled format
/// string or string concatenation without that intent being visible at the
/// call site.
///
/// Backed by `Arc<T>`, not an owned `T` — cloning a `Secret` is a refcount
/// bump, never a copy of the actual secret bytes (see this module's own
/// doc comment on why that matters here).
#[derive(Clone)]
pub struct Secret<T>(Arc<T>);

impl<T> Secret<T> {
    pub fn new(value: T) -> Self {
        Secret(Arc::new(value))
    }

    /// The one, explicit, intentionally-named way to read the real value.
    pub fn expose_secret(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(\"***REDACTED***\")")
    }
}

impl<T: PartialEq> PartialEq for Secret<T> {
    fn eq(&self, other: &Self) -> bool {
        *self.0 == *other.0
    }
}

impl<T: Eq> Eq for Secret<T> {}

impl<T> From<T> for Secret<T> {
    fn from(value: T) -> Self {
        Secret::new(value)
    }
}

/// `HOST`/`PORT`/`RUST_LOG` — nothing secret here, just where and how
/// loudly this process listens and logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub bind_addr: SocketAddr,
    pub log_filter: String,
}

/// `DATABASE_URL`/`DATABASE_MAX_CONNECTIONS` — `url` is a real secret (it
/// embeds the database password), hence [`Secret`]; `max_connections` is
/// just a pool-size tuning number, not sensitive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseConfig {
    pub url: Secret<String>,
    pub max_connections: u32,
}

/// `RAZORPAY_KEY_ID`/`RAZORPAY_KEY_SECRET`/`RAZORPAY_WEBHOOK_SECRET`/
/// `RAZORPAY_MONTHLY_PLAN_ID`/`RAZORPAY_YEARLY_PLAN_ID` — the credential
/// trio is wrapped in [`Secret`] (Key ID doubles as the HTTP Basic Auth
/// username against Razorpay's API, so it's treated as sensitive too, not
/// just the secret proper); the two plan ids are Razorpay-side
/// configuration identifiers, not credentials, so they stay plain
/// `Option<String>`. All five independently optional — a missing set means
/// Razorpay isn't configured in this environment (`PHASE4_DESIGN.md` §6),
/// a normal state for local dev/early staging, not a startup failure
/// (unchanged behavior from before this phase — see
/// `AppConfig::from_vars`'s doc comment on the one new check this phase
/// *does* add: internal consistency between `key_id` and `key_secret`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentConfig {
    pub razorpay_key_id: Option<Secret<String>>,
    pub razorpay_key_secret: Option<Secret<String>>,
    pub razorpay_webhook_secret: Option<Secret<String>>,
    pub razorpay_monthly_plan_id: Option<String>,
    pub razorpay_yearly_plan_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub payment: PaymentConfig,
}

impl AppConfig {
    /// Reads `HOST` (default `0.0.0.0`), `PORT` (default `8080`),
    /// `RUST_LOG` (default `license_server=info,tower_http=info`),
    /// `DATABASE_URL` (**required**, and must look like a Postgres
    /// connection string — no default, since silently falling back to
    /// some hardcoded local connection string risks a production deploy
    /// quietly pointing at nothing, or at a developer's own machine), and
    /// `DATABASE_MAX_CONNECTIONS` (default `5`) from the real process
    /// environment. A variable that's simply unset falls back to its
    /// default (or errors, for `DATABASE_URL`); a variable that's set but
    /// fails to parse/validate is a real misconfiguration and fails
    /// startup loudly (`main.rs` exits non-zero) rather than silently
    /// substituting a default the operator didn't ask for.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(|key| env::var(key).ok())
    }

    /// The actual parsing/validation logic, parameterized over a variable
    /// lookup function instead of reading `std::env` directly — lets
    /// tests supply fixed values without mutating real process
    /// environment state (which would otherwise race across `cargo
    /// test`'s parallel test threads).
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
        // Scheme-only check — deliberately not a full connection-string
        // parse (that's `sqlx`'s job, at pool-build time in `main.rs`,
        // which already fails startup loudly on a genuinely malformed
        // URL). This just catches the "not even a Postgres URL at all"
        // class of mistake earlier, with a clear message, and — critically
        // — without ever echoing the offending value back (see
        // `ConfigError::InvalidDatabaseUrl`'s own doc comment: unlike
        // `HOST`/`PORT`, a malformed `DATABASE_URL` may still contain a
        // real password even though it fails this check).
        if !database_url.starts_with("postgres://") && !database_url.starts_with("postgresql://") {
            return Err(ConfigError::InvalidDatabaseUrl);
        }
        let database_max_connections = match get("DATABASE_MAX_CONNECTIONS") {
            Some(v) => v
                .parse::<u32>()
                .map_err(|_| ConfigError::InvalidDatabaseMaxConnections(v))?,
            None => 5,
        };

        let razorpay_key_id = get("RAZORPAY_KEY_ID");
        let razorpay_key_secret = get("RAZORPAY_KEY_SECRET");
        // Both-or-neither: a half-configured pair (one set, one missing)
        // is never a valid state — the missing half would only be
        // discovered later, at first-checkout-attempt, as a
        // `PROVIDER_ERROR` (`razorpay::HttpRazorpayClient::credentials`).
        // Failing at startup instead surfaces a real misconfiguration
        // immediately rather than the first time a customer tries to pay.
        // This does not change the fully-unset case at all — that's still
        // "Razorpay isn't configured here," not an error (unchanged from
        // before this phase).
        if razorpay_key_id.is_some() != razorpay_key_secret.is_some() {
            return Err(ConfigError::IncompleteRazorpayCredentials);
        }

        Ok(AppConfig {
            server: ServerConfig {
                bind_addr: SocketAddr::new(host, port),
                log_filter,
            },
            database: DatabaseConfig {
                url: Secret::new(database_url),
                max_connections: database_max_connections,
            },
            payment: PaymentConfig {
                razorpay_key_id: razorpay_key_id.map(Secret::new),
                razorpay_key_secret: razorpay_key_secret.map(Secret::new),
                razorpay_webhook_secret: get("RAZORPAY_WEBHOOK_SECRET").map(Secret::new),
                razorpay_monthly_plan_id: get("RAZORPAY_MONTHLY_PLAN_ID"),
                razorpay_yearly_plan_id: get("RAZORPAY_YEARLY_PLAN_ID"),
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    InvalidHost(String),
    InvalidPort(String),
    MissingDatabaseUrl,
    /// No message payload, deliberately — unlike `HOST`/`PORT` (never
    /// secret), an invalid `DATABASE_URL` may still contain a real
    /// password even though it fails this check (e.g. a bad port with a
    /// perfectly real, sensitive password still embedded ahead of it), so
    /// this variant never carries the offending value, only a fixed,
    /// generic message (`Display`, below).
    InvalidDatabaseUrl,
    InvalidDatabaseMaxConnections(String),
    /// `RAZORPAY_KEY_ID` and `RAZORPAY_KEY_SECRET` were not both set or
    /// both unset. No message payload for the same reason as
    /// `InvalidDatabaseUrl` — whichever *is* set could be a real secret.
    IncompleteRazorpayCredentials,
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
            ConfigError::InvalidDatabaseUrl => write!(
                f,
                "DATABASE_URL must start with postgres:// or postgresql:// \
                 (the value itself is not logged, since it may contain credentials)"
            ),
            ConfigError::InvalidDatabaseMaxConnections(v) => {
                write!(
                    f,
                    "invalid DATABASE_MAX_CONNECTIONS value {v:?} (expected a positive number)"
                )
            }
            ConfigError::IncompleteRazorpayCredentials => write!(
                f,
                "RAZORPAY_KEY_ID and RAZORPAY_KEY_SECRET must both be set, or both left unset \
                 (values are not logged, since either may be a real secret)"
            ),
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

    // ── Successful config load ──────────────────────────────────────────

    #[test]
    fn defaults_apply_when_only_database_url_is_set() {
        let config = AppConfig::from_vars(vars(&[("DATABASE_URL", DB_URL)])).unwrap();
        assert_eq!(config.server.bind_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(
            config.server.log_filter,
            "license_server=info,tower_http=info"
        );
        assert_eq!(config.database.url.expose_secret(), DB_URL);
        assert_eq!(config.database.max_connections, 5);
    }

    #[test]
    fn explicit_host_and_port_are_honored() {
        let config = AppConfig::from_vars(vars(&[
            ("HOST", "127.0.0.1"),
            ("PORT", "9090"),
            ("DATABASE_URL", DB_URL),
        ]))
        .unwrap();
        assert_eq!(config.server.bind_addr, "127.0.0.1:9090".parse().unwrap());
    }

    #[test]
    fn explicit_rust_log_is_honored() {
        let config =
            AppConfig::from_vars(vars(&[("RUST_LOG", "debug"), ("DATABASE_URL", DB_URL)])).unwrap();
        assert_eq!(config.server.log_filter, "debug");
    }

    #[test]
    fn explicit_database_max_connections_is_honored() {
        let config = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("DATABASE_MAX_CONNECTIONS", "20"),
        ]))
        .unwrap();
        assert_eq!(config.database.max_connections, 20);
    }

    #[test]
    fn a_postgresql_scheme_url_is_also_accepted() {
        const URL: &str = "postgresql://user:pass@localhost:5432/license_server";
        let config = AppConfig::from_vars(vars(&[("DATABASE_URL", URL)])).unwrap();
        assert_eq!(config.database.url.expose_secret(), URL);
    }

    #[test]
    fn razorpay_settings_default_to_none_when_unset() {
        let config = AppConfig::from_vars(vars(&[("DATABASE_URL", DB_URL)])).unwrap();
        assert_eq!(config.payment.razorpay_key_id, None);
        assert_eq!(config.payment.razorpay_key_secret, None);
        assert_eq!(config.payment.razorpay_webhook_secret, None);
        assert_eq!(config.payment.razorpay_monthly_plan_id, None);
        assert_eq!(config.payment.razorpay_yearly_plan_id, None);
    }

    #[test]
    fn explicit_razorpay_settings_are_honored() {
        let config = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("RAZORPAY_KEY_ID", "rzp_test_key"),
            ("RAZORPAY_KEY_SECRET", "rzp_test_secret"),
            ("RAZORPAY_WEBHOOK_SECRET", "whsec_test"),
            ("RAZORPAY_MONTHLY_PLAN_ID", "plan_monthly"),
            ("RAZORPAY_YEARLY_PLAN_ID", "plan_yearly"),
        ]))
        .unwrap();
        assert_eq!(
            config.payment.razorpay_key_id.unwrap().expose_secret(),
            "rzp_test_key"
        );
        assert_eq!(
            config.payment.razorpay_key_secret.unwrap().expose_secret(),
            "rzp_test_secret"
        );
        assert_eq!(
            config
                .payment
                .razorpay_webhook_secret
                .unwrap()
                .expose_secret(),
            "whsec_test"
        );
        assert_eq!(
            config.payment.razorpay_monthly_plan_id.as_deref(),
            Some("plan_monthly")
        );
        assert_eq!(
            config.payment.razorpay_yearly_plan_id.as_deref(),
            Some("plan_yearly")
        );
    }

    // ── Missing env vars / invalid config ───────────────────────────────

    #[test]
    fn missing_database_url_is_a_config_error_not_a_default_connection_string() {
        let result = AppConfig::from_vars(vars(&[]));
        assert_eq!(result.unwrap_err(), ConfigError::MissingDatabaseUrl);
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
    fn a_database_url_with_no_recognized_scheme_is_a_config_error() {
        let result = AppConfig::from_vars(vars(&[("DATABASE_URL", "not-a-url-at-all")]));
        assert_eq!(result.unwrap_err(), ConfigError::InvalidDatabaseUrl);
    }

    #[test]
    fn an_invalid_database_url_error_never_echoes_the_offending_value() {
        // The whole point of `ConfigError::InvalidDatabaseUrl` carrying no
        // payload — even a real, sensitive password embedded in a
        // malformed URL must never end up in the error's Display text.
        const SECRET_LOOKING_VALUE: &str = "mysql://user:SUPERSECRETPASSWORD@host/db";
        let result = AppConfig::from_vars(vars(&[("DATABASE_URL", SECRET_LOOKING_VALUE)]));
        let message = result.unwrap_err().to_string();
        assert!(!message.contains("SUPERSECRETPASSWORD"));
        assert!(!message.contains(SECRET_LOOKING_VALUE));
    }

    #[test]
    fn only_razorpay_key_id_set_is_a_config_error() {
        let result = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("RAZORPAY_KEY_ID", "rzp_test_key"),
        ]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::IncompleteRazorpayCredentials
        );
    }

    #[test]
    fn only_razorpay_key_secret_set_is_a_config_error() {
        let result = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("RAZORPAY_KEY_SECRET", "rzp_test_secret"),
        ]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::IncompleteRazorpayCredentials
        );
    }

    #[test]
    fn an_incomplete_razorpay_credentials_error_never_echoes_the_offending_value() {
        let result = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("RAZORPAY_KEY_ID", "a-real-key-id-shaped-value"),
        ]));
        let message = result.unwrap_err().to_string();
        assert!(!message.contains("a-real-key-id-shaped-value"));
    }

    #[test]
    fn config_error_display_mentions_the_offending_value_for_non_secret_fields() {
        let err = ConfigError::InvalidPort("xyz".to_string());
        assert!(err.to_string().contains("xyz"));
    }

    // ── Redacted Debug ───────────────────────────────────────────────────

    #[test]
    fn secret_debug_output_never_contains_the_wrapped_value() {
        let secret = Secret::new("hunter2".to_string());
        let debug_output = format!("{secret:?}");
        assert!(!debug_output.contains("hunter2"));
        assert_eq!(debug_output, "Secret(\"***REDACTED***\")");
    }

    #[test]
    fn database_config_debug_output_never_contains_the_url() {
        let config = DatabaseConfig {
            url: Secret::new("postgres://user:realpassword@host:5432/db".to_string()),
            max_connections: 5,
        };
        let debug_output = format!("{config:?}");
        assert!(!debug_output.contains("realpassword"));
        assert!(!debug_output.contains("postgres://user"));
        assert!(debug_output.contains("***REDACTED***"));
    }

    #[test]
    fn app_config_debug_output_never_contains_any_configured_secret() {
        let config = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", "postgres://user:realpassword@host/db"),
            ("RAZORPAY_KEY_ID", "rzp_live_realkeyid"),
            ("RAZORPAY_KEY_SECRET", "reallysecretvalue"),
            ("RAZORPAY_WEBHOOK_SECRET", "reallysecretwebhook"),
        ]))
        .unwrap();

        let debug_output = format!("{config:?}");
        assert!(!debug_output.contains("realpassword"));
        assert!(!debug_output.contains("rzp_live_realkeyid"));
        assert!(!debug_output.contains("reallysecretvalue"));
        assert!(!debug_output.contains("reallysecretwebhook"));
    }

    #[test]
    fn secret_partial_eq_compares_the_wrapped_value_not_the_redacted_text() {
        assert_eq!(
            Secret::new("same".to_string()),
            Secret::new("same".to_string())
        );
        assert_ne!(Secret::new("a".to_string()), Secret::new("b".to_string()));
    }
}
