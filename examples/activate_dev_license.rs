// examples/activate_dev_license.rs — activates the local bsp_data.db with
// the debug-build-only DEV_TEST_LICENSE_KEY via the real license::activate()
// function (the exact same call the app's "Activate License" button makes),
// without needing to click through the UI. Useful for quickly restoring an
// activated state after `reset_license` during repeated manual test cycles.
//
// Run (app must be closed first — SQLite file lock):
//   cargo run --example activate_dev_license -- [path to bsp_data.db]

use bank_statement_processor::license;

fn main() {
    let db_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/debug/bsp_data.db".to_string());

    let conn = bank_statement_processor::db::open(&db_path).expect("db::open failed");
    let status = license::activate(&conn, &license::OfflineClient, license::DEV_TEST_LICENSE_KEY)
        .expect("dev key activation failed");
    println!("Activated: {status:?}");

    let record = license::storage::load_local_license(&conn).ok().flatten();
    println!("describe() = {}", license::describe(status, record.as_ref()));
}
