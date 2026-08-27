// examples/reset_license.rs — reset this device's local license activation
// back to "not activated", without touching clients/transactions/settings,
// so the first-run License Activation screen can be exercised again.
//
// Run (app must be closed first — SQLite file lock):
//   cargo run --example reset_license -- [path to bsp_data.db]
// With no argument, defaults to "bsp_data.db" next to this example's own
// target/debug directory — pass the real path explicitly if your db lives
// elsewhere (it's always next to the running .exe, see main.rs's db_path).

use bank_statement_processor::{db, license};

fn main() {
    let db_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/debug/bsp_data.db".to_string());

    let path = std::path::Path::new(&db_path);
    if !path.exists() {
        eprintln!("No database found at {db_path} — nothing to reset.");
        std::process::exit(1);
    }

    let conn = db::open(path).expect("db::open failed");
    let before = license::storage::load_local_license(&conn).ok().flatten();
    match &before {
        Some(r) => println!(
            "Current activation: status={:?} license_key={:?}",
            r.status, r.license_key
        ),
        None => println!("No local license record found — already unactivated."),
    }

    license::clear_local_activation(&conn).expect("clear_local_activation failed");
    println!("Reset {db_path}'s local_license row to not_activated.");
    println!("Next launch will show the License Activation screen again.");
}
