// monthly_password.rs — HMAC-SHA512 monthly password validation.
//
// Algorithm mirrors main.js exactly:
//   1. Concatenate 8 SK fragments into the secret key
//   2. HMAC-SHA512( key, "<email_lower>|YYYY-MM" )
//   3. Base64url-encode (no padding) the digest
//   4. Keep only alphanumeric chars, uppercase, take first 32
//   5. Split into 4 groups of 8 separated by "-"

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::Local;
use hmac::{Hmac, Mac};
use sha2::Sha512;

type HmacSha512 = Hmac<Sha512>;

// Secret key fragments — same split as _SK in main.js
const SK_FRAGMENTS: [&str; 8] = [
    "9b5840c7c68b090a",
    "5d1f24ae80984360",
    "9f21dfa6fc594712",
    "64c876c32718e849",
    "712214c5e3a68cf2",
    "11b2249a0c858dab",
    "8203a42df73e37de",
    "1933d9869309aa8c",
];

fn secret_key() -> String {
    SK_FRAGMENTS.concat()
}

/// Returns the current month as "YYYY-MM".
fn current_month() -> String {
    Local::now().format("%Y-%m").to_string()
}

/// Generates the expected password for a given email and month string.
/// Returns `None` only if the HMAC digest produces fewer than 32 alphanumeric chars
/// (essentially impossible with SHA-512 output).
fn generate_password(email: &str, month: &str) -> Option<String> {
    let key     = secret_key();
    let message = format!("{}|{}", email.trim().to_lowercase(), month);

    let mut mac = HmacSha512::new_from_slice(key.as_bytes()).ok()?;
    mac.update(message.as_bytes());
    let digest = mac.finalize().into_bytes();

    // Base64url-encode without padding — matches Node.js .digest('base64url')
    let b64 = URL_SAFE_NO_PAD.encode(digest);

    // Remove non-alphanumeric, uppercase, take first 32 characters
    let alphanum: String = b64
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase();

    if alphanum.len() < 32 {
        return None;
    }

    // Format: XXXXXXXX-XXXXXXXX-XXXXXXXX-XXXXXXXX
    let raw    = &alphanum[..32];
    let groups: Vec<&str> = (0..4).map(|i| &raw[i * 8..(i + 1) * 8]).collect();
    Some(groups.join("-"))
}

/// Returns `true` if `password` matches the expected password for `email` this month.
pub fn validate_credentials(email: &str, password: &str) -> bool {
    let e = email.trim();
    let p = password.trim();
    if e.is_empty() || p.is_empty() {
        return false;
    }
    match generate_password(e, &current_month()) {
        Some(expected) => p.to_uppercase() == expected,
        None           => false,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_has_correct_format() {
        let pwd = generate_password("test@example.com", "2026-06")
            .expect("password generation should not fail");
        let parts: Vec<&str> = pwd.split('-').collect();
        assert_eq!(parts.len(), 4, "must have 4 groups");
        for part in &parts {
            assert_eq!(part.len(), 8, "each group must be 8 chars");
            assert!(part.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
        }
    }

    #[test]
    fn same_inputs_same_output() {
        let a = generate_password("user@firm.com", "2026-05");
        let b = generate_password("user@firm.com", "2026-05");
        assert_eq!(a, b, "password must be deterministic");
    }

    #[test]
    fn different_months_different_passwords() {
        let a = generate_password("user@firm.com", "2026-05");
        let b = generate_password("user@firm.com", "2026-06");
        assert_ne!(a, b, "password must change each month");
    }

    #[test]
    fn empty_inputs_rejected() {
        assert!(!validate_credentials("", "somepass"));
        assert!(!validate_credentials("user@x.com", ""));
    }

    #[test]
    fn wrong_password_rejected() {
        assert!(!validate_credentials("user@firm.com", "AAAAAAAA-BBBBBBBB-CCCCCCCC-DDDDDDDD"));
    }
}
