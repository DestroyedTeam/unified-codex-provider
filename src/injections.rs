use anyhow::{bail, Context, Result};
use chrono::Local;
use fs2::FileExt;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

use crate::config::codex_dir;
use crate::sessions::backup_session_file;

pub const REPAIR_VERSION: u32 = 3;

#[derive(Default)]
pub struct InjectionRepairSummary {
    pub rollouts_scanned: usize,
    pub rollouts_affected: usize,
    pub rollouts_repaired: usize,
    pub items_repaired: usize,
    pub errors: usize,
    pub traversal_errors: usize,
    pub skipped_active: usize,
    pub deferred_rollouts: Vec<PathBuf>,
    pub backup_dir: Option<PathBuf>,
}

/// Rewrite injected app items that strict Responses providers reject into
/// plain user messages, keeping their text and letting history replay.
///
/// `modified_since` limits the scan to rollouts touched after a previous
/// incremental repair; `None` scans every rollout under `sessions/` and
/// `archived_sessions/`.
pub fn repair_injected_items(
    apply: bool,
    modified_since: Option<SystemTime>,
) -> Result<InjectionRepairSummary> {
    repair_with_deferred(apply, modified_since, &[])
}

pub fn repair_with_deferred(
    apply: bool,
    modified_since: Option<SystemTime>,
    deferred: &[PathBuf],
) -> Result<InjectionRepairSummary> {
    let mut summary = InjectionRepairSummary::default();
    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%f");
    let backup_dir = codex_dir().join(format!(".sessions_backup_injections_{timestamp}"));

    for root_name in ["sessions", "archived_sessions"] {
        let root = codex_dir().join(root_name);
        if !root.exists() {
            continue;
        }

        for entry in WalkDir::new(&root) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    summary.errors += 1;
                    summary.traversal_errors += 1;
                    continue;
                }
            };
            let path = entry.path();
            let file_name = path.file_name().unwrap_or_default().to_string_lossy();
            if !entry.file_type().is_file()
                || !file_name.starts_with("rollout-")
                || !file_name.ends_with(".jsonl")
            {
                continue;
            }

            if let Some(since) = modified_since {
                let is_newer = fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .map(|modified| modified > since);
                let is_newer = match is_newer {
                    Ok(value) => value,
                    Err(_) => {
                        summary.errors += 1;
                        summary.deferred_rollouts.push(path.to_path_buf());
                        continue;
                    }
                };
                if !is_newer && !deferred.iter().any(|pending| pending == path) {
                    continue;
                }
            }

            summary.rollouts_scanned += 1;
            match repair_injected_file(path, &backup_dir, apply) {
                Ok(result) => {
                    if result.skipped_active {
                        summary.skipped_active += 1;
                        summary.deferred_rollouts.push(path.to_path_buf());
                    }
                    if result.affected {
                        summary.rollouts_affected += 1;
                        if apply {
                            summary.rollouts_repaired += 1;
                        }
                    }
                    summary.items_repaired += result.items_repaired;
                }
                Err(error) => {
                    eprintln!(
                        "  Error repairing injected items in {}: {error}",
                        path.display()
                    );
                    summary.errors += 1;
                    summary.deferred_rollouts.push(path.to_path_buf());
                }
            }
        }
    }

    if apply && backup_dir.exists() {
        summary.backup_dir = Some(backup_dir);
    } else {
        let _ = fs::remove_dir(&backup_dir);
    }

    Ok(summary)
}

#[derive(Default)]
struct RepairFileResult {
    skipped_active: bool,
    affected: bool,
    items_repaired: usize,
}

