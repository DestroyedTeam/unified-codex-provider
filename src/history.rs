use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use walkdir::WalkDir;

use crate::config::codex_dir;

const HISTORY_DIR: &str = ".ucp_history";
const TOOL_CALLS_FILE: &str = "tool_calls.jsonl";
const COMMAND_EXECUTIONS_FILE: &str = "command_executions.jsonl";
const SUMMARY_FILE: &str = "summary.json";
const DEFAULT_PREVIEW_CHARS: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct HistoryIndexSummary {
    pub generated_at: String,
    pub rollouts_scanned: usize,
    pub live_rollouts: usize,
    pub archived_rollouts: usize,
    pub json_lines_read: usize,
    pub malformed_lines: usize,
    pub tool_calls_indexed: usize,
    pub command_executions_indexed: usize,
    pub output_previews_indexed: usize,
    pub errors: usize,
    pub index_dir: String,
}

#[derive(Debug, Clone, Serialize)]
struct ToolCallRecord {
    thread_id: Option<String>,
    timestamp: Option<String>,
    rollout_path: String,
    call_line: usize,
    output_line: Option<usize>,
    tool_kind: String,
    tool_name: String,
    call_id: Option<String>,
    status: Option<String>,
    command: Option<String>,
    cwd: Option<String>,
    arguments_preview: Option<String>,
    arguments_bytes: Option<usize>,
    output_preview: Option<String>,
    output_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
struct CommandExecutionRecord {
    thread_id: Option<String>,
    timestamp: Option<String>,
    rollout_path: String,
    call_line: Option<usize>,
    output_line: Option<usize>,
    event_line: Option<usize>,
    call_id: Option<String>,
    execution_kind: String,
    tool_name: String,
    command: Option<String>,
    command_argv: Option<Vec<String>>,
    cwd: Option<String>,
    status: Option<String>,
    exit_code: Option<i64>,
    duration_ms: Option<u64>,
    parsed_commands: Vec<ParsedCommandRecord>,
    output_preview: Option<String>,
    output_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Default)]
