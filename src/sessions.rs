use anyhow::{Context, Result};
use chrono::Local;
use filetime::FileTime;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::config::codex_dir;
use crate::history;

#[derive(Debug, Clone, Copy, Default)]
pub struct SessionSyncOptions {
    pub full_rollout_rewrite: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSyncSummary {
    pub rollouts_modified: usize,
    pub rollouts_skipped: usize,
    pub rollout_errors: usize,
    pub metadata_lines_updated: usize,
    pub db_records_updated: usize,
    pub db_backup_files: usize,
    pub db_errors: usize,
    pub history_rollouts_scanned: usize,
    pub history_tool_calls_indexed: usize,
    pub history_command_executions_indexed: usize,
    pub history_errors: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct DbUpdateSummary {
    records_updated: usize,
    backup_files: usize,
    errors: usize,
}

/// Unify session visibility for the given model_provider and model.
///
/// By default only rollout metadata rows are rewritten: `session_meta` carries
/// the provider identity and `turn_context` carries the model. Tool calls,
/// command outputs, assistant messages, and other event rows remain byte-for-
/// byte unchanged. Tool/event rendering belongs in the separate history audit
/// index; adding synthetic response items to rollout files can cause those
/// display-only rows to be replayed as model input. A full rollout rewrite is
/// still available as an explicit recovery operation through
/// `SessionSyncOptions::full_rollout_rewrite`.
pub fn unify_sessions(
    target_provider: &str,
    target_model: &str,
    options: SessionSyncOptions,
) -> Result<SessionSyncSummary> {
    let mut summary = SessionSyncSummary::default();

    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    let backup_dir = codex_dir().join(format!(".sessions_backup_{}", timestamp));

    if options.full_rollout_rewrite {
        println!(
            "  Warning: full rollout JSONL rewrite enabled; originals will be backed up first."
        );
    } else {
        println!("  Rollout JSONL: syncing metadata rows only; event/tool rows stay unchanged.");
    }
    sync_rollout_files(
        target_provider,
        target_model,
        &backup_dir,
        options.full_rollout_rewrite,
        &mut summary,
    );

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
    match history::refresh_history_index() {
        Ok(history_summary) => {
            summary.history_rollouts_scanned = history_summary.rollouts_scanned;
            summary.history_tool_calls_indexed = history_summary.tool_calls_indexed;
            summary.history_command_executions_indexed = history_summary.command_executions_indexed;
            summary.history_errors = history_summary.errors;
            println!(
                "  History index: {} rollout(s), {} tool call(s), {} command execution(s)",
                summary.history_rollouts_scanned,
                summary.history_tool_calls_indexed,
                summary.history_command_executions_indexed
            );
        }
        Err(e) => {
            eprintln!("  Warning: failed to refresh history index: {}", e);
            summary.history_errors += 1;
        }
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

/// Sync rollout files under both live and archived session roots.
fn sync_rollout_files(
    target_provider: &str,
    target_model: &str,
    backup_dir: &Path,
    full_rewrite: bool,
    summary: &mut SessionSyncSummary,
) {
    for root_name in ["sessions", "archived_sessions"] {
        let root = codex_dir().join(root_name);
        if !root.exists() {
            continue;
        }

        for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let fname = path.file_name().unwrap_or_default().to_string_lossy();
            if !fname.starts_with("rollout-") || !fname.ends_with(".jsonl") {
                continue;
            }

            let result = if full_rewrite {
                process_session_file(path, target_provider, target_model, backup_dir)
            } else {
                process_session_metadata_file(path, target_provider, target_model, backup_dir)
            };

            match result {
                Ok(ProcessSessionFileResult {
                    modified: true,
                    lines_updated,
                }) => {
                    summary.rollouts_modified += 1;
                    summary.metadata_lines_updated += lines_updated;
                }
                Ok(ProcessSessionFileResult {
                    modified: false, ..
                }) => summary.rollouts_skipped += 1,
                Err(e) => {
                    eprintln!("  Error processing {}: {}", path.display(), e);
                    summary.rollout_errors += 1;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ProcessSessionFileResult {
    modified: bool,
    lines_updated: usize,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SessionRepairSummary {
    pub rollouts_scanned: usize,
    pub rollouts_affected: usize,
    pub rollouts_repaired: usize,
    pub projection_rows_removed: usize,
    pub invalid_names_normalized: usize,
    pub errors: usize,
    pub backup_dir: Option<PathBuf>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RepairFileResult {
    affected: bool,
    projection_rows_removed: usize,
    invalid_names_normalized: usize,
}

/// Scan or repair rollout rows that newer Responses API validation rejects.
///
/// UCP-owned display projections are removed because they are synthetic UI
/// data and must not become model input. Invalid names on non-UCP historical
/// tool calls are normalized while retaining their call IDs and output pairs.
pub fn repair_session_history(apply: bool) -> Result<SessionRepairSummary> {
    let mut summary = SessionRepairSummary::default();
    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%f");
    let backup_dir = codex_dir().join(format!(".sessions_backup_repair_{timestamp}"));

    for root_name in ["sessions", "archived_sessions"] {
        let root = codex_dir().join(root_name);
        if !root.exists() {
            continue;
        }

        for entry in WalkDir::new(&root)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            let path = entry.path();
            let file_name = path.file_name().unwrap_or_default().to_string_lossy();
            if !path.is_file()
                || !file_name.starts_with("rollout-")
                || !file_name.ends_with(".jsonl")
            {
                continue;
            }

            summary.rollouts_scanned += 1;
            match repair_rollout_file(path, &backup_dir, apply) {
                Ok(result) => {
                    if result.affected {
                        summary.rollouts_affected += 1;
                        if apply {
                            summary.rollouts_repaired += 1;
                        }
                    }
                    summary.projection_rows_removed += result.projection_rows_removed;
                    summary.invalid_names_normalized += result.invalid_names_normalized;
                }
                Err(error) => {
                    eprintln!("  Error repairing {}: {error}", path.display());
                    summary.errors += 1;
                }
            }
        }
    }

    if apply && summary.rollouts_repaired > 0 {
        summary.backup_dir = Some(backup_dir);
    } else {
        let _ = fs::remove_dir(&backup_dir);
    }

    Ok(summary)
}

fn repair_rollout_file(path: &Path, backup_dir: &Path, apply: bool) -> Result<RepairFileResult> {
    let content = fs::read_to_string(path)?;
    let mut result = RepairFileResult::default();
    let mut repaired_lines = Vec::with_capacity(content.lines().count());

    for line in content.lines() {
        let Ok(mut value) = serde_json::from_str::<Value>(line) else {
            repaired_lines.push(line.to_string());
            continue;
        };

        let is_response_item = value.get("type").and_then(Value::as_str) == Some("response_item");
        let is_ucp_projection = is_response_item
            && value
                .get("payload")
                .and_then(|payload| payload.get("ucp_display_projection"))
                .and_then(Value::as_bool)
                == Some(true);

        if is_ucp_projection {
            result.affected = true;
            result.projection_rows_removed += 1;
            continue;
        }

        let mut normalized = false;
        if is_response_item {
            if let Some(payload) = value.get_mut("payload").and_then(Value::as_object_mut) {
                let is_tool_call = matches!(
                    payload.get("type").and_then(Value::as_str),
                    Some("function_call" | "custom_tool_call")
                );
                if is_tool_call {
                    if let Some(name) = payload.get("name").and_then(Value::as_str) {
                        if !is_valid_tool_name(name) {
                            payload.insert(
                                "name".to_string(),
                                Value::String(normalize_tool_name(name)),
                            );
                            normalized = true;
                        }
                    }
                }
            }
        }

        if normalized {
            result.affected = true;
            result.invalid_names_normalized += 1;
            repaired_lines.push(serde_json::to_string(&value)?);
        } else {
            repaired_lines.push(line.to_string());
        }
    }

    if apply && result.affected {
        backup_session_file(path, backup_dir)?;
        write_session_content_preserving_mtime(path, &content, repaired_lines)?;
    }

    Ok(result)
}

fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn normalize_tool_name(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut last_was_separator = false;
    for character in name.chars() {
        let valid = character.is_ascii_alphanumeric() || character == '_' || character == '-';
        if valid {
            normalized.push(character);
            last_was_separator = false;
        } else if !last_was_separator {
            normalized.push('_');
            last_was_separator = true;
        }
    }
    let normalized = normalized.trim_matches('_');
    if normalized.is_empty() {
        "legacy_tool".to_string()
    } else {
        normalized.to_string()
    }
}

/// Process a single session file using the legacy full rewrite behavior.
///
/// This rewrites every parsed JSON line and is intentionally reserved for
/// explicit recovery workflows. Normal switch/sync uses
/// `process_session_metadata_file` so tool calls and command output rows remain
/// byte-for-byte unchanged.
fn process_session_file(
    path: &Path,
    target_provider: &str,
    target_model: &str,
    backup_dir: &Path,
) -> Result<ProcessSessionFileResult> {
    let content = fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Ok(ProcessSessionFileResult::default());
    }

    let needs_update = lines.iter().any(|line| {
        if let Ok(d) = serde_json::from_str::<serde_json::Value>(line) {
            return payload_needs_update(&d, target_provider, target_model);
        }
        false
    });

    if !needs_update {
        return Ok(ProcessSessionFileResult::default());
    }

    backup_session_file(path, backup_dir)?;

    let mut new_lines: Vec<String> = Vec::with_capacity(lines.len());
    let mut lines_updated = 0usize;
    for line in lines.iter() {
        let mut d: serde_json::Value =
            serde_json::from_str(line).context("Failed to parse line")?;
        let line_needs_update = payload_needs_update(&d, target_provider, target_model);

        if let Some(obj) = d.get_mut("payload").and_then(|p| p.as_object_mut()) {
            if line_needs_update {
                lines_updated += 1;
            }
            update_payload(obj, target_provider, target_model);
        }

        new_lines.push(serde_json::to_string(&d)?);
    }

    write_session_content_preserving_mtime(path, &content, new_lines)?;

    Ok(ProcessSessionFileResult {
        modified: true,
        lines_updated,
    })
}

/// Process a single session file by rewriting only metadata JSONL rows.
fn process_session_metadata_file(
    path: &Path,
    target_provider: &str,
    target_model: &str,
    backup_dir: &Path,
) -> Result<ProcessSessionFileResult> {
    let content = fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Ok(ProcessSessionFileResult::default());
    }

    let mut changed = false;
    let mut lines_updated = 0usize;
    let mut new_lines: Vec<String> = Vec::with_capacity(lines.len());

    for line in lines.iter() {
        let Some(updated) = rewrite_metadata_line(line, target_provider, target_model)? else {
            new_lines.push(line.to_string());
            continue;
        };
        changed = true;
        lines_updated += 1;
        new_lines.push(updated);
    }

    if !changed {
        return Ok(ProcessSessionFileResult::default());
    }

    backup_session_file(path, backup_dir)?;
    write_session_content_preserving_mtime(path, &content, new_lines)?;

    Ok(ProcessSessionFileResult {
        modified: true,
        lines_updated,
    })
}

fn rewrite_metadata_line(
    line: &str,
    target_provider: &str,
    target_model: &str,
) -> Result<Option<String>> {
    if !line.contains("\"session_meta\"") && !line.contains("\"turn_context\"") {
        return Ok(None);
    }

    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Ok(None);
    };
    let Some(record_type) = value.get("type").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    if record_type != "session_meta" && record_type != "turn_context" {
        return Ok(None);
    }

    let needs_update = payload_needs_update(&value, target_provider, target_model);
    if !needs_update {
        return Ok(None);
    }

    if let Some(obj) = value.get_mut("payload").and_then(|p| p.as_object_mut()) {
        update_payload(obj, target_provider, target_model);
    }

    Ok(Some(serde_json::to_string(&value)?))
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

fn backup_session_file(path: &Path, backup_dir: &Path) -> Result<()> {
    let base = codex_dir();
    let relative = path
        .strip_prefix(&base)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| {
            path.file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("rollout.jsonl"))
        });
    let backup_path = backup_dir.join(relative);
    if let Some(parent) = backup_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(path, &backup_path)?;
    Ok(())
}

fn write_session_content_preserving_mtime(
    path: &Path,
    original_content: &str,
    new_lines: Vec<String>,
) -> Result<()> {
    let new_content = new_lines.join("\n")
        + if original_content.ends_with('\n') {
            "\n"
        } else {
            ""
        };

    let original_mtime = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(FileTime::from_system_time);

    fs::write(path, new_content)?;

    if let Some(mtime) = original_mtime {
        let _ = filetime::set_file_mtime(path, mtime);
    }

    Ok(())
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
    use super::{
        normalize_tool_name, process_session_file, process_session_metadata_file,
        repair_rollout_file,
    };
    use filetime::FileTime;
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
        assert!(changed.modified);

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
        assert!(changed.modified);

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

    #[test]
    fn metadata_sync_does_not_materialize_display_projections() {
        let root = temp_root();
        let _ = fs::remove_dir_all(&root);
        let backup_dir = root.join("backup");
        fs::create_dir_all(&backup_dir).unwrap();
        let path = root.join("rollout-test.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"target\",\"model\":\"gpt-5.5\"}}\n",
                "{\"timestamp\":\"2026-06-14T01:02:08Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"mcp_tool_call_end\",\"call_id\":\"call_mcp\",\"invocation\":{\"server\":\"codex_app\",\"tool\":\"read_thread_terminal\",\"arguments\":{\"limit\":10}},\"result\":{\"Ok\":\"terminal output\"}}}\n",
                "{\"timestamp\":\"2026-06-14T01:02:10Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"dynamic_tool_call_request\",\"callId\":\"call_dynamic\",\"namespace\":\"codex_app\",\"tool\":\"load_workspace_dependencies\",\"arguments\":{\"include\":\"runtime\"}}}\n",
                "{\"timestamp\":\"2026-06-14T01:02:11Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"dynamic_tool_call_response\",\"call_id\":\"call_dynamic\",\"tool\":\"load_workspace_dependencies\",\"success\":true,\"content_items\":[{\"text\":\"deps loaded\"}]}}\n",
                "{\"timestamp\":\"2026-06-14T01:02:13Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"exec_command_output\",\"call_id\":\"call_stream\",\"stdout\":\"streamed output\"}}\n",
                "{\"timestamp\":\"2026-06-14T01:02:14Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"exec_command_end\",\"call_id\":\"call_end\",\"command\":[\"/bin/zsh\",\"-lc\",\"printf done\"],\"aggregated_output\":\"aggregated only\",\"formatted_output\":\"formatted duplicate\",\"status\":\"completed\"}}\n"
            ),
        )
        .unwrap();

        let first = process_session_metadata_file(&path, "target", "gpt-5.5", &backup_dir).unwrap();
        assert!(!first.modified);
        assert_eq!(first.lines_updated, 0);

        let content = fs::read_to_string(&path).unwrap();
        let rows: Vec<Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert!(
            rows.iter()
                .any(|row| row["type"] == "event_msg"
                    && row["payload"]["type"] == "mcp_tool_call_end")
        );
        assert!(rows
            .iter()
            .all(|row| row["payload"]["ucp_display_projection"] != true));

        let second =
            process_session_metadata_file(&path, "target", "gpt-5.5", &backup_dir).unwrap();
        assert!(!second.modified);

        let second_rows: Vec<Value> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), second_rows.len());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn repairs_incompatible_tool_rows_with_backup_and_preserved_mtime() {
        let root = temp_root();
        let _ = fs::remove_dir_all(&root);
        let backup_dir = root.join("backup");
        let path = root.join("rollout-test.jsonl");
        fs::create_dir_all(&root).unwrap();
        let original = concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"target\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\",\"name\":\"computer-use.click\",\"call_id\":\"projection-call\",\"input\":\"{}\",\"ucp_display_projection\":true}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call_output\",\"call_id\":\"projection-call\",\"output\":\"ok\",\"ucp_display_projection\":true}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\",\"name\":\"computer-use.get_app_state\",\"call_id\":\"native-call\",\"input\":\"{}\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call_output\",\"call_id\":\"native-call\",\"output\":\"ok\"}}\n",
            "not json\n"
        );
        fs::write(&path, original).unwrap();
        let original_mtime = FileTime::from_unix_time(1_700_000_000, 0);
        filetime::set_file_mtime(&path, original_mtime).unwrap();

        let dry_run = repair_rollout_file(&path, &backup_dir, false).unwrap();
        assert!(dry_run.affected);
        assert_eq!(dry_run.projection_rows_removed, 2);
        assert_eq!(dry_run.invalid_names_normalized, 1);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!backup_dir.exists());

        let applied = repair_rollout_file(&path, &backup_dir, true).unwrap();
        assert_eq!(applied, dry_run);
        assert_eq!(
            fs::read_to_string(backup_dir.join("rollout-test.jsonl")).unwrap(),
            original
        );

        let repaired = fs::read_to_string(&path).unwrap();
        assert!(!repaired.contains("ucp_display_projection"));
        assert!(repaired.contains("\"name\":\"computer-use_get_app_state\""));
        assert!(repaired.contains("\"call_id\":\"native-call\""));
        assert!(repaired.contains("not json"));
        assert_eq!(
            FileTime::from_last_modification_time(&fs::metadata(&path).unwrap()),
            original_mtime
        );

        let second = repair_rollout_file(&path, &backup_dir, true).unwrap();
        assert!(!second.affected);
        assert_eq!(second.projection_rows_removed, 0);
        assert_eq!(second.invalid_names_normalized, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn normalizes_tool_names_to_the_responses_api_character_set() {
        assert_eq!(
            normalize_tool_name("computer-use.click"),
            "computer-use_click"
        );
        assert_eq!(
            normalize_tool_name("btt::run javascript"),
            "btt_run_javascript"
        );
        assert_eq!(normalize_tool_name("工具"), "legacy_tool");
        assert_eq!(normalize_tool_name("already_valid-1"), "already_valid-1");
    }
}
