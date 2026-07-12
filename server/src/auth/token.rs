//! Secure session-token generation and hashing
//! (`PHASE4_DESIGN.md` §1.3/§6 — "random 256-bit, stored hashed (SHA-256)
//! in a sessions table... token itself never stored").

use rand::RngCore;
use sha2::{Digest, Sha256};

/// A fresh, high-entropy bearer token: 256 bits from the OS CSPRNG
/// (`rand::thread_rng`), hex-encoded (64 characters). Returned to the
/// caller exactly once, at login — never stored anywhere in this form,
/// only its hash (`hash_token`) is persisted.
pub fn generate_session_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

/// SHA-256 of a token, hex-encoded — what actually lives in
/// `sessions.token_hash`. Deterministic, so a presented bearer token can be
/// looked up by its hash without ever storing (or needing to decrypt) the
/// real value.
pub fn hash_token(token: &str) -> String {
    hex_encode(&Sha256::digest(token.as_bytes()))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_64_hex_characters_and_unique_per_call() {
        let a = generate_session_token();
        let b = generate_session_token();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two calls must not produce the same token");
    }

    #[test]
    fn hash_token_is_deterministic() {
        let token = "abc123";
        assert_eq!(hash_token(token), hash_token(token));
    }

    #[test]
    fn hash_token_differs_for_different_tokens() {
        assert_ne!(hash_token("a"), hash_token("b"));
    }

    #[test]
    fn hash_token_never_equals_the_raw_token() {
        let token = generate_session_token();
        assert_ne!(hash_token(&token), token);
    }
}