struct ParsedCommandRecord {
    kind: Option<String>,
    cmd: Option<String>,
    name: Option<String>,
    path: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct PartialToolCall {
    thread_id: Option<String>,
    timestamp: Option<String>,
    rollout_path: String,
    call_line: usize,
    output_line: Option<usize>,
    tool_kind: String,
    tool_name: String,
    call_id: Option<String>,
    status: Option<String>,
    command: Option<String>,
    cwd: Option<String>,
    arguments_preview: Option<String>,
    arguments_bytes: Option<usize>,
    output_preview: Option<String>,
    output_bytes: Option<usize>,
}

#[derive(Debug, Clone, Default)]
struct ExecEvent {
    thread_id: Option<String>,
    timestamp: Option<String>,
    rollout_path: String,
    event_line: usize,
    call_id: Option<String>,
    command: Option<String>,
    command_argv: Option<Vec<String>>,
    cwd: Option<String>,
    status: Option<String>,
    exit_code: Option<i64>,
    duration_ms: Option<u64>,
    parsed_commands: Vec<ParsedCommandRecord>,
    output_preview: Option<String>,
    output_bytes: Option<usize>,
}

/// Refresh UCP's read-only history audit index from raw Codex rollout JSONL.
///
/// The raw rollout files remain the source of truth. This index exists so UCP
/// can recover command/tool history even when Codex Desktop history replay omits
/// low-level command execution items.
pub fn refresh_history_index() -> Result<HistoryIndexSummary> {
    refresh_history_index_with_preview(DEFAULT_PREVIEW_CHARS)
}

fn refresh_history_index_with_preview(preview_chars: usize) -> Result<HistoryIndexSummary> {
    let base = codex_dir();
    let index_dir = base.join(HISTORY_DIR);
    refresh_history_index_at(&base, &index_dir, preview_chars)
}

fn refresh_history_index_at(
    codex_home: &Path,
    index_dir: &Path,
    preview_chars: usize,
) -> Result<HistoryIndexSummary> {
    fs::create_dir_all(index_dir)
        .with_context(|| format!("Failed to create {}", index_dir.display()))?;

    let tool_tmp = index_dir.join(format!("{TOOL_CALLS_FILE}.tmp"));
    let exec_tmp = index_dir.join(format!("{COMMAND_EXECUTIONS_FILE}.tmp"));
    let summary_tmp = index_dir.join(format!("{SUMMARY_FILE}.tmp"));

    let tool_file = File::create(&tool_tmp)
        .with_context(|| format!("Failed to create {}", tool_tmp.display()))?;
    let exec_file = File::create(&exec_tmp)
        .with_context(|| format!("Failed to create {}", exec_tmp.display()))?;
    let mut tool_writer = BufWriter::new(tool_file);
    let mut exec_writer = BufWriter::new(exec_file);

    let mut summary = HistoryIndexSummary {
        generated_at: Local::now().to_rfc3339(),
        index_dir: relative_path(codex_home, index_dir),
        ..HistoryIndexSummary::default()
    };

    for root_name in ["sessions", "archived_sessions"] {
        let root = codex_home.join(root_name);
        if !root.exists() {
            continue;
        }

        for entry in WalkDir::new(&root)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            let path = entry.path();
            if !is_rollout_file(path) {
                continue;
            }

            summary.rollouts_scanned += 1;
            if root_name == "sessions" {
                summary.live_rollouts += 1;
            } else {
                summary.archived_rollouts += 1;
            }

            if let Err(err) = index_rollout_file(
                codex_home,
                path,
                preview_chars,
                &mut tool_writer,
                &mut exec_writer,
                &mut summary,
            ) {
                summary.errors += 1;
                eprintln!(
                    "  Warning: failed to index history {}: {}",
                    path.display(),
                    err
                );
            }
        }
    }

    tool_writer.flush()?;
    exec_writer.flush()?;

    let summary_content = serde_json::to_string_pretty(&summary)?;
    fs::write(&summary_tmp, summary_content)?;

    fs::rename(&tool_tmp, index_dir.join(TOOL_CALLS_FILE))?;
    fs::rename(&exec_tmp, index_dir.join(COMMAND_EXECUTIONS_FILE))?;
    fs::rename(&summary_tmp, index_dir.join(SUMMARY_FILE))?;

    Ok(summary)
}

