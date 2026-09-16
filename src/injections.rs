use anyhow::Result;
use chrono::Local;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

use crate::config::codex_dir;
use crate::sessions::{backup_session_file, write_session_content_preserving_mtime};

/// Payload types that a strict Responses upstream rejects when `call_id` is
/// missing. Codex Desktop records injected app items (automation heartbeats,
/// cross-thread messages) as standalone `function_call_output` rows without
/// `call_id` and without a matching `function_call`. The built-in `openai`
/// provider tolerates that shape, while strict third-party providers reject
/// the entire request with `missing field `call_id``.
const REJECTED_PAYLOAD_TYPES: &[&str] = &[
    "function_call",
    "function_call_output",
    "custom_tool_call",
    "custom_tool_call_output",
    "local_shell_call",
    "local_shell_call_output",
];

#[derive(Default)]
pub struct InjectionRepairSummary {
    pub rollouts_scanned: usize,
    pub rollouts_affected: usize,
    pub rollouts_repaired: usize,
    pub items_repaired: usize,
    pub errors: usize,
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
    let mut summary = InjectionRepairSummary::default();
    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%f");
    let backup_dir = codex_dir().join(format!(".sessions_backup_injections_{timestamp}"));

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

            if let Some(since) = modified_since {
                let is_newer = fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .map(|modified| modified > since)
                    .unwrap_or(false);
                if !is_newer {
                    continue;
                }
            }

            summary.rollouts_scanned += 1;
            match repair_injected_file(path, &backup_dir, apply) {
                Ok(result) => {
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

#[derive(Default)]
struct RepairFileResult {
    affected: bool,
    items_repaired: usize,
}

fn repair_injected_file(path: &Path, backup_dir: &Path, apply: bool) -> Result<RepairFileResult> {
    let content = fs::read_to_string(path)?;
    let mut result = RepairFileResult::default();
    let mut repaired_lines = Vec::with_capacity(content.lines().count());

    for line in content.lines() {
        match repair_injected_line(line)? {
            Some(new_line) => {
                result.affected = true;
                result.items_repaired += 1;
                repaired_lines.push(new_line);
            }
            None => repaired_lines.push(line.to_string()),
        }
    }

    if apply && result.affected {
        backup_session_file(path, backup_dir)?;
        write_session_content_preserving_mtime(path, &content, repaired_lines)?;
    }

    Ok(result)
}

fn repair_injected_line(line: &str) -> Result<Option<String>> {
    if !line.contains("\"response_item\"") {
        return Ok(None);
    }
    if !REJECTED_PAYLOAD_TYPES
        .iter()
        .any(|payload_type| line.contains(payload_type))
    {
        return Ok(None);
    }

    let Ok(mut value) = serde_json::from_str::<Value>(line) else {
        return Ok(None);
    };
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return Ok(None);
    }

    let Some(payload) = value.get_mut("payload").and_then(Value::as_object_mut) else {
        return Ok(None);
    };
    let is_rejected_type = payload
        .get("type")
        .and_then(Value::as_str)
        .map(|payload_type| REJECTED_PAYLOAD_TYPES.contains(&payload_type))
        .unwrap_or(false);
    if !is_rejected_type || payload.contains_key("call_id") {
        return Ok(None);
    }

    let text = match payload.get("output") {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };

    let mut replacement = serde_json::Map::new();
    replacement.insert("type".to_string(), Value::String("message".to_string()));
    if let Some(id) = payload.get("id").cloned() {
        replacement.insert("id".to_string(), id);
    }
    replacement.insert("role".to_string(), Value::String("user".to_string()));
    replacement.insert(
        "content".to_string(),
        Value::Array(vec![
            serde_json::json!({ "type": "input_text", "text": text }),
        ]),
    );

    *payload = replacement;
    Ok(Some(serde_json::to_string(&value)?))
}

#[cfg(test)]
mod tests {
    use super::repair_injected_line;

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
        assert_eq!(payload.get("id").and_then(|v| v.as_str()), Some("fco_1"));
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
    fn detects_orphans_whose_text_mentions_call_id() {
        let line = r#"{"type":"response_item","payload":{"type":"function_call_output","output":"note: call_id is missing"}}"#;
        assert!(repair_injected_line(line).expect("repair").is_some());
    }
}
