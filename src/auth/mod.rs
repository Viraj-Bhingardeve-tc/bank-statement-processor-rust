// auth/mod.rs — Authentication module.

mod monthly_password;

// DEV_BYPASS: accept any non-empty credentials for UI testing
pub fn validate_credentials(email: &str, password: &str) -> bool {
    let _ = password;  // suppress unused warning
    !email.trim().is_empty()  // any email accepts
}
