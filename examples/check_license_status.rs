// examples/check_license_status.rs — read-only diagnostic: prints the
// current local license record and computed LicenseStatus for a given
// bsp_data.db, without modifying anything (safe to run alongside a running
// instance of the app — read-only, no write transaction).
//
// Run: cargo run --example check_license_status -- [path to bsp_data.db]

use bank_statement_processor::license;

fn main() {
    let db_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/debug/bsp_data.db".to_string());

    let conn = bank_statement_processor::db::open(&db_path).expect("db::open failed");
    let status = license::check_status(&conn, &license::OfflineClient);
    let record = license::storage::load_local_license(&conn).ok().flatten();

    println!("status = {status:?}");
    println!("is_licensed = {}", status.is_licensed());
    match &record {
        Some(r) => {
            println!("record:");
            println!("  license_key        = {:?}", r.license_key);
            println!("  license_id         = {:?}", r.license_id);
            println!("  status             = {:?}", r.status);
            println!("  subscription_type  = {:?}", r.subscription_type);
            println!("  expires_at         = {:?}", r.expires_at);
            println!("  last_validated_at  = {:?}", r.last_validated_at);
            println!("  grace_period_days  = {}", r.grace_period_days);
        }
        None => println!("record: none (never activated)"),
    }
    println!("describe() = {}", license::describe(status, record.as_ref()));
}
