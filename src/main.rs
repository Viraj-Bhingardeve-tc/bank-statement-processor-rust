// main.rs — Entry point for the Bank Statement Processor (Rust + Slint).
//
// Boot sequence:
//   1. Initialise logger
//   2. Open (or create) the SQLite database in the user's app-data folder
//   3. Create the Slint AppWindow
//   4. Wire callbacks: do-login, do-load-file, do-batch-folder
//   5. Run the Slint event loop (blocks until the window is closed)

// In release mode, suppress the Windows console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auth;
mod db;
mod parser;
mod ui;

use std::sync::{Arc, Mutex};

use anyhow::Result;

// Slint types are only available when the "slint-ui" feature is enabled.
#[cfg(feature = "slint-ui")]
use slint::SharedString;

// Include Slint-generated types (AppWindow, etc.) produced by build.rs
#[cfg(feature = "slint-ui")]
slint::include_modules!();

// ── Login attempt state ───────────────────────────────────────────────────────

struct LoginState {
    attempts: u32,
    max:      u32,
}

impl LoginState {
    fn new() -> Self {
        Self { attempts: 0, max: 3 }
    }

    fn record_failure(&mut self) {
        self.attempts += 1;
    }

    fn remaining(&self) -> u32 {
        self.max.saturating_sub(self.attempts)
    }

    fn exhausted(&self) -> bool {
        self.attempts >= self.max
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialise env_logger.  Set RUST_LOG=debug for verbose output.
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .init();

    log::info!("Bank Statement Processor starting…");

    // Open database (create if absent) in the executable's directory.
    let db_path = {
        let mut p = std::env::current_exe()
            .unwrap_or_else(|_| std::path::PathBuf::from("."));
        p.pop();
        p.push("bsp_data.db");
        p
    };

    match db::open(&db_path) {
        Ok(_)    => log::info!("Database ready at {:?}", db_path),
        Err(err) => log::warn!("Database init failed (non-fatal): {}", err),
    }

    // ── Slint UI (only when feature is enabled) ───────────────────────────────
    #[cfg(feature = "slint-ui")]
    {
        let app = AppWindow::new()?;

        let login_state = Arc::new(Mutex::new(LoginState::new()));
        {
            let handle      = app.as_weak();
            let state_clone = login_state.clone();

            app.on_do_login(move |email: SharedString, password: SharedString| {
                let handle = match handle.upgrade() {
                    Some(h) => h,
                    None    => return,
                };

                let mut state = state_clone.lock().unwrap();

                if state.exhausted() {
                    handle.set_login_error(
                        "Too many failed attempts. Please restart the application.".into(),
                    );
                    return;
                }

                if auth::validate_credentials(&email, &password) {
                    log::info!("Login successful for {}", email);
                    handle.set_logged_in(true);
                    handle.set_login_error("".into());
                    handle.set_login_loading(false);
                } else {
                    state.record_failure();
                    let remaining = state.remaining();

                    let msg: SharedString = if remaining == 0 {
                        "Too many failed attempts. Please restart the application.".into()
                    } else {
                        format!(
                            "Invalid credentials. {} attempt{} remaining.",
                            remaining,
                            if remaining == 1 { "" } else { "s" }
                        )
                        .into()
                    };

                    log::warn!(
                        "Login failed for {} — {} attempt(s) remaining",
                        email,
                        remaining
                    );
                    handle.set_login_error(msg);
                }
            });
        }

        {
            let handle = app.as_weak();
            app.on_do_load_file(move || {
                if let Some(h) = handle.upgrade() {
                    log::info!("Load File clicked (not yet implemented — Phase 3)");
                    let _ = h;
                }
            });
        }

        {
            let handle = app.as_weak();
            app.on_do_batch_folder(move || {
                if let Some(h) = handle.upgrade() {
                    log::info!("Batch Folder clicked (not yet implemented — Phase 9)");
                    let _ = h;
                }
            });
        }

        log::info!("Slint event loop starting…");
        app.run()?;
    }

    #[cfg(not(feature = "slint-ui"))]
    {
        log::warn!("Built without slint-ui feature — no window will open.");
    }

    Ok(())
}
