// fingerprint.rs — device id generation and machine fingerprinting.
//
// See LICENSE_SYSTEM_DESIGN.md §5 and LICENSE_SECURITY_REVIEW.md §5 for the
// full design rationale and honest strength/weakness discussion. Summary:
// device_id is a random identity generated once per installation;
// machine_fingerprint is a secondary, deliberately lightweight consistency
// signal (env-var-derived, no new OS-level dependency), not the primary
// identity check.

use rand::RngExt;
use sha2::{Digest, Sha256};

/// Generates a random RFC-4122-shaped v4 UUID string, e.g.
/// "a1b2c3d4-e5f6-4a5b-8c9d-0e1f2a3b4c5d". Implemented directly on the
/// `rand` crate (already a dependency, same `rand::rng().fill(...)` pattern
/// `db/encryption.rs` uses for key generation) rather than adding a `uuid`
/// crate dependency for a single call site.
pub fn generate_device_id() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    // Set version (4) and variant (RFC 4122) bits per the UUID v4 spec.
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// The raw, human-readable signals hashed into the fingerprint — returned
/// separately from the hash itself so callers can persist them (see
/// `device_info.fingerprint_inputs`) for support diagnostics ("why did my
/// fingerprint change?") without needing to reverse the hash.
pub struct FingerprintInputs {
    pub computer_name: String,
    pub user_name: String,
    pub processor_identifier: String,
}

impl FingerprintInputs {
    pub fn collect() -> Self {
        FingerprintInputs {
            computer_name: env_or_unknown("COMPUTERNAME"),
            user_name: env_or_unknown("USERNAME"),
            processor_identifier: env_or_unknown("PROCESSOR_IDENTIFIER"),
        }
    }

    pub fn to_json(&self) -> String {
        format!(
            "{{\"computer_name\":{:?},\"user_name\":{:?},\"processor_identifier\":{:?}}}",
            self.computer_name, self.user_name, self.processor_identifier
        )
    }

    /// SHA-256 hex digest of the three inputs, `|`-joined. Deterministic for
    /// a given machine/user/session environment — see
    /// LICENSE_SECURITY_REVIEW.md §5 for why these three specifically, and
    /// what legitimate drift looks like.
    pub fn hash(&self) -> String {
        let joined = format!(
            "{}|{}|{}",
            self.computer_name, self.user_name, self.processor_identifier
        );
        let digest = Sha256::digest(joined.as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }
}

fn env_or_unknown(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_device_id_produces_the_expected_shape() {
        let id = generate_device_id();
        assert_eq!(
            id.len(),
            36,
            "expected 8-4-4-4-12 hyphenated form, got: {id}"
        );
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        // Version nibble must be '4' (v4 UUID).
        assert!(
            parts[2].starts_with('4'),
            "expected version 4, got: {}",
            parts[2]
        );
    }

    #[test]
    fn generate_device_id_is_not_constant() {
        let a = generate_device_id();
        let b = generate_device_id();
        assert_ne!(a, b, "two calls must not produce the same id");
    }

    #[test]
    fn fingerprint_hash_is_deterministic_for_the_same_inputs() {
        let a = FingerprintInputs {
            computer_name: "DESKTOP-AB12CD".to_string(),
            user_name: "alice".to_string(),
            processor_identifier: "Intel64 Family 6".to_string(),
        };
        let b = FingerprintInputs {
            computer_name: "DESKTOP-AB12CD".to_string(),
            user_name: "alice".to_string(),
            processor_identifier: "Intel64 Family 6".to_string(),
        };
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn fingerprint_hash_differs_when_any_input_differs() {
        let base = FingerprintInputs {
            computer_name: "DESKTOP-AB12CD".to_string(),
            user_name: "alice".to_string(),
            processor_identifier: "Intel64 Family 6".to_string(),
        };
        let renamed = FingerprintInputs {
            computer_name: "DESKTOP-XY99ZZ".to_string(),
            user_name: "alice".to_string(),
            processor_identifier: "Intel64 Family 6".to_string(),
        };
        assert_ne!(base.hash(), renamed.hash());
    }

    #[test]
    fn fingerprint_hash_is_a_64_char_hex_string() {
        let inputs = FingerprintInputs::collect();
        let h = inputs.hash();
        assert_eq!(h.len(), 64, "SHA-256 hex digest must be 64 chars");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
