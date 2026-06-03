// auth/mod.rs — Authentication module.

mod monthly_password;

pub use monthly_password::validate_credentials;
