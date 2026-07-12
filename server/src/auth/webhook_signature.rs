//! Razorpay webhook signature verification (`PHASE4_DESIGN.md` §4 step 1).
//!
//! Razorpay signs each webhook `X-Razorpay-Signature: <hex-hmac-sha256>`,
//! computed over the *raw* request body using a shared secret configured
//! in the Razorpay dashboard. This is the *only* thing that authenticates
//! a webhook call — there is no bearer token on that endpoint, by design
//! (`PHASE4_DESIGN.md` §5) — so this function is the entire trust
//! boundary for every webhook-triggered write.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Recomputes the HMAC-SHA256 of `raw_body` using `secret` and compares it
/// against the hex-encoded `signature_header` value in constant time
/// (`Mac::verify_slice`, not a `==` string compare, so response timing
/// can't leak how many leading bytes matched). Returns `false` for a
/// malformed (non-hex) header too, not just a genuine mismatch — a
/// caller doesn't need to distinguish those.
pub fn verify_webhook_signature(secret: &str, raw_body: &[u8], signature_header: &str) -> bool {
    let Ok(expected_bytes) = hex_decode(signature_header) else {
        return false;
    };

    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        // HMAC accepts any key length, so this only fails on an
        // allocation-level problem — fail closed regardless.
        return false;
    };
    mac.update(raw_body);

    mac.verify_slice(&expected_bytes).is_ok()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, ()> {
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn a_correctly_signed_body_verifies() {
        let secret = "whsec_test";
        let body = br#"{"event":"payment.captured"}"#;
        let signature = sign(secret, body);

        assert!(verify_webhook_signature(secret, body, &signature));
    }

    #[test]
    fn a_tampered_body_fails_verification() {
        let secret = "whsec_test";
        let body = br#"{"event":"payment.captured"}"#;
        let signature = sign(secret, body);

        let tampered = br#"{"event":"payment.failed"}"#;
        assert!(!verify_webhook_signature(secret, tampered, &signature));
    }

    #[test]
    fn the_wrong_secret_fails_verification() {
        let body = br#"{"event":"payment.captured"}"#;
        let signature = sign("whsec_real", body);

        assert!(!verify_webhook_signature("whsec_wrong", body, &signature));
    }

    #[test]
    fn a_non_hex_signature_header_fails_closed_not_panics() {
        let body = b"anything";
        assert!(!verify_webhook_signature(
            "secret",
            body,
            "not-hex-at-all!!"
        ));
    }

    #[test]
    fn an_empty_signature_header_fails_closed() {
        assert!(!verify_webhook_signature("secret", b"body", ""));
    }
}