struct TemporaryRollout(PathBuf);
impl Drop for TemporaryRollout {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn writer_guard(id: &str) -> Result<Option<fs::File>> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        bail!("invalid thread identity; refusing rollout write");
    }
    let dir = codex_dir().join("thread-writer-locks");
    fs::create_dir_all(&dir)?;
    let coordination = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(".coordination.lock"))?;
    match coordination.try_lock_exclusive() {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(format!("{id}.lock")))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn repair_injected_file(path: &Path, backup_dir: &Path, apply: bool) -> Result<RepairFileResult> {
    let metadata = fs::metadata(path)?;
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let meta: Value = serde_json::from_str(&first).unwrap_or(Value::Null);
    let id = meta.pointer("/payload/id").and_then(Value::as_str);
    let _guard = if apply {
        if let Some(id) = id {
            match writer_guard(id)? {
                Some(guard) => Some(guard),
                None => {
                    return Ok(RepairFileResult {
                        skipped_active: true,
                        ..Default::default()
                    })
                }
            }
        } else {
            None
        }
    } else {
        None
    };
    let mut result = RepairFileResult::default();
    let mut temp = None;
    let mut output: Option<BufWriter<fs::File>> = None;
    let mut line = first;
    let mut old_offset = 0u64;
    let mut changes = Vec::new();
    loop {
        let fixed = repair_injected_line(line.trim_end_matches(['\r', '\n']))?;
        if let Some(fixed) = fixed {
            result.affected = true;
            result.items_repaired += 1;
            let newline_bytes = if line.ends_with("\r\n") {
                2
            } else if line.ends_with('\n') {
                1
            } else {
                0
            };
            changes.push((
                old_offset,
                old_offset + line.len() as u64,
                fixed.len() as u64 + newline_bytes,
            ));
            if apply && output.is_none() {
                let temp_path = path.with_extension(format!("repair-{}", std::process::id()));
                let file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temp_path)?;
                temp = Some(TemporaryRollout(temp_path));
                fs::set_permissions(&temp.as_ref().unwrap().0, metadata.permissions())?;
                let mut writer = BufWriter::new(file);
                let mut prefix = std::io::Read::take(fs::File::open(path)?, old_offset);
                std::io::copy(&mut prefix, &mut writer)?;
                output = Some(writer);
            }
            if let Some(out) = output.as_mut() {
                out.write_all(fixed.as_bytes())?;
                if line.ends_with("\r\n") {
                    out.write_all(b"\r\n")?;
                } else if line.ends_with('\n') {
                    out.write_all(b"\n")?;
                }
            }
        } else if let Some(out) = output.as_mut() {
            out.write_all(line.as_bytes())?;
        }
        old_offset += line.len() as u64;
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line.len() > 128 * 1024 * 1024 {
            bail!("oversized rollout record; repair deferred");
        }
    }
    if apply && result.affected {
        let id = id.context("missing session identity; refusing rollout write")?;
        if meta
            .pointer("/payload/history_base")
            .is_some_and(|v| !v.is_null())
        {
            bail!("segmented history needs lineage-aware projection rebuild; repair deferred");
        }
        let out = output.as_mut().unwrap();
        out.flush()?;
        out.get_ref().sync_all()?;
        let current = fs::metadata(path)?;
        if current.len() != metadata.len() || current.modified()? != metadata.modified()? {
            bail!("rollout changed during repair; refusing overwrite");
        }
        fs::create_dir_all(backup_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(backup_dir, fs::Permissions::from_mode(0o700))?;
        }
        backup_session_file(path, backup_dir)?;
        let temp = temp.as_ref().unwrap();
        filetime::set_file_mtime(
            &temp.0,
            filetime::FileTime::from_last_modification_time(&metadata),
        )?;
        drop(output.take());
        if let Err(e) = publish_repair(id, path, &temp.0, backup_dir, &changes) {
            let backup = backup_dir.join(path.strip_prefix(codex_dir())?);
            fs::copy(backup, path)?;
            filetime::set_file_mtime(
                path,
                filetime::FileTime::from_last_modification_time(&metadata),
            )?;
            return Err(e);
        }
    }
    Ok(result)
}