fn index_rollout_file(
    codex_home: &Path,
    path: &Path,
    preview_chars: usize,
    tool_writer: &mut BufWriter<File>,
    exec_writer: &mut BufWriter<File>,
    summary: &mut HistoryIndexSummary,
) -> Result<()> {
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let rollout_path = relative_path(codex_home, path);
    let mut thread_id = thread_id_from_rollout_path(path);
    let mut calls: Vec<PartialToolCall> = Vec::new();
    let mut calls_by_id: HashMap<String, usize> = HashMap::new();
    let mut events_by_id: HashMap<String, ExecEvent> = HashMap::new();
    let mut anonymous_events: Vec<ExecEvent> = Vec::new();

    for (idx, line) in reader.lines().enumerate() {
        let line_number = idx + 1;
        let line = line?;
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                summary.malformed_lines += 1;
                continue;
            }
        };
        summary.json_lines_read += 1;

        if let Some(meta_id) = session_meta_id(&value) {
            thread_id = Some(meta_id);
        }

        let Some(payload) = value.get("payload") else {
            continue;
        };
        let Some(payload_type) = payload.get("type").and_then(Value::as_str) else {
            continue;
        };
        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(ToString::to_string);

        match payload_type {
            "function_call" | "custom_tool_call" => {
                let Some(call) = parse_tool_call(
                    payload,
                    payload_type,
                    &rollout_path,
                    line_number,
                    thread_id.clone(),
                    timestamp,
                    preview_chars,
                ) else {
                    continue;
                };
                if let Some(call_id) = call.call_id.clone() {
                    calls_by_id.insert(call_id, calls.len());
                }
                calls.push(call);
            }
            "function_call_output" | "custom_tool_call_output" => {
                let Some(call_id) = payload.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(call_index) = calls_by_id.get(call_id).copied() else {
                    continue;
                };
                let output = payload
                    .get("output")
                    .map(|output| value_to_text(output))
                    .unwrap_or_default();
                let (preview, bytes) = preview_text(&output, preview_chars);
                let call = &mut calls[call_index];
                call.output_line = Some(line_number);
                call.output_preview = preview;
                call.output_bytes = Some(bytes);
            }
            "exec_command_end" => {
                let event = parse_exec_event(
                    payload,
                    &rollout_path,
                    line_number,
                    thread_id.clone(),
                    timestamp,
                    preview_chars,
                );
                if let Some(call_id) = event.call_id.clone() {
                    events_by_id.insert(call_id, event);
                } else {
                    anonymous_events.push(event);
                }
            }
            _ => {}
        }
    }

    let mut emitted_event_ids: HashSet<String> = HashSet::new();
    for call in calls {
        write_tool_call_record(tool_writer, &call, summary)?;

        if is_execution_tool(&call.tool_name) {
            let event = call
                .call_id
                .as_ref()
                .and_then(|call_id| events_by_id.get(call_id));
            if let Some(call_id) = &call.call_id {
                emitted_event_ids.insert(call_id.clone());
            }
            write_command_execution_for_call(exec_writer, &call, event, summary)?;
        }
    }

    for (call_id, event) in events_by_id {
        if emitted_event_ids.contains(&call_id) {
            continue;
        }
        write_command_execution_for_event(exec_writer, &event, summary)?;
    }

    for event in anonymous_events {
        write_command_execution_for_event(exec_writer, &event, summary)?;
    }

    Ok(())
}

fn parse_tool_call(
    payload: &Value,
    payload_type: &str,
    rollout_path: &str,
    line_number: usize,
    thread_id: Option<String>,
    timestamp: Option<String>,
    preview_chars: usize,
) -> Option<PartialToolCall> {
    let tool_name = payload.get("name").and_then(Value::as_str)?.to_string();
    let call_id = payload
        .get("call_id")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let status = payload
        .get("status")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let argument_value = if payload_type == "function_call" {
        payload.get("arguments")
    } else {
        payload.get("input")
    };
    let arguments_text = argument_value.map(value_to_text).unwrap_or_default();
    let (arguments_preview, arguments_bytes) = preview_text(&arguments_text, preview_chars);
    let parsed_args = argument_value.and_then(parse_argument_value);
    let command = extract_command(&tool_name, parsed_args.as_ref(), &arguments_text);
    let cwd = parsed_args
        .as_ref()
        .and_then(|args| {
            args.get("workdir")
                .or_else(|| args.get("cwd"))
                .and_then(Value::as_str)
        })
        .map(ToString::to_string);

    Some(PartialToolCall {
        thread_id,
        timestamp,
        rollout_path: rollout_path.to_string(),
        call_line: line_number,
        output_line: None,
        tool_kind: if payload_type == "function_call" {
            "function".to_string()
        } else {
            "custom".to_string()
        },
        tool_name,
        call_id,
        status,
        command,
        cwd,
        arguments_preview,
        arguments_bytes: Some(arguments_bytes),
        output_preview: None,
        output_bytes: None,
    })
}

