use anyhow::{Context, Result};
use chrono::Local;
use filetime::FileTime;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::config::codex_dir;

#[derive(Debug, Clone, Copy, Default)]
pub struct SessionSyncOptions {
    pub rewrite_rollouts: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSyncSummary {
    pub rollouts_modified: usize,
    pub rollouts_skipped: usize,
    pub rollout_errors: usize,
    pub db_records_updated: usize,
    pub db_backup_files: usize,
    pub db_errors: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct DbUpdateSummary {
    records_updated: usize,
    backup_files: usize,
    errors: usize,
}

/// Unify session visibility for the given model_provider and model.
///
/// By default rollout JSONL files are treated as append-only audit logs and are
/// left untouched. Codex Desktop discovers historical threads through SQLite
/// indexes, so the default sync updates those indexes only. Rewriting rollout
/// metadata remains available as an explicit recovery operation through
/// `SessionSyncOptions::rewrite_rollouts`.
pub fn unify_sessions(
    target_provider: &str,
    target_model: &str,
    options: SessionSyncOptions,
) -> Result<SessionSyncSummary> {
    let mut summary = SessionSyncSummary::default();

    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    let backup_dir = codex_dir().join(format!(".sessions_backup_{}", timestamp));

    if options.rewrite_rollouts {
        println!("  Warning: rewriting rollout JSONL metadata; originals will be backed up first.");
        let sessions_dir = codex_dir().join("sessions");
        if sessions_dir.exists() {
            fs::create_dir_all(&backup_dir)?;

            for entry in WalkDir::new(&sessions_dir)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let fname = path.file_name().unwrap_or_default().to_string_lossy();
                if !fname.starts_with("rollout-") || !fname.ends_with(".jsonl") {
                    continue;
                }

                match process_session_file(path, target_provider, target_model, &backup_dir) {
                    Ok(true) => summary.rollouts_modified += 1,
                    Ok(false) => summary.rollouts_skipped += 1,
                    Err(e) => {
                        eprintln!("  Error processing {}: {}", path.display(), e);
                        summary.rollout_errors += 1;
                    }
                }
            }
        }
    } else {
        println!("  Rollout JSONL: left unchanged (SQLite index sync only).");
    }

    match update_sessions_db(target_provider, target_model, &backup_dir) {
        Ok(db_summary) => {
            summary.db_records_updated = db_summary.records_updated;
            summary.db_backup_files = db_summary.backup_files;
            summary.db_errors = db_summary.errors;
        }
        Err(e) => {
            eprintln!("  Warning: failed to update session databases: {}", e);
            summary.db_errors += 1;
        }
    }

    if summary.db_records_updated > 0 {
        println!(
            "  Database: updated {} thread records",
            summary.db_records_updated
        );
    }
    if summary.db_backup_files > 0 {
        println!(
            "  Database backups: copied {} file(s) to {}",
            summary.db_backup_files,
            backup_dir.display()
        );
    }

    if summary.rollouts_modified == 0 && summary.db_backup_files == 0 {
        let _ = fs::remove_dir(&backup_dir);
    }

    // Retain only the 3 most recent session backups to prevent unbounded disk growth.
    prune_session_backups(3);

    Ok(summary)
}

/// Remove old `.sessions_backup_*` directories, keeping only the most recent `keep` entries.
pub fn prune_session_backups(keep: usize) {
    let base = codex_dir();
    let mut backups: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(&base) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(".sessions_backup_") && entry.path().is_dir() {
                backups.push(name);
            }
        }
    }
    if backups.len() <= keep {
        return;
    }
    // Sort ascending by name (timestamp suffix gives chronological order).
    backups.sort();
    let to_remove = backups.len() - keep;
    for name in backups.iter().take(to_remove) {
        let path = base.join(name);
        if let Err(e) = fs::remove_dir_all(&path) {
            eprintln!("  Warning: failed to remove old backup {}: {}", name, e);
        } else {
            println!("  Pruned old session backup: {}", name);
        }
    }
}