// Existing UI rows may predate rollout ordinals. Retain them and translate only
// the byte checkpoint; rebuilding from scratch can lose those legacy rows.
fn translated_offset(offset: u64, changes: &[(u64, u64, u64)]) -> Result<u64> {
    let mut result = i128::from(offset);
    for &(start, end, new_len) in changes {
        if start < offset && offset < end {
            bail!("projection checkpoint lies inside a changed record; repair deferred");
        }
        if end <= offset {
            result += i128::from(new_len) - i128::from(end - start);
        }
    }
    Ok(u64::try_from(result)?)
}

fn publish_repair(
    id: &str,
    path: &Path,
    temporary: &Path,
    backup_dir: &Path,
    changes: &[(u64, u64, u64)],
) -> Result<()> {
    let mut conn = rusqlite::Connection::open_in_memory()?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    let mut databases = Vec::new();
    for (index, relative) in ["thread_history_1.sqlite", "sqlite/thread_history_1.sqlite"]
        .iter()
        .enumerate()
    {
        let path = codex_dir().join(relative);
        if !path.exists() {
            continue;
        }
        let alias = format!("history{index}");
        conn.execute(
            &format!("ATTACH DATABASE ?1 AS {alias}"),
            [path.to_string_lossy().as_ref()],
        )?;
        let state = path.parent().unwrap().join("state_5.sqlite");
        databases.push((alias, state));
    }
    if databases.is_empty() {
        fs::rename(temporary, path)?;
        return Ok(());
    }
    let backup = backup_dir.join("projection.sqlite");
    conn.execute(
        "ATTACH DATABASE ?1 AS repair_backup",
        [backup.to_string_lossy().as_ref()],
    )?;
    // Lock projection writes before replacing the rollout. The same transaction
    // backs up checkpoint rows and updates both supported database locations.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for (alias, state_path) in databases {
        let exists: bool = tx.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {alias}.sqlite_master WHERE name='thread_history_projection_state' AND type='table')"), [], |r| r.get(0))?;
        if !exists {
            continue;
        }
        let table = format!("{alias}.thread_history_projection_state");
        use rusqlite::OptionalExtension;
        let offset: Option<u64> = tx
            .query_row(
                &format!("SELECT next_rollout_byte_offset FROM {table} WHERE thread_id=?1"),
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(offset) = offset else {
            continue;
        };
        // Archived copies can share the same thread ID. Only the canonical
        // rollout named in the state database owns this projection checkpoint.
        if !state_path.exists() {
            bail!("projection has no state database to identify its rollout; repair deferred");
        }
        let state = rusqlite::Connection::open_with_flags(
            state_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let canonical: Option<String> = state
            .query_row("SELECT rollout_path FROM threads WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .flatten();
        let canonical =
            canonical.context("projection has no canonical rollout; repair deferred")?;
        let canonical = PathBuf::from(canonical);
        let canonical = if canonical.is_absolute() {
            canonical
        } else {
            codex_dir().join(canonical)
        };
        if fs::canonicalize(canonical)? != fs::canonicalize(path)? {
            continue;
        }
        let new_offset = translated_offset(offset, changes)?;
        let backup_table = format!("repair_backup.{alias}_projection_state");
        tx.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {backup_table} AS SELECT * FROM {table} WHERE 0"
        ))?;
        tx.execute(
            &format!("INSERT INTO {backup_table} SELECT * FROM {table} WHERE thread_id=?1"),
            [id],
        )?;
        tx.execute(
            &format!("UPDATE {table} SET next_rollout_byte_offset=?1 WHERE thread_id=?2"),
            rusqlite::params![new_offset, id],
        )?;
    }
    fs::rename(temporary, path)?;
    tx.commit()?;
    Ok(())
}