fn parse_exec_event(
    payload: &Value,
    rollout_path: &str,
    line_number: usize,
    thread_id: Option<String>,
    timestamp: Option<String>,
    preview_chars: usize,
) -> ExecEvent {
    let command_argv = payload
        .get("command")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|items| !items.is_empty());
    let command = command_argv
        .as_ref()
        .and_then(|items| items.last().cloned())
        .or_else(|| {
            payload
                .get("cmd")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        });
    let output = payload
        .get("aggregated_output")
        .or_else(|| payload.get("formatted_output"))
        .or_else(|| payload.get("stdout"))
        .or_else(|| payload.get("stderr"))
        .map(value_to_text)
        .unwrap_or_default();
    let (output_preview, output_bytes) = preview_text(&output, preview_chars);

    ExecEvent {
        thread_id,
        timestamp,
        rollout_path: rollout_path.to_string(),
        event_line: line_number,
        call_id: payload
            .get("call_id")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        command,
        command_argv,
        cwd: payload
            .get("cwd")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        status: payload
            .get("status")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        exit_code: payload.get("exit_code").and_then(Value::as_i64),
        duration_ms: duration_ms(payload.get("duration")),
        parsed_commands: parse_parsed_commands(payload.get("parsed_cmd")),
        output_preview,
        output_bytes: Some(output_bytes),
    }
}

fn write_tool_call_record(
    writer: &mut BufWriter<File>,
    call: &PartialToolCall,
    summary: &mut HistoryIndexSummary,
) -> Result<()> {
    let record = ToolCallRecord {
        thread_id: call.thread_id.clone(),
        timestamp: call.timestamp.clone(),
        rollout_path: call.rollout_path.clone(),
        call_line: call.call_line,
        output_line: call.output_line,
        tool_kind: call.tool_kind.clone(),
        tool_name: call.tool_name.clone(),
        call_id: call.call_id.clone(),
        status: call.status.clone(),
        command: call.command.clone(),
        cwd: call.cwd.clone(),
        arguments_preview: call.arguments_preview.clone(),
        arguments_bytes: call.arguments_bytes,
        output_preview: call.output_preview.clone(),
        output_bytes: call.output_bytes,
    };
    writeln!(writer, "{}", serde_json::to_string(&record)?)?;
    summary.tool_calls_indexed += 1;
    if record.output_preview.is_some() {
        summary.output_previews_indexed += 1;
    }
    Ok(())
}

fn write_command_execution_for_call(
    writer: &mut BufWriter<File>,
    call: &PartialToolCall,
    event: Option<&ExecEvent>,
    summary: &mut HistoryIndexSummary,
) -> Result<()> {
    let record = CommandExecutionRecord {
        thread_id: event
            .and_then(|event| event.thread_id.clone())
            .or_else(|| call.thread_id.clone()),
        timestamp: event
            .and_then(|event| event.timestamp.clone())
            .or_else(|| call.timestamp.clone()),
        rollout_path: call.rollout_path.clone(),
        call_line: Some(call.call_line),
        output_line: call.output_line,
        event_line: event.map(|event| event.event_line),
        call_id: call.call_id.clone(),
        execution_kind: if call.tool_name == "exec_command" {
            "shell".to_string()
        } else {
            "tool".to_string()
        },
        tool_name: call.tool_name.clone(),
        command: event
            .and_then(|event| event.command.clone())
            .or_else(|| call.command.clone()),
        command_argv: event.and_then(|event| event.command_argv.clone()),
        cwd: event
            .and_then(|event| event.cwd.clone())
            .or_else(|| call.cwd.clone()),
        status: event
            .and_then(|event| event.status.clone())
            .or_else(|| call.status.clone()),
        exit_code: event.and_then(|event| event.exit_code),
        duration_ms: event.and_then(|event| event.duration_ms),
        parsed_commands: event
            .map(|event| event.parsed_commands.clone())
            .unwrap_or_default(),
        output_preview: event
            .and_then(|event| event.output_preview.clone())
            .or_else(|| call.output_preview.clone()),
        output_bytes: event
            .and_then(|event| event.output_bytes)
            .or(call.output_bytes),
    };
    writeln!(writer, "{}", serde_json::to_string(&record)?)?;
    summary.command_executions_indexed += 1;
    Ok(())
}

