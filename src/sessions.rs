use anyhow::{Context, Result};
use chrono::Local;
use filetime::FileTime;
use serde_json::{json, Value};
use std::collections::HashSet;
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
    pub display_lines_added: usize,
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
/// byte unchanged. UCP may add marked `response_item` display projection rows
/// for top-level low-level event messages so Codex history replay can render
/// them. A full rollout rewrite is still available as an explicit recovery
/// operation through `SessionSyncOptions::full_rollout_rewrite`.
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
        println!(
            "  Rollout JSONL: syncing metadata rows and display projections; original event/tool rows stay unchanged."
        );
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
    if summary.display_lines_added > 0 {
        println!(
            "  Display projections: added {} response item row(s)",
            summary.display_lines_added
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
                    display_lines_added,
                }) => {
                    summary.rollouts_modified += 1;
                    summary.metadata_lines_updated += lines_updated;
                    summary.display_lines_added += display_lines_added;
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
    display_lines_added: usize,
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
        display_lines_added: 0,
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
    let mut display_lines_added = 0usize;
    let mut new_lines: Vec<String> = Vec::with_capacity(lines.len());
    let mut display_state = DisplayProjectionState::from_lines(&lines);

    for (idx, line) in lines.iter().enumerate() {
        let Some(updated) = rewrite_metadata_line(line, target_provider, target_model)? else {
            new_lines.push(line.to_string());
            let projections = display_projection_lines(line, idx + 1, &mut display_state)?;
            if !projections.is_empty() {
                display_lines_added += projections.len();
                changed = true;
                new_lines.extend(projections);
            }
            continue;
        };
        changed = true;
        lines_updated += 1;
        new_lines.push(updated);
        let projections = display_projection_lines(line, idx + 1, &mut display_state)?;
        if !projections.is_empty() {
            display_lines_added += projections.len();
            new_lines.extend(projections);
        }
    }

    if !changed {
        return Ok(ProcessSessionFileResult::default());
    }

    backup_session_file(path, backup_dir)?;
    write_session_content_preserving_mtime(path, &content, new_lines)?;

    Ok(ProcessSessionFileResult {
        modified: true,
        lines_updated,
        display_lines_added,
    })
}

#[derive(Debug, Default)]
struct DisplayProjectionState {
    call_ids: HashSet<String>,
    output_ids: HashSet<String>,
}

impl DisplayProjectionState {
    fn from_lines(lines: &[&str]) -> Self {
        let mut state = Self::default();
        for line in lines {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("response_item") {
                continue;
            }
            let Some(payload) = value.get("payload") else {
                continue;
            };
            let Some(call_id) = payload.get("call_id").and_then(Value::as_str) else {
                continue;
            };
            match payload.get("type").and_then(Value::as_str) {
                Some(
                    "function_call" | "custom_tool_call" | "tool_search_call" | "web_search_call",
                ) => {
                    state.call_ids.insert(call_id.to_string());
                }
                Some("function_call_output" | "custom_tool_call_output" | "tool_search_output") => {
                    state.output_ids.insert(call_id.to_string());
                }
                _ => {}
            }
        }
        state
    }
}

