// auth/mod.rs — Authentication module.

mod monthly_password;

/// Validates email/password against the HMAC-SHA512 monthly password,
/// matching Electron main.js's `validate_credentials` exactly.
pub fn validate_credentials(email: &str, password: &str) -> bool {
    monthly_password::validate_credentials(email, password)
}
