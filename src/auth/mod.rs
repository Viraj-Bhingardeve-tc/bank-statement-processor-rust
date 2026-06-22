// auth/mod.rs — Authentication module.
//
// SCOPE NOTE (audit remediation, see PRODUCTION_READINESS_AUDIT_2026-06-22.md
// Phase 7): this module is a monthly licensing / anti-piracy gate, not an
// access-control boundary. The password is a deterministic function of a
// secret compiled into the binary (`monthly_password::SK_FRAGMENTS`), so
// anyone with the binary or source can compute a valid password offline —
// this is an inherent limitation of any client-side-only shared-secret
// check, not a bug to "fix" here. Real protection for client banking data
// in this app comes from the data layer (encryption at rest), not from this
// gate. Do not treat a successful login here as proof of user identity or
// as sufficient protection for sensitive data.

mod monthly_password;

/// Validates email/password against the HMAC-SHA512 monthly password,
/// matching Electron main.js's `validate_credentials` exactly.
///
/// See the module-level note above: this is a licensing check, not an
/// identity/access-control check.
pub fn validate_credentials(email: &str, password: &str) -> bool {
    monthly_password::validate_credentials(email, password)
}
