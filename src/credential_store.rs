// credential_store.rs — dependency-injection seam around the OS credential
// store. Production code always talks to the real platform keyring
// (Windows Credential Manager / macOS Keychain / Linux Secret Service) via
// `OsKeyring`, exactly as before. Test builds transparently get an
// in-memory backend instead (see `test_support`), since a headless CI
// runner (e.g. GitHub Actions' ubuntu-latest) has no D-Bus session or
// Secret Service provider for the `keyring` crate to talk to.
//
// `db::encryption` and `settings` both go through `store()` rather than
// calling `keyring::Entry::new` directly, so this is the single point where
// production vs. test backend selection happens.

use std::fmt;

#[derive(Debug)]
pub enum CredentialError {
    NoEntry,
    // Only ever constructed by `OsKeyring` (see below), which `--all-features`
    // lint/test runs never instantiate — the `test-keyring-mock` feature that
    // enables is exactly the thing that compiles `OsKeyring` out. Genuinely
    // live in every real (non-`--all-features`) build.
    #[allow(dead_code)]
    Other(String),
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredentialError::NoEntry => write!(f, "no credential entry found"),
            CredentialError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for CredentialError {}

pub trait CredentialStore: Send + Sync {
    fn get_password(&self, service: &str, username: &str) -> Result<String, CredentialError>;
    fn set_password(
        &self,
        service: &str,
        username: &str,
        password: &str,
    ) -> Result<(), CredentialError>;
    fn delete_credential(&self, service: &str, username: &str) -> Result<(), CredentialError>;
}

/// The real OS credential store, via the `keyring` crate. Used in every
/// non-test build — public behavior is byte-for-byte what this crate did
/// before this module existed.
///
/// `cargo test`/`cargo clippy --all-features` (what CI runs) always enable
/// `test-keyring-mock`, so this type is never constructed under that exact
/// invocation — it's still the real, live production path under the
/// default feature set, so the lint below is a false positive, not
/// evidence this is actually unused.
#[allow(dead_code)]
pub struct OsKeyring;

impl CredentialStore for OsKeyring {
    fn get_password(&self, service: &str, username: &str) -> Result<String, CredentialError> {
        let entry = keyring::Entry::new(service, username)
            .map_err(|e| CredentialError::Other(e.to_string()))?;
        entry.get_password().map_err(|e| match e {
            keyring::Error::NoEntry => CredentialError::NoEntry,
            other => CredentialError::Other(other.to_string()),
        })
    }

    fn set_password(
        &self,
        service: &str,
        username: &str,
        password: &str,
    ) -> Result<(), CredentialError> {
        let entry = keyring::Entry::new(service, username)
            .map_err(|e| CredentialError::Other(e.to_string()))?;
        entry
            .set_password(password)
            .map_err(|e| CredentialError::Other(e.to_string()))
    }

    fn delete_credential(&self, service: &str, username: &str) -> Result<(), CredentialError> {
        let entry = keyring::Entry::new(service, username)
            .map_err(|e| CredentialError::Other(e.to_string()))?;
        entry.delete_credential().map_err(|e| match e {
            keyring::Error::NoEntry => CredentialError::NoEntry,
            other => CredentialError::Other(other.to_string()),
        })
    }
}

/// Returns the active credential-store backend for this build: the real OS
/// keyring in production, an isolated in-memory store in tests.
///
/// `cfg(test)` alone only covers the lib's own unit tests — integration
/// tests under `tests/*.rs` link this crate as an ordinary, non-test
/// dependency, so that copy of the lib is compiled *without* `cfg(test)`
/// even while `cargo test` is running. The `test-keyring-mock` feature
/// (enabled automatically by `cargo test --all-features`, which is what
/// this workspace's CI runs) covers that copy too, so no test binary ever
/// touches the real OS keyring.
pub fn store() -> &'static dyn CredentialStore {
    #[cfg(not(any(test, feature = "test-keyring-mock")))]
    {
        static STORE: OsKeyring = OsKeyring;
        &STORE
    }
    #[cfg(any(test, feature = "test-keyring-mock"))]
    {
        test_support::store()
    }
}

#[cfg(any(test, feature = "test-keyring-mock"))]
pub mod test_support {
    use super::{CredentialError, CredentialStore};
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    /// Deterministic, process-local, no-persistence credential backend used
    /// for the whole test suite. Requires no desktop session, D-Bus, Secret
    /// Service, GNOME Keyring, or Windows Credential Manager, so it behaves
    /// identically on Windows, Linux, and headless CI runners.
    #[derive(Default)]
    pub struct InMemoryCredentialStore {
        entries: Mutex<HashMap<(String, String), String>>,
    }

    impl CredentialStore for InMemoryCredentialStore {
        fn get_password(&self, service: &str, username: &str) -> Result<String, CredentialError> {
            let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            entries
                .get(&(service.to_string(), username.to_string()))
                .cloned()
                .ok_or(CredentialError::NoEntry)
        }

        fn set_password(
            &self,
            service: &str,
            username: &str,
            password: &str,
        ) -> Result<(), CredentialError> {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            entries.insert(
                (service.to_string(), username.to_string()),
                password.to_string(),
            );
            Ok(())
        }

        fn delete_credential(&self, service: &str, username: &str) -> Result<(), CredentialError> {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            entries
                .remove(&(service.to_string(), username.to_string()))
                .map(|_| ())
                .ok_or(CredentialError::NoEntry)
        }
    }

    pub fn store() -> &'static dyn CredentialStore {
        static STORE: OnceLock<InMemoryCredentialStore> = OnceLock::new();
        STORE.get_or_init(InMemoryCredentialStore::default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_store_round_trips_a_password() {
        let store = test_support::InMemoryCredentialStore::default();
        assert!(matches!(
            store.get_password("svc", "user"),
            Err(CredentialError::NoEntry)
        ));
        store.set_password("svc", "user", "secret").unwrap();
        assert_eq!(store.get_password("svc", "user").unwrap(), "secret");
        store.delete_credential("svc", "user").unwrap();
        assert!(matches!(
            store.get_password("svc", "user"),
            Err(CredentialError::NoEntry)
        ));
    }

    #[test]
    fn in_memory_store_keeps_different_service_username_pairs_isolated() {
        let store = test_support::InMemoryCredentialStore::default();
        store.set_password("svc-a", "user", "a-secret").unwrap();
        store.set_password("svc-b", "user", "b-secret").unwrap();
        assert_eq!(store.get_password("svc-a", "user").unwrap(), "a-secret");
        assert_eq!(store.get_password("svc-b", "user").unwrap(), "b-secret");
    }

    #[test]
    fn deleting_a_missing_entry_returns_no_entry() {
        let store = test_support::InMemoryCredentialStore::default();
        assert!(matches!(
            store.delete_credential("svc", "user"),
            Err(CredentialError::NoEntry)
        ));
    }
}
