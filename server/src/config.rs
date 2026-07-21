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

use ipnet::IpNet;
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

/// `HOST`/`PORT`/`RUST_LOG`/`TRUSTED_PROXY_CIDRS` — nothing secret here,
/// just where and how loudly this process listens and logs, plus (Production
/// Hardening, Finding H3) which reverse proxies it's willing to trust an
/// `X-Forwarded-For` header from at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub bind_addr: SocketAddr,
    pub log_filter: String,
    /// Production Hardening, Finding H3: CIDR ranges whose direct TCP peer
    /// address `rate_limit::login_rate_limit` trusts enough to read
    /// `X-Forwarded-For` from at all — `rate_limit::resolve_client_ip` is
    /// the actual decision logic; this is just its configuration input.
    /// **Empty by default** (`TRUSTED_PROXY_CIDRS` unset) — an empty list
    /// means no peer is ever trusted, so `X-Forwarded-For` is never
    /// consulted and every existing deployment that hasn't set this
    /// variable behaves byte-for-byte as it did before this finding was
    /// fixed (this crate's own documented Caddy topology,
    /// `PHASE4_DESIGN.md` §8, needs `TRUSTED_PROXY_CIDRS` set to the
    /// Docker network's own subnet — see `server/.env.example` — to
    /// actually benefit from this fix).
    pub trusted_proxies: Vec<IpNet>,
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

/// `RECONCILIATION_INTERVAL_SECS`/`RECONCILIATION_BATCH_SIZE`/
/// `RECONCILIATION_MAX_AGE_HOURS` (Phase 4K.4) — all three previously
/// fixed constants (`reconciliation::INTERVAL`, the Razorpay payments-list
/// `count=100` query param, `service::payment_service::
/// RECONCILIATION_LOOKBACK_HOURS`). Nothing secret here. Every default
/// below matches the value each was fixed at before this phase, so an
/// operator who sets none of these three variables gets byte-for-byte the
/// same scheduling/lookback behavior as before — this is a configurability
/// addition, not a behavior change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationConfig {
    pub interval_secs: u64,
    pub batch_size: u32,
    pub max_age_hours: i64,
}