fn write_command_execution_for_event(
    writer: &mut BufWriter<File>,
    event: &ExecEvent,
    summary: &mut HistoryIndexSummary,
) -> Result<()> {
    let record = CommandExecutionRecord {
        thread_id: event.thread_id.clone(),
        timestamp: event.timestamp.clone(),
        rollout_path: event.rollout_path.clone(),
        call_line: None,
        output_line: None,
        event_line: Some(event.event_line),
        call_id: event.call_id.clone(),
        execution_kind: "shell".to_string(),
        tool_name: "exec_command".to_string(),
        command: event.command.clone(),
        command_argv: event.command_argv.clone(),
        cwd: event.cwd.clone(),
        status: event.status.clone(),
        exit_code: event.exit_code,
        duration_ms: event.duration_ms,
        parsed_commands: event.parsed_commands.clone(),
        output_preview: event.output_preview.clone(),
        output_bytes: event.output_bytes,
    };
    writeln!(writer, "{}", serde_json::to_string(&record)?)?;
    summary.command_executions_indexed += 1;
    Ok(())
}

fn parse_argument_value(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => serde_json::from_str(text).ok(),
        Value::Object(_) => Some(value.clone()),
        _ => None,
    }
}

fn extract_command(
    tool_name: &str,
    parsed_args: Option<&Value>,
    raw_arguments: &str,
) -> Option<String> {
    match tool_name {
        "exec_command" => parsed_args
            .and_then(|args| args.get("cmd").and_then(Value::as_str))
            .map(ToString::to_string),
        "write_stdin" => parsed_args
            .and_then(|args| args.get("chars").and_then(Value::as_str))
            .map(|chars| format!("stdin: {}", chars.escape_debug())),
        "exec" => Some(raw_arguments.to_string()),
        _ => None,
    }
}

fn is_execution_tool(tool_name: &str) -> bool {
    matches!(tool_name, "exec_command" | "write_stdin" | "exec")
}

fn duration_ms(value: Option<&Value>) -> Option<u64> {
    let duration = value?;
    if let Some(ms) = duration.get("millis").and_then(Value::as_u64) {
        return Some(ms);
    }
    let secs = duration.get("secs").and_then(Value::as_u64).unwrap_or(0);
    let nanos = duration.get("nanos").and_then(Value::as_u64).unwrap_or(0);
    Some(secs.saturating_mul(1000) + nanos / 1_000_000)
}

fn parse_parsed_commands(value: Option<&Value>) -> Vec<ParsedCommandRecord> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            Some(ParsedCommandRecord {
                kind: item
                    .get("type")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                cmd: item
                    .get("cmd")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                path: item
                    .get("path")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
            })
        })
        .collect()
}