fn display_projection_lines(
    line: &str,
    line_number: usize,
    state: &mut DisplayProjectionState,
) -> Result<Vec<String>> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Ok(Vec::new());
    };
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return Ok(Vec::new());
    }
    let Some(payload) = value.get("payload") else {
        return Ok(Vec::new());
    };
    let Some(payload_type) = payload.get("type").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };

    let fallback_call_id = format!("ucp-display-{}-{}", payload_type, line_number);
    let call_id = event_call_id(payload).unwrap_or(fallback_call_id);
    let mut projections = Vec::new();

    match payload_type {
        "exec_command_end" | "exec_command_output" => {
            let command = exec_command_text(payload).unwrap_or_else(|| payload_type.to_string());
            let mut args = json!({ "cmd": command });
            if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
                args["workdir"] = Value::String(cwd.to_string());
            }
            push_call_projection(
                &mut projections,
                state,
                &value,
                &call_id,
                "function_call",
                "exec_command",
                serde_json::to_string(&args)?,
                event_status(payload),
            )?;
            if let Some(output) = event_output_text(payload) {
                push_output_projection(
                    &mut projections,
                    state,
                    &value,
                    &call_id,
                    "function_call_output",
                    output,
                )?;
            }
        }
        "mcp_tool_call_end" => {
            let invocation = payload.get("invocation").unwrap_or(&Value::Null);
            let tool_name = mcp_tool_name(invocation);
            let input = invocation
                .get("arguments")
                .map(value_to_text)
                .unwrap_or_default();
            push_call_projection(
                &mut projections,
                state,
                &value,
                &call_id,
                "custom_tool_call",
                &tool_name,
                input,
                mcp_status(payload),
            )?;
            if let Some(output) = payload.get("result").map(value_to_text) {
                push_output_projection(
                    &mut projections,
                    state,
                    &value,
                    &call_id,
                    "custom_tool_call_output",
                    truncate_projection_output(output),
                )?;
            }
        }
        "patch_apply_end" => {
            let input = payload
                .get("changes")
                .map(value_to_text)
                .unwrap_or_default();
            push_call_projection(
                &mut projections,
                state,
                &value,
                &call_id,
                "custom_tool_call",
                "apply_patch",
                input,
                event_status(payload),
            )?;
            if let Some(output) = event_output_text(payload) {
                push_output_projection(
                    &mut projections,
                    state,
                    &value,
                    &call_id,
                    "custom_tool_call_output",
                    output,
                )?;
            }
        }
        "dynamic_tool_call_request" => {
            let Some(tool_name) = dynamic_tool_name(payload) else {
                return Ok(projections);
            };
            let input = payload
                .get("arguments")
                .map(value_to_text)
                .unwrap_or_default();
            push_call_projection(
                &mut projections,
                state,
                &value,
                &call_id,
                "custom_tool_call",
                &tool_name,
                input,
                None,
            )?;
        }
        "dynamic_tool_call_response" => {
            let Some(tool_name) = dynamic_tool_name(payload) else {
                return Ok(projections);
            };
            if !state.call_ids.contains(&call_id) {
                let input = payload
                    .get("arguments")
                    .map(value_to_text)
                    .unwrap_or_default();
                push_call_projection(
                    &mut projections,
                    state,
                    &value,
                    &call_id,
                    "custom_tool_call",
                    &tool_name,
                    input,
                    dynamic_status(payload),
                )?;
            }
            if let Some(output) = dynamic_output(payload) {
                push_output_projection(
                    &mut projections,
                    state,
                    &value,
                    &call_id,
                    "custom_tool_call_output",
                    output,
                )?;
            }
        }
        "view_image_tool_call" => {
            let input = payload.get("path").map(value_to_text).unwrap_or_default();
            push_call_projection(
                &mut projections,
                state,
                &value,
                &call_id,
                "custom_tool_call",
                "view_image",
                input,
                None,
            )?;
        }
        "web_search_end" => {
            let input = payload
                .get("query")
                .or_else(|| payload.get("action"))
                .map(value_to_text)
                .unwrap_or_default();
            push_call_projection(
                &mut projections,
                state,
                &value,
                &call_id,
                "web_search_call",
                "web_search",
                input,
                None,
            )?;
        }
        _ => {}
    }

    Ok(projections)
}

fn push_call_projection(
    projections: &mut Vec<String>,
    state: &mut DisplayProjectionState,
    source: &Value,
    call_id: &str,
    response_type: &str,
    name: &str,
    input: String,
    status: Option<String>,
) -> Result<()> {
    if !state.call_ids.insert(call_id.to_string()) {
        return Ok(());
    }

    let input_key = if response_type == "function_call" {
        "arguments"
    } else {
        "input"
    };
    let mut payload = json!({
        "type": response_type,
        "name": name,
        "call_id": call_id,
        input_key: input,
        "ucp_display_projection": true
    });
    if let Some(status) = status {
        payload["status"] = Value::String(status);
    }
    projections.push(serde_json::to_string(&response_item_projection(
        source, payload,
    ))?);
    Ok(())
}

fn push_output_projection(
    projections: &mut Vec<String>,
    state: &mut DisplayProjectionState,
    source: &Value,
    call_id: &str,
    response_type: &str,
    output: String,
) -> Result<()> {
    if !state.output_ids.insert(call_id.to_string()) {
        return Ok(());
    }
    let payload = json!({
        "type": response_type,
        "call_id": call_id,
        "output": output,
        "ucp_display_projection": true
    });
    projections.push(serde_json::to_string(&response_item_projection(
        source, payload,
    ))?);
    Ok(())
}

fn response_item_projection(source: &Value, payload: Value) -> Value {
    let mut item = json!({
        "type": "response_item",
        "payload": payload
    });
    if let Some(timestamp) = source.get("timestamp").cloned() {
        item["timestamp"] = timestamp;
    }
    item
}

