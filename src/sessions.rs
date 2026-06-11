use anyhow::{Context, Result};
use chrono::Local;
use filetime::FileTime;
use std::fs;
use std::path::PathBuf;
use walkdir::WalkDir;

use crate::config::codex_dir;

/// Unify all sessions to use the given model_provider and model.
/// Returns (modified_count, skipped_count, error_count).
pub fn unify_sessions(target_provider: &str, target_model: &str) -> Result<(usize, usize, usize)> {
    let sessions_dir = codex_dir().join("sessions");
    if !sessions_dir.exists() {
        return Ok((0, 0, 0));
    }

    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    let backup_dir = codex_dir().join(format!(".sessions_backup_{}", timestamp));
    fs::create_dir_all(&backup_dir)?;

    let mut modified = 0;
    let mut skipped = 0;
    let mut errors = 0;

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
            Ok(true) => modified += 1,
            Ok(false) => skipped += 1,
            Err(e) => {
                eprintln!("  Error processing {}: {}", path.display(), e);
                errors += 1;
            }
        }
    }

    let db_updated = update_sessions_db(target_provider, target_model).unwrap_or(0);
    if db_updated > 0 {
        println!("  Database: updated {} thread records", db_updated);
    }

    if modified == 0 {
        let _ = fs::remove_dir(&backup_dir);
    }

    Ok((modified, skipped, errors))
}

/// Process a single session file: update every existing model_provider field,
/// and every non-empty model field.
fn process_session_file(
    path: &std::path::Path,
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

/// Update the SQLite database threads table
fn update_sessions_db(target_provider: &str, target_model: &str) -> Result<usize> {
    let db_path = codex_dir().join("state_5.sqlite");
    if !db_path.exists() {
        return Ok(0);
    }
    let conn = rusqlite::Connection::open(&db_path).context("Failed to open state_5.sqlite")?;
    let table_exists: bool = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='threads'")?
        .exists([])?;
    if !table_exists {
        return Ok(0);
    }
    let updated = conn.execute(
        "UPDATE threads
         SET model_provider = ?1,
             model = CASE
                 WHEN model IS NULL OR model = '' THEN model
                 ELSE ?2
             END
         WHERE model_provider != ?1
            OR (model IS NOT NULL AND model != '' AND model != ?2)",
        (target_provider, target_model),
    )?;
    Ok(updated)
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