/// Process a single session file: update every existing model_provider field,
/// and every non-empty model field.
fn process_session_file(
    path: &Path,
    target_provider: &str,
    target_model: &str,
    backup_dir: &PathBuf,
) -> Result<bool> {
    let content = fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Ok(false);
    }

    let needs_update = lines.iter().any(|line| {
        if let Ok(d) = serde_json::from_str::<serde_json::Value>(line) {
            return payload_needs_update(&d, target_provider, target_model);
        }
        false
    });

    if !needs_update {
        return Ok(false);
    }

    // Backup
    let backup_path = backup_dir.join(path.file_name().unwrap());
    fs::copy(path, &backup_path)?;

    let mut new_lines: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines.iter() {
        let mut d: serde_json::Value =
            serde_json::from_str(line).context("Failed to parse line")?;

        if let Some(obj) = d.get_mut("payload").and_then(|p| p.as_object_mut()) {
            update_payload(obj, target_provider, target_model);
        }

        new_lines.push(serde_json::to_string(&d)?);
    }

    let new_content = new_lines.join("\n") + if content.ends_with('\n') { "\n" } else { "" };

    // Preserve original mtime so Codex session ordering is not disturbed
    let original_mtime = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(FileTime::from_system_time);

    fs::write(path, new_content)?;

    if let Some(mtime) = original_mtime {
        let _ = filetime::set_file_mtime(path, mtime);
    }

    Ok(true)
}

fn payload_needs_update(
    value: &serde_json::Value,
    target_provider: &str,
    target_model: &str,
) -> bool {
    let Some(payload) = value.get("payload").and_then(|v| v.as_object()) else {
        return false;
    };

    if let Some(provider) = payload.get("model_provider").and_then(|v| v.as_str()) {
        if provider != target_provider {
            return true;
        }
    }

    if let Some(model) = payload.get("model").and_then(|v| v.as_str()) {
        if !model.is_empty() && model != target_model {
            return true;
        }
    }

    if let Some(settings) = payload
        .get("collaboration_mode")
        .and_then(|v| v.as_object())
        .and_then(|v| v.get("settings"))
        .and_then(|v| v.as_object())
    {
        if let Some(provider) = settings.get("model_provider").and_then(|v| v.as_str()) {
            if provider != target_provider {
                return true;
            }
        }
        if let Some(model) = settings.get("model").and_then(|v| v.as_str()) {
            if !model.is_empty() && model != target_model {
                return true;
            }
        }
    }

    false
}

fn update_payload(
    payload: &mut serde_json::Map<String, serde_json::Value>,
    target_provider: &str,
    target_model: &str,
) {
    if payload
        .get("model_provider")
        .and_then(|v| v.as_str())
        .is_some()
    {
        payload.insert(
            "model_provider".to_string(),
            serde_json::Value::String(target_provider.to_string()),
        );
    }

    let has_non_empty_model = payload
        .get("model")
        .and_then(|v| v.as_str())
        .map(|m| !m.is_empty())
        .unwrap_or(false);
    if has_non_empty_model {
        payload.insert(
            "model".to_string(),
            serde_json::Value::String(target_model.to_string()),
        );
    }

    if let Some(settings) = payload
        .get_mut("collaboration_mode")
        .and_then(|v| v.as_object_mut())
        .and_then(|v| v.get_mut("settings"))
        .and_then(|v| v.as_object_mut())
    {
        if settings
            .get("model_provider")
            .and_then(|v| v.as_str())
            .is_some()
        {
            settings.insert(
                "model_provider".to_string(),
                serde_json::Value::String(target_provider.to_string()),
            );
        }

        let has_non_empty_settings_model = settings
            .get("model")
            .and_then(|v| v.as_str())
            .map(|m| !m.is_empty())
            .unwrap_or(false);
        if has_non_empty_settings_model {
            settings.insert(
                "model".to_string(),
                serde_json::Value::String(target_model.to_string()),
            );
        }
    }
}

/// Update the SQLite database threads table.
fn update_sessions_db(
    target_provider: &str,
    target_model: &str,
    backup_dir: &Path,
) -> Result<DbUpdateSummary> {
    // Codex may store its state DB at either legacy `~/.codex/state_5.sqlite`
    // or the newer `~/.codex/sqlite/state_5.sqlite`. Update all that exist.
    let base = codex_dir();
    let candidates = [
        base.join("state_5.sqlite"),
        base.join("sqlite").join("state_5.sqlite"),
    ];

    let mut summary = DbUpdateSummary::default();
    for db_path in &candidates {
        if !db_path.exists() {
            continue;
        }
        match update_single_db(db_path, target_provider, target_model, backup_dir) {
            Ok(db_summary) => {
                summary.records_updated += db_summary.records_updated;
                summary.backup_files += db_summary.backup_files;
                summary.errors += db_summary.errors;
            }
            Err(e) => {
                eprintln!("  Warning: failed to update {}: {}", db_path.display(), e);
                summary.errors += 1;
            }
        }
    }

    Ok(summary)
}