fn event_call_id(payload: &Value) -> Option<String> {
    payload
        .get("call_id")
        .or_else(|| payload.get("callId"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn dynamic_tool_name(payload: &Value) -> Option<String> {
    let tool = payload.get("tool").and_then(Value::as_str)?;
    let namespace = payload
        .get("namespace")
        .and_then(Value::as_str)
        .filter(|namespace| !namespace.is_empty());
    Some(
        namespace
            .map(|namespace| format!("{namespace}.{tool}"))
            .unwrap_or_else(|| tool.to_string()),
    )
}

fn mcp_tool_name(invocation: &Value) -> String {
    let tool = invocation
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("mcp_tool");
    invocation
        .get("server")
        .and_then(Value::as_str)
        .filter(|server| !server.is_empty())
        .map(|server| format!("{server}.{tool}"))
        .unwrap_or_else(|| tool.to_string())
}

fn exec_command_text(payload: &Value) -> Option<String> {
    payload
        .get("command")
        .and_then(Value::as_array)
        .and_then(|items| items.iter().rev().find_map(Value::as_str))
        .map(ToString::to_string)
        .or_else(|| {
            payload
                .get("cmd")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
}

fn mcp_status(payload: &Value) -> Option<String> {
    let result = payload.get("result")?;
    if result.get("Ok").is_some() {
        Some("completed".to_string())
    } else if result.get("Err").is_some() {
        Some("failed".to_string())
    } else {
        event_status(payload)
    }
}

fn dynamic_status(payload: &Value) -> Option<String> {
    event_status(payload).or_else(|| {
        payload
            .get("error")
            .filter(|error| !error.is_null())
            .map(|_| {
                if payload.get("success").and_then(Value::as_bool) == Some(true) {
                    "completed".to_string()
                } else {
                    "failed".to_string()
                }
            })
    })
}

fn event_status(payload: &Value) -> Option<String> {
    payload
        .get("status")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            payload
                .get("success")
                .and_then(Value::as_bool)
                .map(|success| {
                    if success {
                        "completed".to_string()
                    } else {
                        "failed".to_string()
                    }
                })
        })
}

fn dynamic_output(payload: &Value) -> Option<String> {
    payload
        .get("content_items")
        .or_else(|| payload.get("error"))
        .or_else(|| payload.get("result"))
        .map(value_to_text)
        .map(truncate_projection_output)
}

fn event_output_text(payload: &Value) -> Option<String> {
    for key in ["aggregated_output", "formatted_output"] {
        if let Some(text) = payload.get(key).map(value_to_text) {
            if !text.is_empty() {
                return Some(truncate_projection_output(text));
            }
        }
    }

    let mut parts = Vec::new();
    for key in ["stdout", "stderr"] {
        if let Some(text) = payload.get(key).map(value_to_text) {
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }
    if parts.is_empty() {
        payload
            .get("output")
            .or_else(|| payload.get("result"))
            .map(value_to_text)
            .map(truncate_projection_output)
    } else {
        Some(truncate_projection_output(parts.join("\n")))
    }
}

fn truncate_projection_output(output: String) -> String {
    const MAX_DISPLAY_PROJECTION_CHARS: usize = 8192;
    if output.chars().nth(MAX_DISPLAY_PROJECTION_CHARS).is_none() {
        return output;
    }
    let preview: String = output.chars().take(MAX_DISPLAY_PROJECTION_CHARS).collect();
    format!("{preview}\n[truncated by ucp display projection]")
}

fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
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
    use super::{process_session_file, process_session_metadata_file};
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
    fn adds_idempotent_display_projections_for_top_level_events() {
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
        assert!(first.modified);
        assert_eq!(first.lines_updated, 0);
        assert_eq!(first.display_lines_added, 8);

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
        assert!(rows.iter().any(|row| row["type"] == "response_item"
            && row["payload"]["ucp_display_projection"] == true
            && row["payload"]["type"] == "custom_tool_call"
            && row["payload"]["name"] == "codex_app.read_thread_terminal"));
        assert!(rows.iter().any(|row| row["type"] == "response_item"
            && row["payload"]["ucp_display_projection"] == true
            && row["payload"]["type"] == "custom_tool_call_output"
            && row["payload"]["call_id"] == "call_dynamic"
            && row["payload"]["output"]
                .as_str()
                .is_some_and(|output| output.contains("deps loaded"))));
        assert!(rows.iter().any(|row| row["type"] == "response_item"
            && row["payload"]["ucp_display_projection"] == true
            && row["payload"]["type"] == "function_call_output"
            && row["payload"]["call_id"] == "call_stream"
            && row["payload"]["output"] == "streamed output"));
        assert!(rows.iter().any(|row| row["type"] == "response_item"
            && row["payload"]["ucp_display_projection"] == true
            && row["payload"]["type"] == "function_call_output"
            && row["payload"]["call_id"] == "call_end"
            && row["payload"]["output"] == "aggregated only"));

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
}