fn repair_injected_line(line: &str) -> Result<Option<String>> {
    if !line.contains("\"response_item\"") && !line.contains("\"compacted\"") {
        return Ok(None);
    }

    let Ok(mut value) = serde_json::from_str::<Value>(line) else {
        return Ok(None);
    };
    let changed = match value.get("type").and_then(Value::as_str) {
        Some("response_item") => value
            .get_mut("payload")
            .map(repair_item)
            .transpose()?
            .unwrap_or(false),
        Some("compacted") => {
            let mut changed = false;
            if let Some(items) = value
                .pointer_mut("/payload/replacement_history")
                .and_then(Value::as_array_mut)
            {
                for item in items {
                    changed |= repair_item(item)?;
                }
            }
            changed
        }
        _ => false,
    };
    Ok(if changed {
        Some(serde_json::to_string(&value)?)
    } else {
        None
    })
}

fn repair_item(item: &mut Value) -> Result<bool> {
    let Some(payload) = item.as_object_mut() else {
        return Ok(false);
    };
    if matches!(
        payload.get("type").and_then(Value::as_str),
        Some("function_call" | "custom_tool_call")
    ) {
        if let Some(name) = payload.get("name").and_then(Value::as_str) {
            if !crate::sessions::is_valid_tool_name(name) {
                payload.insert(
                    "name".into(),
                    Value::String(crate::sessions::normalize_tool_name(name)),
                );
                return Ok(true);
            }
        }
    }
    // Older repairs kept a tool-output ID after changing its type to message.
    if payload.get("type").and_then(Value::as_str) == Some("message") {
        if let Some(id) = payload.get("id").and_then(Value::as_str) {
            if let Some(suffix) = id.strip_prefix("fco_") {
                payload.insert("id".into(), Value::String(format!("msg_{suffix}")));
                return Ok(true);
            }
        }
        return Ok(false);
    }

    // Third-party adapters store plaintext reasoning with UUID placeholders,
    // which cannot be replayed as native OpenAI encrypted reasoning. Keep the
    // text as assistant analysis; UI event rows remain byte-for-byte intact.
    if payload.get("type").and_then(Value::as_str) == Some("reasoning") {
        let id = payload.get("id").and_then(Value::as_str).unwrap_or("");
        let is_uuid = |s: &str| {
            s.len() == 36
                && s.bytes().enumerate().all(|(i, c)| {
                    if [8, 13, 18, 23].contains(&i) {
                        c == b'-'
                    } else {
                        c.is_ascii_hexdigit()
                    }
                })
        };
        let placeholder = match payload.get("encrypted_content") {
            None | Some(Value::Null) => true,
            Some(Value::String(s)) => s.rsplit_once('-').is_some_and(|(id, n)| {
                is_uuid(id) && !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())
            }),
            _ => false,
        };
        let content = payload.get("content").and_then(Value::as_array);
        let summary = payload.get("summary").and_then(Value::as_array);
        let valid_text = |v: &Value, kind: &str| {
            v.get("type").and_then(Value::as_str) == Some(kind)
                && v.get("text").and_then(Value::as_str).is_some()
        };
        if is_uuid(id)
            && placeholder
            && content
                .is_some_and(|v| !v.is_empty() && v.iter().all(|x| valid_text(x, "reasoning_text")))
            && summary.is_some_and(|v| v.iter().all(|x| valid_text(x, "summary_text")))
        {
            let text: Vec<Value> = summary
                .unwrap()
                .iter()
                .chain(content.unwrap())
                .map(|v| serde_json::json!({"type":"output_text", "text":v["text"]}))
                .collect();
            let mut replacement = serde_json::json!({
                "type":"message", "id":format!("msg_{id}"), "role":"assistant",
                "channel":"analysis", "content":text
            });
            if let Some(metadata) = payload.get("internal_chat_message_metadata_passthrough") {
                replacement["internal_chat_message_metadata_passthrough"] = metadata.clone();
            }
            *payload = replacement.as_object().unwrap().clone();
            return Ok(true);
        }
        return Ok(false);
    }

    let known_injection = payload.get("type").and_then(Value::as_str)
        == Some("function_call_output")
        && payload.get("namespace").and_then(Value::as_str) == Some("codex_app")
        && matches!(
            payload.get("name").and_then(Value::as_str),
            Some("automation_update" | "send_message_to_thread")
        )
        && payload.get("output").and_then(Value::as_str).is_some();
    if !known_injection || payload.contains_key("call_id") {
        return Ok(false);
    }
    let text = match payload.get("output") {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };

    let mut replacement = serde_json::Map::new();
    replacement.insert("type".to_string(), Value::String("message".to_string()));
    if let Some(id) = payload.get("id").cloned() {
        if let Some(id) = id.as_str() {
            let id = if id.starts_with("msg_") {
                id.to_string()
            } else {
                format!("msg_{}", id.strip_prefix("fco_").unwrap_or(id))
            };
            replacement.insert("id".to_string(), Value::String(id));
        }
    }
    replacement.insert("role".to_string(), Value::String("user".to_string()));
    replacement.insert(
        "content".to_string(),
        Value::Array(vec![
            serde_json::json!({ "type": "input_text", "text": text }),
        ]),
    );

    if let Some(metadata) = payload.get("internal_chat_message_metadata_passthrough") {
        replacement.insert(
            "internal_chat_message_metadata_passthrough".into(),
            metadata.clone(),
        );
    }
    *payload = replacement;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::repair_injected_line;

    #[test]
    fn repairs_compacted_history_without_touching_arguments_or_ui_events() {
        let items = serde_json::json!([
            {"type":"function_call","name":"mcp__tools::exec_command","call_id":"call_1","arguments":"{\"name\":\"a.b\"}"},
            {"type":"custom_tool_call","name":"mcp__node_repl.js","call_id":"call_2","input":"original"},
            {"type":"function_call_output","call_id":"call_1","output":"ok"},
            {"type":"message","id":"fco_legacy","role":"user","content":[{"type":"input_text","text":"all original text"}]}
        ]);
        let original = serde_json::json!({"type":"compacted","payload":{"replacement_history":items,"message":"summary"}});
        let fixed = repair_injected_line(&original.to_string())
            .unwrap()
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&fixed).unwrap();
        let history = &value["payload"]["replacement_history"];
        assert_eq!(history[0]["name"], "mcp__tools_exec_command");
        assert_eq!(history[1]["name"], "mcp__node_repl_js");
        assert_eq!(history[0]["arguments"], items[0]["arguments"]);
        assert_eq!(history[0]["call_id"], items[0]["call_id"]);
        assert_eq!(history[2], items[2]);
        assert_eq!(history[3]["id"], "msg_legacy");
        assert_eq!(history[3]["content"], items[3]["content"]);
        assert!(repair_injected_line(&fixed).unwrap().is_none());
        let ui = serde_json::json!({"type":"event_msg","payload":items});
        assert!(repair_injected_line(&ui.to_string()).unwrap().is_none());
        for item in items.as_array().unwrap().iter().take(2) {
            let row = serde_json::json!({"type":"response_item","payload":item});
            assert!(repair_injected_line(&row.to_string()).unwrap().is_some());
        }
    }

    #[test]
    fn keeps_unknown_orphans_and_real_function_calls() {
        for item in [
            serde_json::json!({"type":"function_call","name":"run","arguments":"valuable"}),
            serde_json::json!({"type":"function_call_output","output":"valuable"}),
            serde_json::json!({"type":"custom_tool_call_output","output":[{"type":"image","data":"valuable"}]}),
        ] {
            let row = serde_json::json!({"type":"response_item","payload":item});
            assert!(repair_injected_line(&row.to_string()).unwrap().is_none());
        }
    }

    #[test]
    fn translates_partial_projection_checkpoints_at_record_boundaries() {
        let changes = [(10, 20, 15), (30, 50, 12)];
        assert_eq!(super::translated_offset(0, &changes).unwrap(), 0);
        assert_eq!(super::translated_offset(10, &changes).unwrap(), 10);
        assert_eq!(super::translated_offset(20, &changes).unwrap(), 25);
        assert_eq!(super::translated_offset(25, &changes).unwrap(), 30);
        assert_eq!(super::translated_offset(50, &changes).unwrap(), 47);
        assert!(super::translated_offset(15, &changes).is_err());
    }

    #[test]
    fn rewrites_orphan_function_call_output() {
        let line = r#"{"timestamp":"2026-09-16T09:32:07Z","ordinal":1,"type":"response_item","payload":{"type":"function_call_output","id":"fco_1","name":"automation_update","namespace":"codex_app","output":"<heartbeat>go</heartbeat>"}}"#;
        let repaired = repair_injected_line(line)
            .expect("repair")
            .expect("line is repaired");
        let value: serde_json::Value = serde_json::from_str(&repaired).expect("valid json");
        let payload = value.get("payload").expect("payload");
        assert_eq!(
            payload.get("type").and_then(|v| v.as_str()),
            Some("message")
        );
        assert_eq!(payload.get("role").and_then(|v| v.as_str()), Some("user"));
        assert_eq!(payload.get("id").and_then(|v| v.as_str()), Some("msg_1"));
        assert_eq!(
            payload
                .get("content")
                .and_then(|content| content.get(0))
                .and_then(|item| item.get("text"))
                .and_then(|text| text.as_str()),
            Some("<heartbeat>go</heartbeat>")
        );
    }

    #[test]
    fn keeps_paired_outputs_and_other_rows() {
        let paired = r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"ok"}}"#;
        assert!(repair_injected_line(paired).expect("repair").is_none());
        let message =
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[]}}"#;
        assert!(repair_injected_line(message).expect("repair").is_none());
        let meta = r#"{"type":"session_meta","payload":{"id":"thread"}}"#;
        assert!(repair_injected_line(meta).expect("repair").is_none());
        let garbage = "not json";
        assert!(repair_injected_line(garbage).expect("repair").is_none());
    }

    #[test]
    fn repairs_previous_message_ids_without_changing_text() {
        let line = r#"{"type":"response_item","payload":{"type":"message","id":"fco_123","role":"user","content":[{"type":"input_text","text":"heartbeat"}]}}"#;
        let fixed = repair_injected_line(line).unwrap().unwrap();
        let mut expected: serde_json::Value = serde_json::from_str(line).unwrap();
        expected["payload"]["id"] = "msg_123".into();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&fixed).unwrap(),
            expected
        );
        assert!(repair_injected_line(&fixed).unwrap().is_none());
    }

    #[test]
    fn preserves_foreign_reasoning_as_analysis_but_keeps_native_encryption() {
        let mut value = serde_json::json!({"type":"response_item","payload":{
            "type":"reasoning","id":"49128d42-2f71-41a0-86df-ec01ad9b475d",
            "summary":[{"type":"summary_text","text":"summary"}],
            "content":[{"type":"reasoning_text","text":"original text"}],
            "encrypted_content":"b0df28fe-1330-40ab-a940-7f09b22f84b1-0"
        }});
        let fixed = repair_injected_line(&value.to_string()).unwrap().unwrap();
        let result: serde_json::Value = serde_json::from_str(&fixed).unwrap();
        assert_eq!(result["payload"]["channel"], "analysis");
        assert_eq!(result["payload"]["content"][0]["text"], "summary");
        assert_eq!(result["payload"]["content"][1]["text"], "original text");
        assert!(repair_injected_line(&fixed).unwrap().is_none());
        value["payload"]["encrypted_content"] = "gAAAA-native-opaque".into();
        assert!(repair_injected_line(&value.to_string()).unwrap().is_none());
    }

    #[test]
    fn detects_orphans_whose_text_mentions_call_id() {
        let line = r#"{"type":"response_item","payload":{"type":"function_call_output","namespace":"codex_app","name":"automation_update","output":"note: call_id is missing"}}"#;
        assert!(repair_injected_line(line).expect("repair").is_some());
    }
}