/// Update a single SQLite state database.
fn update_single_db(
    db_path: &Path,
    target_provider: &str,
    target_model: &str,
    backup_dir: &Path,
) -> Result<DbUpdateSummary> {
    let conn = rusqlite::Connection::open(db_path)
        .with_context(|| format!("Failed to open {}", db_path.display()))?;
    let table_exists: bool = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='threads'")?
        .exists([])?;
    if !table_exists {
        return Ok(DbUpdateSummary::default());
    }

    let needs_update: usize = conn.query_row(
        "SELECT COUNT(*)
         FROM threads
         WHERE model_provider IS NULL
            OR model_provider != ?1
            OR (model IS NOT NULL AND model != '' AND model != ?2)",
        (target_provider, target_model),
        |row| row.get(0),
    )?;
    if needs_update == 0 {
        return Ok(DbUpdateSummary::default());
    }
    drop(conn);

    let backup_files = backup_sqlite_files(db_path, backup_dir)?;

    let conn = rusqlite::Connection::open(db_path)
        .with_context(|| format!("Failed to open {}", db_path.display()))?;
    let updated = conn.execute(
        "UPDATE threads
         SET model_provider = ?1,
             model = CASE
                 WHEN model IS NULL OR model = '' THEN model
                 ELSE ?2
             END
         WHERE model_provider IS NULL
            OR model_provider != ?1
            OR (model IS NOT NULL AND model != '' AND model != ?2)",
        (target_provider, target_model),
    )?;
    Ok(DbUpdateSummary {
        records_updated: updated,
        backup_files,
        errors: 0,
    })
}

fn backup_sqlite_files(db_path: &Path, backup_dir: &Path) -> Result<usize> {
    let base = codex_dir();
    let paths = [
        db_path.to_path_buf(),
        sqlite_sidecar_path(db_path, "-wal"),
        sqlite_sidecar_path(db_path, "-shm"),
    ];

    let mut copied = 0usize;
    for src in paths {
        if !src.exists() {
            continue;
        }

        let relative = src
            .strip_prefix(&base)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| {
                src.file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("state_5.sqlite"))
            });
        let dst = backup_dir.join(relative);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&src, &dst).with_context(|| {
            format!(
                "Failed to back up SQLite state file {} to {}",
                src.display(),
                dst.display()
            )
        })?;
        copied += 1;
    }

    Ok(copied)
}

fn sqlite_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut raw = db_path.as_os_str().to_os_string();
    raw.push(suffix);
    PathBuf::from(raw)
}

#[cfg(test)]
mod tests {
    use super::process_session_file;
    use serde_json::Value;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ucp-session-test-{}-{}-{:?}",
            std::process::id(),
            nonce,
            std::thread::current().id()
        ))
    }

    #[test]
    fn updates_all_existing_provider_and_model_fields() {
        let root = temp_root();
        let _ = fs::remove_dir_all(&root);
        let backup_dir = root.join("backup");
        fs::create_dir_all(&backup_dir).unwrap();
        let path = root.join("rollout-test.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"a2o_proxy\",\"model\":null}}\n",
                "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"openai\",\"model\":\"gpt-5.4\"}}\n",
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.5-pro\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"model_provider\":null,\"model\":null}}\n"
            ),
        )
        .unwrap();

        let changed = process_session_file(&path, "newapi", "gpt-5.5", &backup_dir).unwrap();
        assert!(changed);

        let content = fs::read_to_string(&path).unwrap();
        let rows: Vec<Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(rows[0]["payload"]["model_provider"], "newapi");
        assert!(rows[0]["payload"]["model"].is_null());
        assert_eq!(rows[1]["payload"]["model_provider"], "newapi");
        assert_eq!(rows[1]["payload"]["model"], "gpt-5.5");
        assert_eq!(rows[2]["payload"]["model"], "gpt-5.5");
        assert!(rows[3]["payload"]["model_provider"].is_null());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn updates_nested_collaboration_mode_model_fields() {
        let root = temp_root();
        let _ = fs::remove_dir_all(&root);
        let backup_dir = root.join("backup");
        fs::create_dir_all(&backup_dir).unwrap();
        let path = root.join("rollout-test.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.4\",\"collaboration_mode\":{\"settings\":{\"model\":\"gpt-5.5-pro\",\"reasoning_effort\":\"high\"}}}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"collaboration_mode\":{\"settings\":{\"model\":\"gpt-5.5-max\"}}}}\n"
            ),
        )
        .unwrap();

        let changed = process_session_file(&path, "switch", "gpt-5.5", &backup_dir).unwrap();
        assert!(changed);

        let content = fs::read_to_string(&path).unwrap();
        let rows: Vec<Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(rows[0]["payload"]["model"], "gpt-5.5");
        assert_eq!(
            rows[0]["payload"]["collaboration_mode"]["settings"]["model"],
            "gpt-5.5"
        );
        assert_eq!(
            rows[1]["payload"]["collaboration_mode"]["settings"]["model"],
            "gpt-5.5"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