/// `RATE_LIMIT_ENTRY_TTL_SECONDS` (Production Hardening, Finding H4 — "the
/// in-memory rate limiters grow forever").
///
/// [`rate_limit::RateLimiters`](crate::rate_limit::RateLimiters) keys its
/// login/device limiters by client IP / `device_id`; every *distinct* key
/// ever seen gets its own entry in `governor`'s keyed state store, and
/// nothing removed one until this finding — an attacker cycling through
/// fresh IPs/device ids indefinitely could grow that memory without bound.
/// [`rate_limit_cleanup::spawn`](crate::rate_limit_cleanup::spawn) fixes
/// this by periodically calling `governor`'s own built-in
/// `RateLimiter::retain_recent`/`shrink_to_fit` housekeeping (every keyed
/// state store `governor` ships implements
/// `governor::state::keyed::ShrinkableKeyedStateStore` for exactly this
/// purpose) — this field is that period.
///
/// **What this does and does not control:** `retain_recent`'s own eviction
/// rule is intrinsic to each limiter's quota — a key is dropped once its
/// GCRA bucket's last-recorded state is older than the quota's own
/// per-cell replenishment weight (`governor`'s `Gcra::t()`: for a
/// `Quota::per_minute(N)` budget, `60/N` seconds — 12 seconds for the
/// login limiter's `N = 5`, 2 seconds for the device limiter's `N = 30`;
/// verified directly against `governor` 0.10.4's `gcra.rs` source, not
/// assumed). `governor` exposes no public hook to override that per-key
/// threshold with an arbitrary externally-supplied duration (`RateLimiter::
/// retain_recent()` takes no arguments and computes its own cutoff from the
/// quota; the only variant that accepts a custom age lives on the private
/// state store field, unreachable without consuming the whole limiter,
/// which isn't possible once it's shared behind the `Arc` `RateLimiters`
/// holds). So `entry_ttl_secs` configures **how often the cleanup sweep
/// runs**, not a standalone idle threshold — an idle entry becomes
/// eligible for removal after that quota-derived handful of seconds
/// (unchanged, and deliberately not reconfigurable here, since shortening
/// or lengthening it independently of the quota would change actual
/// rate-limiting behavior, which Finding H4 explicitly must not do) and is
/// guaranteed to actually be reclaimed within `entry_ttl_secs` of that. A
/// hand-rolled, fully independent last-access TTL layered on top was
/// considered and rejected: it would duplicate state `governor` already
/// tracks internally, add a new lock, and — since no key can usefully
/// outlive its own quota's replenishment window anyway — buy nothing a
/// shorter sweep interval doesn't already give for free.
///
/// Default: 900 seconds (15 minutes) — frequent enough that abandoned keys
/// don't linger long, infrequent enough that the periodic sweep (an O(n)
/// scan of currently-tracked keys, `n` bounded by recent unique
/// callers/devices) stays negligible against normal traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitConfig {
    pub entry_ttl_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub payment: PaymentConfig,
    pub reconciliation: ReconciliationConfig,
    pub rate_limit: RateLimitConfig,
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

        // Production Hardening, Finding H3: comma-separated CIDR list
        // (e.g. "127.0.0.1/32,172.16.0.0/12"). Unset or empty means no
        // trusted proxies at all — see `ServerConfig::trusted_proxies`'s
        // own doc comment for why that's the safe, behavior-preserving
        // default for every deployment that doesn't explicitly opt in.
        let trusted_proxies = match get("TRUSTED_PROXY_CIDRS") {
            Some(v) if !v.trim().is_empty() => v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| {
                    s.parse::<IpNet>()
                        .map_err(|_| ConfigError::InvalidTrustedProxyCidr(s.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => Vec::new(),
        };

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

        let reconciliation_interval_secs = match get("RECONCILIATION_INTERVAL_SECS") {
            Some(v) => v
                .parse::<u64>()
                .map_err(|_| ConfigError::InvalidReconciliationInterval(v))?,
            None => 15 * 60,
        };
        let reconciliation_batch_size = match get("RECONCILIATION_BATCH_SIZE") {
            Some(v) => v
                .parse::<u32>()
                .map_err(|_| ConfigError::InvalidReconciliationBatchSize(v))?,
            None => 100,
        };
        let reconciliation_max_age_hours = match get("RECONCILIATION_MAX_AGE_HOURS") {
            Some(v) => v
                .parse::<i64>()
                .map_err(|_| ConfigError::InvalidReconciliationMaxAge(v))?,
            None => 2,
        };

        // Production Hardening, Finding H4: see `RateLimitConfig`'s own doc
        // comment for what this actually controls (the cleanup sweep
        // interval, not a standalone per-key idle threshold) and why.
        let rate_limit_entry_ttl_secs = match get("RATE_LIMIT_ENTRY_TTL_SECONDS") {
            Some(v) => v
                .parse::<u64>()
                .map_err(|_| ConfigError::InvalidRateLimitEntryTtl(v))?,
            None => 900,
        };

        Ok(AppConfig {
            server: ServerConfig {
                bind_addr: SocketAddr::new(host, port),
                log_filter,
                trusted_proxies,
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
            reconciliation: ReconciliationConfig {
                interval_secs: reconciliation_interval_secs,
                batch_size: reconciliation_batch_size,
                max_age_hours: reconciliation_max_age_hours,
            },
            rate_limit: RateLimitConfig {
                entry_ttl_secs: rate_limit_entry_ttl_secs,
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    InvalidHost(String),
    InvalidPort(String),
    /// Production Hardening, Finding H3: `TRUSTED_PROXY_CIDRS` was set but
    /// one of its comma-separated entries isn't a valid CIDR (e.g.
    /// `192.168.1.1` with no `/prefix`, or a malformed address). Carries
    /// the offending entry — unlike the secret-adjacent variants below,
    /// nothing here can ever be a credential, so echoing it back is safe
    /// and helps an operator find their typo.
    InvalidTrustedProxyCidr(String),
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
    /// (Phase 4K.4) `RECONCILIATION_INTERVAL_SECS` set but not a valid
    /// non-negative integer.
    InvalidReconciliationInterval(String),
    /// (Phase 4K.4) `RECONCILIATION_BATCH_SIZE` set but not a valid
    /// non-negative integer.
    InvalidReconciliationBatchSize(String),
    /// (Phase 4K.4) `RECONCILIATION_MAX_AGE_HOURS` set but not a valid
    /// integer.
    InvalidReconciliationMaxAge(String),
    /// Production Hardening, Finding H4: `RATE_LIMIT_ENTRY_TTL_SECONDS` set
    /// but not a valid non-negative integer.
    InvalidRateLimitEntryTtl(String),
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
            ConfigError::InvalidTrustedProxyCidr(v) => write!(
                f,
                "invalid TRUSTED_PROXY_CIDRS entry {v:?} (expected CIDR notation, e.g. 127.0.0.1/32)"
            ),
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
            ConfigError::InvalidReconciliationInterval(v) => write!(
                f,
                "invalid RECONCILIATION_INTERVAL_SECS value {v:?} (expected a non-negative number of seconds)"
            ),
            ConfigError::InvalidReconciliationBatchSize(v) => write!(
                f,
                "invalid RECONCILIATION_BATCH_SIZE value {v:?} (expected a positive number)"
            ),
            ConfigError::InvalidReconciliationMaxAge(v) => write!(
                f,
                "invalid RECONCILIATION_MAX_AGE_HOURS value {v:?} (expected a number of hours)"
            ),
            ConfigError::InvalidRateLimitEntryTtl(v) => write!(
                f,
                "invalid RATE_LIMIT_ENTRY_TTL_SECONDS value {v:?} (expected a non-negative number of seconds)"
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

    // ── Trusted proxies (Production Hardening, Finding H3) ───────────────

    #[test]
    fn trusted_proxies_defaults_to_empty_when_unset() {
        // The behavior-preserving default: no `TRUSTED_PROXY_CIDRS` at all
        // means no peer is ever trusted, so every existing deployment that
        // hasn't set this variable is unaffected by this finding's fix.
        let config = AppConfig::from_vars(vars(&[("DATABASE_URL", DB_URL)])).unwrap();
        assert!(config.server.trusted_proxies.is_empty());
    }

    #[test]
    fn trusted_proxies_defaults_to_empty_when_set_to_an_empty_string() {
        let config = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("TRUSTED_PROXY_CIDRS", ""),
        ]))
        .unwrap();
        assert!(config.server.trusted_proxies.is_empty());
    }

    #[test]
    fn a_single_trusted_proxy_cidr_is_parsed() {
        let config = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("TRUSTED_PROXY_CIDRS", "127.0.0.1/32"),
        ]))
        .unwrap();
        assert_eq!(config.server.trusted_proxies.len(), 1);
        assert!(config.server.trusted_proxies[0].contains(&"127.0.0.1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn multiple_comma_separated_trusted_proxy_cidrs_are_all_parsed() {
        let config = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            (
                "TRUSTED_PROXY_CIDRS",
                "127.0.0.1/32,172.16.0.0/12,10.0.0.0/8",
            ),
        ]))
        .unwrap();
        assert_eq!(config.server.trusted_proxies.len(), 3);
    }

    #[test]
    fn whitespace_around_trusted_proxy_cidrs_is_tolerated() {
        let config = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("TRUSTED_PROXY_CIDRS", " 127.0.0.1/32 , 10.0.0.0/8 "),
        ]))
        .unwrap();
        assert_eq!(config.server.trusted_proxies.len(), 2);
    }

    #[test]
    fn an_ipv6_trusted_proxy_cidr_is_parsed() {
        let config = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("TRUSTED_PROXY_CIDRS", "::1/128"),
        ]))
        .unwrap();
        assert_eq!(config.server.trusted_proxies.len(), 1);
        assert!(config.server.trusted_proxies[0].contains(&"::1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn a_malformed_trusted_proxy_cidr_is_a_config_error_not_a_silent_skip() {
        let result = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("TRUSTED_PROXY_CIDRS", "not-a-cidr"),
        ]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::InvalidTrustedProxyCidr("not-a-cidr".to_string())
        );
    }

    #[test]
    fn a_cidr_missing_its_prefix_length_is_a_config_error() {
        // A bare IP with no `/prefix` is exactly the kind of mistake this
        // must reject rather than silently treating as a /32 or /128 —
        // being explicit here is what keeps an operator's intent visible.
        let result = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("TRUSTED_PROXY_CIDRS", "192.168.1.1"),
        ]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::InvalidTrustedProxyCidr("192.168.1.1".to_string())
        );
    }

    #[test]
    fn one_valid_and_one_malformed_trusted_proxy_cidr_is_still_a_config_error() {
        let result = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("TRUSTED_PROXY_CIDRS", "127.0.0.1/32,garbage"),
        ]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::InvalidTrustedProxyCidr("garbage".to_string())
        );
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

    // ── Reconciliation config (Phase 4K.4) ──────────────────────────────

    #[test]
    fn reconciliation_defaults_match_the_values_every_call_site_used_to_hardcode() {
        let config = AppConfig::from_vars(vars(&[("DATABASE_URL", DB_URL)])).unwrap();
        assert_eq!(config.reconciliation.interval_secs, 15 * 60);
        assert_eq!(config.reconciliation.batch_size, 100);
        assert_eq!(config.reconciliation.max_age_hours, 2);
    }

    #[test]
    fn explicit_reconciliation_settings_are_honored() {
        let config = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("RECONCILIATION_INTERVAL_SECS", "60"),
            ("RECONCILIATION_BATCH_SIZE", "25"),
            ("RECONCILIATION_MAX_AGE_HOURS", "6"),
        ]))
        .unwrap();
        assert_eq!(config.reconciliation.interval_secs, 60);
        assert_eq!(config.reconciliation.batch_size, 25);
        assert_eq!(config.reconciliation.max_age_hours, 6);
    }

    #[test]
    fn invalid_reconciliation_interval_is_a_config_error() {
        let result = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("RECONCILIATION_INTERVAL_SECS", "not-a-number"),
        ]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::InvalidReconciliationInterval("not-a-number".to_string())
        );
    }

    #[test]
    fn invalid_reconciliation_batch_size_is_a_config_error() {
        let result = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("RECONCILIATION_BATCH_SIZE", "not-a-number"),
        ]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::InvalidReconciliationBatchSize("not-a-number".to_string())
        );
    }

    #[test]
    fn invalid_reconciliation_max_age_is_a_config_error() {
        let result = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("RECONCILIATION_MAX_AGE_HOURS", "not-a-number"),
        ]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::InvalidReconciliationMaxAge("not-a-number".to_string())
        );
    }

    // ── Rate limit entry TTL / cleanup sweep interval (Production
    // Hardening, Finding H4) ─────────────────────────────────────────────

    #[test]
    fn rate_limit_entry_ttl_defaults_to_900_seconds_when_unset() {
        let config = AppConfig::from_vars(vars(&[("DATABASE_URL", DB_URL)])).unwrap();
        assert_eq!(config.rate_limit.entry_ttl_secs, 900);
    }

    #[test]
    fn explicit_rate_limit_entry_ttl_is_honored() {
        let config = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("RATE_LIMIT_ENTRY_TTL_SECONDS", "60"),
        ]))
        .unwrap();
        assert_eq!(config.rate_limit.entry_ttl_secs, 60);
    }

    #[test]
    fn invalid_rate_limit_entry_ttl_is_a_config_error_not_a_silent_fallback() {
        let result = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("RATE_LIMIT_ENTRY_TTL_SECONDS", "not-a-number"),
        ]));
        assert_eq!(
            result.unwrap_err(),
            ConfigError::InvalidRateLimitEntryTtl("not-a-number".to_string())
        );
    }

    #[test]
    fn a_zero_rate_limit_entry_ttl_is_accepted_as_a_valid_non_negative_integer() {
        // Not a sensible production value (it'd sweep on every tick), but
        // `u64` parsing has no reason to special-case zero as invalid —
        // an operator who sets this is responsible for the consequences,
        // same as any other tuning knob here.
        let config = AppConfig::from_vars(vars(&[
            ("DATABASE_URL", DB_URL),
            ("RATE_LIMIT_ENTRY_TTL_SECONDS", "0"),
        ]))
        .unwrap();
        assert_eq!(config.rate_limit.entry_ttl_secs, 0);
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
