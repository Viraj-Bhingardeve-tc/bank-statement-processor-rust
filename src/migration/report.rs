//! report.rs — Structured migration report: per-entity counts, warnings,
//! errors, and human-readable rendering for the UI / a saved report file.

use std::fmt::Write as _;

/// Outcome for a single entity type (clients, rules, ledgers, ...).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EntityReport {
    pub name: String,
    /// How many records of this type were present in the legacy export.
    pub found: usize,
    /// How many were newly written to the database.
    pub imported: usize,
    /// How many were skipped because an equivalent record already existed
    /// (duplicate-safe re-run, not an error).
    pub skipped_duplicate: usize,
    /// How many failed to import (each failure is also appended to the
    /// parent report's `warnings`, with enough detail to act on).
    pub failed: usize,
}

impl EntityReport {
    pub fn new(name: &str) -> Self {
        EntityReport {
            name: name.to_string(),
            ..Default::default()
        }
    }
}

/// Full report for one migration run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MigrationReport {
    pub started_at: String,
    pub finished_at: String,
    pub source_path: String,
    /// Path of the pre-migration database backup, if one was taken.
    pub backup_path: Option<String>,
    pub entities: Vec<EntityReport>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    /// True only if the migration committed successfully (no rollback).
    pub success: bool,
    /// True if a rollback to the pre-migration backup was performed.
    pub rolled_back: bool,
}

impl MigrationReport {
    pub fn new(source_path: &str) -> Self {
        MigrationReport {
            started_at: now_iso(),
            finished_at: String::new(),
            source_path: source_path.to_string(),
            backup_path: None,
            entities: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
            success: false,
            rolled_back: false,
        }
    }

    pub fn entity_mut(&mut self, name: &str) -> &mut EntityReport {
        if let Some(idx) = self.entities.iter().position(|e| e.name == name) {
            &mut self.entities[idx]
        } else {
            self.entities.push(EntityReport::new(name));
            self.entities.last_mut().unwrap()
        }
    }

    pub fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }

    pub fn total_imported(&self) -> usize {
        self.entities.iter().map(|e| e.imported).sum()
    }

    pub fn total_found(&self) -> usize {
        self.entities.iter().map(|e| e.found).sum()
    }

    pub fn finish(&mut self, success: bool, rolled_back: bool) {
        self.finished_at = now_iso();
        self.success = success;
        self.rolled_back = rolled_back;
    }

    /// Human-readable summary for a toast / status line.
    pub fn one_line_summary(&self) -> String {
        if self.rolled_back {
            return format!(
                "Migration failed and was rolled back \u{2014} database restored to its pre-migration state. {} error(s).",
                self.errors.len()
            );
        }
        if !self.success {
            return format!(
                "Migration did not complete. {} error(s).",
                self.errors.len()
            );
        }
        format!(
            "Migration complete: {} record(s) imported across {} categor{} ({} warning(s)).",
            self.total_imported(),
            self.entities.len(),
            if self.entities.len() == 1 { "y" } else { "ies" },
            self.warnings.len(),
        )
    }

    /// Full Markdown report, suitable for saving to disk or showing in a
    /// scrollable UI panel.
    pub fn to_markdown(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "# Migration Report");
        let _ = writeln!(s);
        let _ = writeln!(s, "- **Source:** `{}`", self.source_path);
        let _ = writeln!(s, "- **Started:** {}", self.started_at);
        let _ = writeln!(s, "- **Finished:** {}", self.finished_at);
        if let Some(b) = &self.backup_path {
            let _ = writeln!(s, "- **Pre-migration backup:** `{}`", b);
        }
        let _ = writeln!(
            s,
            "- **Result:** {}",
            if self.rolled_back {
                "FAILED \u{2014} rolled back to pre-migration backup"
            } else if self.success {
                "SUCCESS"
            } else {
                "FAILED"
            }
        );
        let _ = writeln!(s);
        let _ = writeln!(s, "## Entities");
        let _ = writeln!(s);
        let _ = writeln!(
            s,
            "| Entity | Found | Imported | Skipped (duplicate) | Failed |"
        );
        let _ = writeln!(s, "|---|---|---|---|---|");
        for e in &self.entities {
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {} |",
                e.name, e.found, e.imported, e.skipped_duplicate, e.failed
            );
        }
        if !self.warnings.is_empty() {
            let _ = writeln!(s);
            let _ = writeln!(s, "## Warnings ({})", self.warnings.len());
            let _ = writeln!(s);
            for w in &self.warnings {
                let _ = writeln!(s, "- {}", w);
            }
        }
        if !self.errors.is_empty() {
            let _ = writeln!(s);
            let _ = writeln!(s, "## Errors ({})", self.errors.len());
            let _ = writeln!(s);
            for e in &self.errors {
                let _ = writeln!(s, "- {}", e);
            }
            let _ = writeln!(s);
            let _ = writeln!(s, "## Recovery instructions");
            let _ = writeln!(s);
            if self.rolled_back {
                let _ = writeln!(s, "The migration failed partway through and was automatically rolled back — your database was restored from the pre-migration backup listed above, and **no partial data was left behind**. It is safe to fix the underlying issue (see the errors above) and re-run the migration; already-imported records will be skipped as duplicates, not doubled.");
            } else {
                let _ = writeln!(s, "The migration did not complete successfully. Your original database has not been modified beyond what's reported as imported above. If a pre-migration backup path is listed, you can manually restore it by copying that file back over your live database file while the app is closed. Re-running the migration after resolving the errors above is safe — already-imported records are skipped, not duplicated.");
            }
        }
        s
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_mut_creates_then_reuses_same_entry() {
        let mut r = MigrationReport::new("test.json");
        r.entity_mut("clients").found = 5;
        r.entity_mut("clients").imported = 3;
        assert_eq!(r.entities.len(), 1);
        assert_eq!(r.entities[0].found, 5);
        assert_eq!(r.entities[0].imported, 3);
    }

    #[test]
    fn total_imported_sums_across_entities() {
        let mut r = MigrationReport::new("test.json");
        r.entity_mut("clients").imported = 2;
        r.entity_mut("rules").imported = 7;
        assert_eq!(r.total_imported(), 9);
    }

    #[test]
    fn one_line_summary_reflects_rollback() {
        let mut r = MigrationReport::new("test.json");
        r.error("boom");
        r.finish(false, true);
        let s = r.one_line_summary();
        assert!(s.contains("rolled back"), "got: {s}");
    }

    #[test]
    fn one_line_summary_reflects_success() {
        let mut r = MigrationReport::new("test.json");
        r.entity_mut("clients").imported = 3;
        r.finish(true, false);
        let s = r.one_line_summary();
        assert!(s.contains("3 record"), "got: {s}");
    }

    #[test]
    fn markdown_includes_recovery_instructions_only_on_failure() {
        let mut ok = MigrationReport::new("test.json");
        ok.finish(true, false);
        assert!(!ok.to_markdown().contains("Recovery instructions"));

        let mut failed = MigrationReport::new("test.json");
        failed.error("disk full");
        failed.finish(false, true);
        let md = failed.to_markdown();
        assert!(md.contains("Recovery instructions"));
        assert!(md.contains("disk full"));
        assert!(md.contains("rolled back"));
    }

    #[test]
    fn markdown_table_lists_every_entity() {
        let mut r = MigrationReport::new("test.json");
        r.entity_mut("clients").imported = 1;
        r.entity_mut("transactions").imported = 100;
        let md = r.to_markdown();
        assert!(md.contains("| clients |"));
        assert!(md.contains("| transactions |"));
    }
}
