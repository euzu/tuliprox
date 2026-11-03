//! Offline database CLI execution and reporting, independent of argument parsing.
use std::path::Path;

/// A parsed offline database operation. Paths and options borrow the CLI inputs.
#[derive(Clone, Copy, Debug)]
pub enum DatabaseCommand<'a> {
    Inspect { path: &'a Path, db_type: Option<&'a str> },
    Migrate { path: &'a Path, db_type: Option<&'a str>, no_backup: bool },
    UserGet { path: &'a Path, output: Option<&'a Path>, user: Option<&'a str> },
    UserCheck { path: &'a Path, input: Option<&'a Path> },
    UserSet { path: &'a Path, input: Option<&'a Path> },
    InspectUser { path: &'a Path, template: Option<&'a Path> },
    RepairUser { path: &'a Path, decisions: Option<&'a Path> },
}

/// Runs an offline operation and exits with its CLI status. Call before logging
/// is initialized so JSON exports remain the only content on stdout.
pub fn run_database_command(command: DatabaseCommand<'_>) -> ! {
    match command {
        DatabaseCommand::Inspect { path, db_type } => handle_inspect_db(path, db_type),
        DatabaseCommand::Migrate { path, db_type, no_backup } => handle_migrate_db(path, db_type, no_backup),
        DatabaseCommand::UserGet { path, output, user } => handle_user_db_get(path, output, user),
        DatabaseCommand::UserCheck { path, input } => handle_user_db_check(path, input),
        DatabaseCommand::UserSet { path, input } => {
            let result = input.map_or_else(
                || Err(std::io::Error::other("--user-db-set requires --input")),
                |input| crate::set_user_database(path, input),
            );
            finish_user_db_repair(result);
        }
        DatabaseCommand::InspectUser { path, template } => handle_inspect_user_db(path, template),
        DatabaseCommand::RepairUser { path, decisions } => handle_repair_user_db(path, decisions),
    }
}

