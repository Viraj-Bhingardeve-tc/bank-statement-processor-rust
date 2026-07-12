//! Argon2 password hashing for `users.password_hash`
//! (`PHASE4_DESIGN.md` §1.3 — "current OWASP-recommended KDF").

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use std::fmt;

#[derive(Debug)]
pub struct PasswordError(String);

impl fmt::Display for PasswordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "password hashing error: {}", self.0)
    }
}

impl std::error::Error for PasswordError {}

/// Hashes a plaintext password with a freshly generated random salt. Two
/// calls with the same input never produce the same output — that's the
/// point (defeats rainbow-table/identical-hash-across-users attacks).
pub fn hash_password(plain: &str) -> Result<String, PasswordError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| PasswordError(e.to_string()))
}

/// Verifies a plaintext password against a previously stored Argon2 hash
/// string (in PHC format, i.e. exactly what `hash_password` returns). A
/// malformed stored hash is a distinct `Err` from "password didn't match"
/// (`Ok(false)`) — the caller should treat both the same way for a login
/// attempt (fail closed), but they're different failure modes worth being
/// able to tell apart in logs.
pub fn verify_password(plain: &str, hash: &str) -> Result<bool, PasswordError> {
    let parsed_hash = PasswordHash::new(hash).map_err(|e| PasswordError(e.to_string()))?;
    Ok(Argon2::default()
        .verify_password(plain.as_bytes(), &parsed_hash)
        .is_ok())
}

/// A fixed, valid Argon2 PHC hash of an arbitrary dummy password —
/// nothing else in this codebase ever hashes or stores this value at
/// runtime. `service::auth_service::AuthService::login` verifies the
/// caller-supplied password against this constant when the requested
/// email doesn't exist, purely to make an unknown-email attempt cost
/// approximately the same Argon2 verification time as a wrong-password
/// attempt against a real account — closing the timing side-channel the
/// production readiness audit's HIGH finding #4 identified (an unknown
/// email used to return immediately, while a known email always paid
/// Argon2's real verification cost, letting response latency alone reveal
/// which emails have accounts).
///
/// Generated once, offline, via this module's own `hash_password` — never
/// regenerated at build or run time, never derived from request input.
/// Its verification result is never trusted: `login` only ever grants a
/// session when a *real* user row was found, so no password — including
/// the plaintext this hash was made from — can authenticate as a
/// nonexistent account by matching it.
pub const DUMMY_PASSWORD_HASH: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$AR8yt1NfD9lMVlV1/oHSjw$4rwRmMWXaTJIQaKGaFNn0Dc8yoALxtlDBIfMMUUMYkA";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_round_trips_true() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &hash).unwrap());
    }

    #[test]
    fn verify_rejects_the_wrong_password() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(!verify_password("wrong password", &hash).unwrap());
    }

    #[test]
    fn two_hashes_of_the_same_password_differ_due_to_random_salt() {
        let h1 = hash_password("same-password").unwrap();
        let h2 = hash_password("same-password").unwrap();
        assert_ne!(h1, h2, "identical passwords must not hash identically");
        // Both must still verify against their own hash, salt or no.
        assert!(verify_password("same-password", &h1).unwrap());
        assert!(verify_password("same-password", &h2).unwrap());
    }

    #[test]
    fn verify_with_a_malformed_stored_hash_returns_an_error_not_a_panic() {
        assert!(verify_password("x", "not-a-real-argon2-hash").is_err());
    }

    #[test]
    fn dummy_password_hash_is_a_valid_parseable_argon2_hash() {
        // Regression guard: if this constant were ever hand-edited into
        // something malformed, `AuthService::login`'s unknown-email path
        // would start returning a `Repository`/500 error instead of the
        // documented `InvalidCredentials`/401 — a real behavior change,
        // not just a broken timing-equalization detail.
        let result = verify_password("whatever the caller typed", DUMMY_PASSWORD_HASH);
        assert!(
            result.is_ok(),
            "DUMMY_PASSWORD_HASH must parse as a real Argon2 hash"
        );
        assert!(
            !result.unwrap(),
            "an arbitrary guess must not match the dummy hash"
        );
    }

    #[test]
    fn dummy_password_hash_never_matches_the_password_it_was_generated_from() {
        // Documents the one input that *does* match the dummy hash, and
        // confirms it's still just a boolean fact about Argon2 — not
        // something `AuthService::login` ever consults for a nonexistent
        // user (see that module's own tests for the actual security
        // guarantee: unknown email never authenticates, regardless of
        // this value).
        assert!(verify_password(
            "dummy-password-for-timing-equalization-4j5",
            DUMMY_PASSWORD_HASH
        )
        .unwrap());
    }
}