fn session_meta_id(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    value
        .get("payload")
        .and_then(|payload| payload.get("id"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn preview_text(text: &str, preview_chars: usize) -> (Option<String>, usize) {
    let bytes = text.len();
    if text.is_empty() {
        return (None, bytes);
    }
    let preview: String = text.chars().take(preview_chars).collect();
    let truncated = text.chars().nth(preview_chars).is_some();
    if truncated {
        (Some(format!("{preview}\n[truncated]")), bytes)
    } else {
        (Some(preview), bytes)
    }
}

fn is_rollout_file(path: &Path) -> bool {
    path.is_file()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
            .unwrap_or(false)
}

fn thread_id_from_rollout_path(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    let stem = file_name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    if stem.len() <= 20 {
        return None;
    }
    Some(stem[20..].to_string())
}

fn relative_path(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{refresh_history_index_at, HistoryIndexSummary};
    use serde_json::Value;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ucp-history-test-{}-{}-{:?}",
            std::process::id(),
            nonce,
            std::thread::current().id()
        ))
    }

    #[test]
    fn indexes_function_and_event_command_executions() {
        let root = temp_root();
        let codex = root.join(".codex");
        let sessions = codex.join("sessions").join("2026").join("06").join("14");
        let archived = codex.join("archived_sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&archived).unwrap();

        fs::write(
            sessions.join("rollout-2026-06-14T01-02-03-thread-live.jsonl"),
            concat!(
                "{\"timestamp\":\"2026-06-14T01:02:03Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-live\",\"type\":\"session_meta\"}}\n",
                "{\"timestamp\":\"2026-06-14T01:02:04Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"exec_command\",\"call_id\":\"call_exec\",\"arguments\":\"{\\\"cmd\\\":\\\"echo ok\\\",\\\"workdir\\\":\\\"/tmp/project\\\"}\"}}\n",
                "{\"timestamp\":\"2026-06-14T01:02:05Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"call_exec\",\"output\":\"hello from command\"}}\n",
                "{\"timestamp\":\"2026-06-14T01:02:06Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\",\"name\":\"apply_patch\",\"call_id\":\"call_patch\",\"input\":\"*** Begin Patch\"}}\n",
                "{\"timestamp\":\"2026-06-14T01:02:07Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call_output\",\"call_id\":\"call_patch\",\"output\":\"patch applied\"}}\n"
            ),
        )
        .unwrap();

        fs::write(
            archived.join("rollout-2026-06-13T01-02-03-thread-archived.jsonl"),
            concat!(
                "{\"timestamp\":\"2026-06-13T01:02:04Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"exec_command_end\",\"call_id\":\"call_old\",\"command\":[\"/bin/zsh\",\"-lc\",\"printf old\"],\"cwd\":\"/tmp/old\",\"aggregated_output\":\"old output\",\"exit_code\":0,\"duration\":{\"secs\":1,\"nanos\":500000000},\"status\":\"completed\",\"parsed_cmd\":[{\"type\":\"read\",\"cmd\":\"printf old\",\"name\":\"old.txt\",\"path\":\"/tmp/old.txt\"}]}}\n",
                "not json\n"
            ),
        )
        .unwrap();

        let index_dir = codex.join(".ucp_history");
        let summary = refresh_history_index_at(&codex, &index_dir, 32).unwrap();
        assert_eq!(
            summary,
            HistoryIndexSummary {
                generated_at: summary.generated_at.clone(),
                rollouts_scanned: 2,
                live_rollouts: 1,
                archived_rollouts: 1,
                json_lines_read: 6,
                malformed_lines: 1,
                tool_calls_indexed: 2,
                command_executions_indexed: 2,
                output_previews_indexed: 2,
                errors: 0,
                index_dir: ".ucp_history".to_string(),
            }
        );

        let tool_lines: Vec<Value> = fs::read_to_string(index_dir.join("tool_calls.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(tool_lines.len(), 2);
        assert_eq!(tool_lines[0]["thread_id"], "thread-live");
        assert_eq!(tool_lines[0]["command"], "echo ok");
        assert_eq!(tool_lines[1]["tool_name"], "apply_patch");

        let exec_lines: Vec<Value> = fs::read_to_string(index_dir.join("command_executions.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(exec_lines.len(), 2);
        assert_eq!(exec_lines[0]["command"], "echo ok");
        assert_eq!(exec_lines[0]["cwd"], "/tmp/project");
        assert_eq!(exec_lines[1]["command"], "printf old");
        assert_eq!(exec_lines[1]["exit_code"], 0);
        assert_eq!(exec_lines[1]["duration_ms"], 1500);

        let summary_json: HistoryIndexSummary =
            serde_json::from_str(&fs::read_to_string(index_dir.join("summary.json")).unwrap())
                .unwrap();
        assert_eq!(summary_json.command_executions_indexed, 2);

        let _ = fs::remove_dir_all(&root);
    }
}