fn format_hex_id(id: &[u8; 16]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(32);
    for b in id {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

fn handle_inspect_db(db_path: &Path, db_type: Option<&str>) -> ! {
    match crate::inspect_single_database(db_path, db_type) {
        Ok(report) => {
            println!("=== B+Tree Database Inspection ===");
            println!("Path:              {}", report.path.display());
            println!("File Size:         {} bytes", report.file_size);
            if let Some(v) = report.version {
                println!("Storage Version:   V{v}");
            } else {
                println!("Storage Version:   Not a B+Tree file");
            }
            println!("Database Type:     {}", report.kind);
            println!("Healthy Entries:   {}", report.live_entries);
            if report.corrupted_entries > 0 {
                println!(
                    "Corrupted Entries: {} (corrupted LZ4 blocks or deserialization errors)",
                    report.corrupted_entries
                );
            } else {
                println!("Corrupted Entries: 0");
            }
            if let Some(id) = report.database_id {
                println!("Database ID:       {}", format_hex_id(&id));
            }
            if let Some(gen) = report.generation {
                println!("Generation:        {gen}");
            }
            std::process::exit(0);
        }
        Err(err) => {
            eprintln!("Error inspecting database: {err}");
            std::process::exit(1);
        }
    }
}

fn handle_migrate_db(db_path: &Path, db_type: Option<&str>, no_backup: bool) -> ! {
    let make_backup = !no_backup;
    match crate::migrate_single_database(db_path, db_type, make_backup) {
        Ok(report) => {
            if report.already_current {
                println!(
                    "Database is already at storage version 3 (current). No migration needed: {}",
                    report.path.display()
                );
                std::process::exit(0);
            }
            println!("=== B+Tree Database Migration Complete ===");
            println!("Path:              {}", report.path.display());
            println!("Version:           V{} -> V{}", report.original_version, report.target_version);
            println!("Database Type:     {}", report.kind);
            if let Some(ref bp) = report.backup_path {
                println!("Backup Created:    {}", bp.display());
            }
            println!("Recovered Entries: {}", report.live_entries);
            if report.corrupted_entries > 0 {
                println!("Corrupted Skipped: {} (unreadable records safely bypassed)", report.corrupted_entries);
            } else {
                println!("Corrupted Skipped: 0");
            }
            println!("Database ID:       {}", format_hex_id(&report.database_id));
            println!("Generation:        {}", report.generation);
            println!("Status:            SUCCESS (valid V3 database)");
            std::process::exit(0);
        }
        Err(err) => {
            eprintln!("Error migrating database: {err}");
            std::process::exit(1);
        }
    }
}

fn handle_user_db_get(db_path: &Path, output: Option<&Path>, user: Option<&str>) -> ! {
    let result = crate::get_user_database(db_path, user).and_then(|export| {
        if let Some(path) = output {
            crate::write_user_database_export(db_path, &export, path)?;
            println!("Output Created: {}", path.display());
            println!("Users Exported: {}", export.users.len());
            println!("Resolve unassigned_value entries, then run --user-db-check with --input before --user-db-set.");
        } else {
            println!("{}", serde_json::to_string_pretty(&export).map_err(std::io::Error::other)?);
        }
        Ok(())
    });
    finish_user_db_command(result);
}

fn handle_user_db_check(db_path: &Path, input: Option<&Path>) -> ! {
    let result = crate::check_user_database(db_path, input).map(|report| {
        println!("Status: VALID");
        println!("Records: {}", report.records);
        println!("Resolved Assignments: {}", report.resolved_assignments);
        println!(
            "Recovery History: {}",
            if report.recovery_verified { "VERIFIED" } else { "UNAVAILABLE (set will create verified history)" }
        );
        println!("Database and recovery files were not changed.");
    });
    finish_user_db_command(result);
}

fn finish_user_db_command(result: std::io::Result<()>) -> ! {
    match result {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("Error processing user database: {error}");
            eprintln!("Repair guide: {}", crate::USER_DB_REPAIR_GUIDE);
            std::process::exit(1);
        }
    }
}

fn handle_inspect_user_db(db_path: &Path, template_path: Option<&Path>) -> ! {
    match crate::inspect_user_database(db_path) {
        Ok(report) => {
            println!("=== API User Database Inspection ===");
            println!("Path:                {}", report.path.display());
            println!("Storage Version:     V{}", report.storage_version);
            println!("Records:             {}", report.records);
            if report.live_records_inspected {
                println!("Live Record Scan:    VERIFIED");
                println!("Unambiguous Records: {}", report.unambiguous_records);
                println!("Ambiguous Records:   {}", report.ambiguous_user_keys.len());
            } else {
                println!("Live Record Scan:    UNAVAILABLE (verified recovery history will be used)");
                println!("Unambiguous Records: unknown");
                println!("Ambiguous Records:   unknown");
            }
            match &report.history {
                crate::UserDbHistoryInspection::Verified { database_revision, recovery_revision, records } => {
                    println!("Recovery History:    VERIFIED");
                    println!("Database Revision:   {database_revision}");
                    println!("Recovery Revision:   {recovery_revision}");
                    println!("Recovery Records:    {records}");
                    println!("Repair Strategy:     verified field-named recovery history");
                }
                crate::UserDbHistoryInspection::Unavailable { reason } => {
                    println!("Recovery History:    UNAVAILABLE ({reason})");
                    if report.ambiguous_user_keys.is_empty() {
                        println!("Repair Strategy:     verified live records and fresh recovery history");
                    } else {
                        println!("Repair Strategy:     explicit plan/filter decisions required");
                    }
                }
            }
            for key in &report.ambiguous_user_keys {
                let encoded = serde_json::to_string(key).unwrap_or_else(|_| "\"<invalid-user-key>\"".to_string());
                println!("Ambiguous User Key:  {encoded}");
            }
            println!("{}", crate::user_db_repair_advice(&report, template_path));
            if let Some(path) = template_path {
                if let Err(error) = crate::write_user_db_decision_template(&report, path) {
                    eprintln!("Error creating API user decision template: {error}");
                    std::process::exit(1);
                }
                println!("Template Created:   {}", path.display());
                println!("The template contains usernames, targets and stored values. Leave source unchanged; edit only field.");
                println!(
                    "Resolve every null field from a reliable source, then pass this file to --user-db-decisions."
                );
            }
            std::process::exit(0);
        }
        Err(error) => {
            eprintln!("Error inspecting API user database: {error}");
            eprintln!(
                "Keep the database, WAL and recovery history together; do not delete them or skip invalid users."
            );
            eprintln!("Repair guide: {}", crate::USER_DB_REPAIR_GUIDE);
            std::process::exit(1);
        }
    }
}

fn handle_repair_user_db(db_path: &Path, decisions_path: Option<&Path>) -> ! {
    let result = match decisions_path {
        Some(path) => crate::repair_user_database_with_decision_file(db_path, path),
        None => crate::repair_user_database(db_path, &crate::UserDbRepairDecisions::new()),
    };
    finish_user_db_repair(result);
}

fn finish_user_db_repair(result: std::io::Result<crate::UserDbRepairReport>) -> ! {
    match result {
        Ok(report) => {
            println!("=== API User Database Repair Complete ===");
            println!("Path:              {}", report.path.display());
            println!("Records:           {}", report.records);
            println!(
                "Repair Source:     {}",
                match report.source {
                    crate::UserDbRepairSource::VerifiedHistory => "verified field-named recovery history",
                    crate::UserDbRepairSource::ExplicitLiveRecords => "verified live records",
                }
            );
            println!("Archive Created:   {}", report.backup_path.display());
            println!("Status:            SUCCESS (complete V7 database with verified recovery history)");
            std::process::exit(0);
        }
        Err(error) => {
            eprintln!("Error repairing API user database: {error}");
            eprintln!("Repair guide: {}", crate::USER_DB_REPAIR_GUIDE);
            std::process::exit(1);
        }
    }
}
