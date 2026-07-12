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
}
