use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs};

fn ucp_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ucp"))
}

fn temp_home(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos();
    env::temp_dir().join(format!("ucp_{}_{}_{}", label, std::process::id(), nanos))
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_profile(home: &std::path::Path, name: &str) {
    let providers = home.join(".codex").join("providers");
    fs::create_dir_all(&providers).expect("create providers dir");
    fs::write(
        providers.join(format!("{name}.toml")),
        format!(
            r#"[provider]
model_provider = "{name}"
name = "{name}"
model = "gpt-5.5"
"#
        ),
    )
    .expect("write profile");
}

fn create_state_db(path: &Path, provider: &str, model: Option<&str>) {
    fs::create_dir_all(path.parent().expect("state db parent")).expect("create state db dir");
    let conn = Connection::open(path).expect("open state db");
    create_threads_fixture(&conn, provider, model);
}

fn create_state_db_with_wal(path: &Path, provider: &str, model: Option<&str>) -> Connection {
    fs::create_dir_all(path.parent().expect("state db parent")).expect("create state db dir");
    let conn = Connection::open(path).expect("open state db");
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .expect("enable WAL");
    assert_eq!(mode.to_ascii_lowercase(), "wal");
    create_threads_fixture(&conn, provider, model);
    assert!(sqlite_sidecar_path(path, "-wal").exists());
    assert!(sqlite_sidecar_path(path, "-shm").exists());
    conn
}

fn create_threads_fixture(conn: &Connection, provider: &str, model: Option<&str>) {
    conn.execute(
        "CREATE TABLE threads (
            id TEXT PRIMARY KEY,
            model_provider TEXT,
            model TEXT,
            rollout_path TEXT,
            cwd TEXT,
            archived INTEGER
        )",
        [],
    )
    .expect("create threads table");
    conn.execute(
        "INSERT INTO threads (id, model_provider, model, rollout_path, cwd, archived)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            "thread-1",
            provider,
            model,
            "sessions/rollout-test.jsonl",
            "/tmp/example-project",
            0
        ],
    )
    .expect("insert thread row");
}

fn parse_jsonl(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .expect("read jsonl")
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn read_thread_model(path: &Path) -> (Option<String>, Option<String>) {
    let conn = Connection::open(path).expect("open state db");
    conn.query_row(
        "SELECT model_provider, model FROM threads WHERE id = 'thread-1'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .expect("read thread row")
}

fn session_backup_dirs(home: &Path) -> Vec<PathBuf> {
    let codex = home.join(".codex");
    let mut dirs: Vec<PathBuf> = fs::read_dir(&codex)
        .unwrap_or_else(|_| panic!("read {}", codex.display()))
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with(".sessions_backup_"))
                    .unwrap_or(false)
        })
        .collect();
    dirs.sort();
    dirs
}

fn sqlite_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut raw = db_path.as_os_str().to_os_string();
    raw.push(suffix);
    PathBuf::from(raw)
}

#[test]
fn test_help_output() {
    let output = Command::new(ucp_bin())
        .arg("--help")
        .output()
        .expect("Failed to run ucp");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Unified Codex Provider"));
    assert!(stdout.contains("switch"));
    assert!(stdout.contains("remove"));
    assert!(stdout.contains("list"));
    assert!(stdout.contains("init"));
    assert!(stdout.contains("setup"));
    assert!(stdout.contains("doctor"));
    assert!(stdout.contains("service"));
    assert!(stdout.contains("repair-sessions"));
}

#[test]
fn test_version() {
    let output = Command::new(ucp_bin())
        .arg("--version")
        .output()
        .expect("Failed to run ucp");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ucp"));
}

#[test]
fn test_status_runs() {
    let home = temp_home("status_test");
    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .arg("status")
        .output()
        .expect("Failed to run ucp");
    let _ = fs::remove_dir_all(&home);
    // Should succeed even without prior setup
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("UnifiedCodexProvider Status"));
}

#[test]
fn test_list_runs() {
    let home = temp_home("list_test");
    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .arg("list")
        .output()
        .expect("Failed to run ucp");
    let _ = fs::remove_dir_all(&home);
    assert!(output.status.success());
}

#[test]
fn test_doctor_runs_with_empty_home() {
    let home = temp_home("doctor_test");
    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .arg("doctor")
        .output()
        .expect("Failed to run ucp doctor");
    let _ = fs::remove_dir_all(&home);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("UCP doctor"));
    assert!(stdout.contains("Codex home"));
}

#[test]
fn test_setup_no_service_creates_codex_home() {
    let home = temp_home("setup_test");
    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .args(["setup", "--no-service"])
        .output()
        .expect("Failed to run ucp setup");

    assert!(output.status.success());
    assert!(home.join(".codex").exists());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("UCP setup"));
    assert!(stdout.contains("Service: skipped"));

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn test_list_warns_about_duplicate_chatgpt_auth_snapshots() {
    let home = temp_home("duplicate_auth_test");
    write_profile(&home, "openai");
    write_profile(&home, "openai_work");
    let providers = home.join(".codex").join("providers");
    let auth = r#"{"auth_mode":"chatgpt","tokens":{"refresh_token":"same"}}"#;
    fs::write(providers.join("openai.auth.json"), auth).expect("write openai auth");
    fs::write(providers.join("openai_work.auth.json"), auth).expect("write duplicate auth");

    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .arg("list")
        .output()
        .expect("Failed to run ucp list");

    let _ = fs::remove_dir_all(&home);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ChatGPT auth snapshot reused by: openai, openai_work"));
    assert!(stdout.contains("refresh_token_reused"));
}

#[test]
fn test_switch_nonexistent_profile() {
    let home = temp_home("missing_profile_test");
    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .args(["switch", "nonexistent_provider_xyz"])
        .output()
        .expect("Failed to run ucp");
    let _ = fs::remove_dir_all(&home);
    // Should fail gracefully
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not found") || stderr.contains("Cannot load"));
}

#[test]
fn test_switch_syncs_rollout_metadata_only_by_default_and_updates_db() {
    let home = temp_home("switch_rollout_metadata_only");
    write_profile(&home, "target");
    let codex = home.join(".codex");
    let sessions = codex.join("sessions");
    fs::create_dir_all(&sessions).expect("create sessions dir");
    let rollout = sessions.join("rollout-test.jsonl");
    let event_line = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"exec_command_output\",\"stdout\":\"tool output stays\",\"collaboration_mode\":{\"settings\":{\"model\":\"gpt-5.4\"}}}}";
    fs::write(
        &rollout,
        format!(
            "{}\n{}\nnot json but should remain untouched\n{}\n",
            "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"old\",\"model\":null}}",
            "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.4\",\"collaboration_mode\":{\"settings\":{\"model\":\"gpt-5.4\",\"reasoning_effort\":\"high\"}}}}",
            event_line
        ),
    )
    .expect("write rollout");

    let original_lines: Vec<String> = fs::read_to_string(&rollout)
        .unwrap()
        .lines()
        .map(ToString::to_string)
        .collect();

    let db_path = codex.join("state_5.sqlite");
    create_state_db(&db_path, "old", Some("gpt-5.4"));

    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .args(["switch", "target"])
        .output()
        .expect("Failed to run ucp switch");

    assert_success(&output);
    let rows = parse_jsonl(&rollout);
    assert_eq!(rows[0]["payload"]["model_provider"], "target");
    assert_eq!(rows[1]["payload"]["model"], "gpt-5.5");
    assert_eq!(
        rows[1]["payload"]["collaboration_mode"]["settings"]["model"],
        "gpt-5.5"
    );
    let new_lines: Vec<String> = fs::read_to_string(&rollout)
        .unwrap()
        .lines()
        .map(ToString::to_string)
        .collect();
    assert_eq!(new_lines[2], original_lines[2]);
    assert_eq!(new_lines[3], original_lines[3]);
    assert_eq!(new_lines.len(), original_lines.len());
    assert_eq!(
        read_thread_model(&db_path),
        (Some("target".to_string()), Some("gpt-5.5".to_string()))
    );

    let backups = session_backup_dirs(&home);
    assert!(backups
        .iter()
        .any(|dir| dir.join("state_5.sqlite").exists()));
    assert!(backups
        .iter()
        .any(|dir| dir.join("sessions").join("rollout-test.jsonl").exists()));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("metadata rows only"));

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn test_switch_rebuilds_history_index_by_default_without_printing_outputs() {
    let home = temp_home("switch_history_index");
    write_profile(&home, "target");
    let codex = home.join(".codex");
    let sessions = codex.join("sessions").join("2026").join("06").join("14");
    let archived = codex.join("archived_sessions");
    fs::create_dir_all(&sessions).expect("create sessions dir");
    fs::create_dir_all(&archived).expect("create archived sessions dir");

    let hidden_output = "captured output stays in the index, not stdout";
    let live_rollout = sessions.join("rollout-2026-06-14T01-02-03-thread-live.jsonl");
    fs::write(
        &live_rollout,
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            "{\"timestamp\":\"2026-06-14T01:02:03Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-live\",\"model_provider\":\"old\",\"type\":\"session_meta\"}}",
            "{\"timestamp\":\"2026-06-14T01:02:04Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"exec_command\",\"call_id\":\"call_exec\",\"arguments\":\"{\\\"cmd\\\":\\\"echo ok\\\",\\\"workdir\\\":\\\"/tmp/project\\\"}\"}}",
            format!(
                "{{\"timestamp\":\"2026-06-14T01:02:05Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"function_call_output\",\"call_id\":\"call_exec\",\"output\":\"{}\"}}}}",
                hidden_output
            ),
            "{\"timestamp\":\"2026-06-14T01:02:06Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\",\"name\":\"apply_patch\",\"call_id\":\"call_patch\",\"input\":\"*** Begin Patch\"}}",
            "{\"timestamp\":\"2026-06-14T01:02:07Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"mcp_tool_call_end\",\"call_id\":\"call_mcp\",\"invocation\":{\"server\":\"codex_app\",\"tool\":\"read_thread_terminal\",\"arguments\":{\"limit\":10}},\"result\":{\"Ok\":\"terminal output\"}}}",
            "{\"timestamp\":\"2026-06-14T01:02:08Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"dynamic_tool_call_request\",\"callId\":\"call_dynamic\",\"namespace\":\"codex_app\",\"tool\":\"load_workspace_dependencies\",\"arguments\":{\"include\":\"runtime\"}}}",
            "{\"timestamp\":\"2026-06-14T01:02:09Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"dynamic_tool_call_response\",\"call_id\":\"call_dynamic\",\"tool\":\"load_workspace_dependencies\",\"success\":true,\"content_items\":[{\"text\":\"deps loaded\"}]}}"
        ),
    )
    .expect("write live rollout");

    fs::write(
        archived.join("rollout-2026-06-13T01-02-03-thread-archived.jsonl"),
        concat!(
            "{\"timestamp\":\"2026-06-13T01:02:04Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"exec_command_end\",\"call_id\":\"call_old\",\"command\":[\"/bin/zsh\",\"-lc\",\"printf old\"],\"cwd\":\"/tmp/old\",\"aggregated_output\":\"old output\",\"exit_code\":0,\"duration\":{\"secs\":1,\"nanos\":500000000},\"status\":\"completed\"}}\n",
            "not json\n"
        ),
    )
    .expect("write archived rollout");

    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .args(["switch", "target"])
        .output()
        .expect("Failed to run ucp switch");

    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("History index: 2 rollout(s), 4 tool call(s), 2 command execution(s)"));
    assert!(!stdout.contains(hidden_output));

    let index_dir = codex.join(".ucp_history");
    let summary: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(index_dir.join("summary.json")).unwrap())
            .expect("parse history summary");
    assert_eq!(summary["rollouts_scanned"], 2);
    assert_eq!(summary["live_rollouts"], 1);
    assert_eq!(summary["archived_rollouts"], 1);
    assert_eq!(summary["tool_calls_indexed"], 4);
    assert_eq!(summary["command_executions_indexed"], 2);

    let tool_calls = parse_jsonl(&index_dir.join("tool_calls.jsonl"));
    assert_eq!(tool_calls.len(), 4);
    assert_eq!(tool_calls[0]["thread_id"], "thread-live");
    assert_eq!(tool_calls[0]["command"], "echo ok");
    assert_eq!(tool_calls[1]["tool_name"], "apply_patch");
    assert!(tool_calls
        .iter()
        .any(|call| call["tool_name"] == "codex_app.read_thread_terminal"
            && call["status"] == "completed"));
    assert!(tool_calls.iter().any(|call| call["tool_name"]
        == "codex_app.load_workspace_dependencies"
        && call["output_preview"]
            .as_str()
            .is_some_and(|preview| preview.contains("deps loaded"))));

    let live_rows = parse_jsonl(&live_rollout);
    assert!(live_rows
        .iter()
        .all(|row| row["payload"]["ucp_display_projection"] != true));
    assert!(live_rows
        .iter()
        .any(|row| row["type"] == "event_msg" && row["payload"]["type"] == "mcp_tool_call_end"));

    let commands = parse_jsonl(&index_dir.join("command_executions.jsonl"));
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0]["command"], "echo ok");
    assert_eq!(commands[0]["cwd"], "/tmp/project");
    assert_eq!(commands[0]["output_preview"], hidden_output);
    assert_eq!(commands[1]["command"], "printf old");
    assert_eq!(commands[1]["exit_code"], 0);
    assert_eq!(commands[1]["duration_ms"], 1500);

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn test_repair_sessions_dry_run_then_apply_with_backup() {
    let home = temp_home("repair_sessions");
    let sessions = home.join(".codex").join("sessions");
    fs::create_dir_all(&sessions).expect("create sessions dir");
    let rollout = sessions.join("rollout-incompatible.jsonl");
    let original = concat!(
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\",\"name\":\"computer-use.click\",\"call_id\":\"projection-call\",\"input\":\"{}\",\"ucp_display_projection\":true}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call_output\",\"call_id\":\"projection-call\",\"output\":\"ok\",\"ucp_display_projection\":true}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\",\"name\":\"node_repl.js\",\"call_id\":\"native-call\",\"input\":\"{}\"}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call_output\",\"call_id\":\"native-call\",\"output\":\"ok\"}}\n"
    );
    fs::write(&rollout, original).expect("write incompatible rollout");

    let dry_run = Command::new(ucp_bin())
        .env("HOME", &home)
        .arg("repair-sessions")
        .output()
        .expect("run repair-sessions dry-run");
    assert_success(&dry_run);
    let dry_stdout = String::from_utf8_lossy(&dry_run.stdout);
    assert!(dry_stdout.contains("1 affected, 0 repaired"));
    assert!(dry_stdout.contains("Dry-run only"));
    assert_eq!(fs::read_to_string(&rollout).unwrap(), original);
    assert!(session_backup_dirs(&home).is_empty());

    let applied = Command::new(ucp_bin())
        .env("HOME", &home)
        .args(["repair-sessions", "--apply"])
        .output()
        .expect("run repair-sessions apply");
    assert_success(&applied);
    let applied_stdout = String::from_utf8_lossy(&applied.stdout);
    assert!(applied_stdout.contains("1 affected, 1 repaired"));
    assert!(applied_stdout.contains("2 UCP display projection(s) removed"));
    assert!(applied_stdout.contains("1 invalid tool name(s) normalized"));

    let repaired = fs::read_to_string(&rollout).unwrap();
    assert!(!repaired.contains("ucp_display_projection"));
    assert!(repaired.contains("\"name\":\"node_repl_js\""));
    assert!(repaired.contains("\"call_id\":\"native-call\""));

    let backups = session_backup_dirs(&home);
    assert_eq!(backups.len(), 1);
    assert_eq!(
        fs::read_to_string(
            backups[0]
                .join("sessions")
                .join("rollout-incompatible.jsonl")
        )
        .unwrap(),
        original
    );

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn test_sync_updates_dual_sqlite_paths_and_live_archived_rollout_metadata() {
    let home = temp_home("sync_dual_db_metadata");
    write_profile(&home, "target");
    let codex = home.join(".codex");
    fs::write(
        codex.join("config.toml"),
        "model_provider = \"target\"\nmodel = \"gpt-5.5\"\n",
    )
    .expect("write config");

    let sessions = codex.join("sessions");
    fs::create_dir_all(&sessions).expect("create sessions dir");
    let rollout = sessions.join("rollout-test.jsonl");
    fs::write(
        &rollout,
        "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"old\",\"model\":\"gpt-5.4\"}}\n",
    )
    .expect("write rollout");
    let archived = codex.join("archived_sessions");
    fs::create_dir_all(&archived).expect("create archived sessions dir");
    let archived_rollout = archived.join("rollout-archived.jsonl");
    fs::write(
        &archived_rollout,
        "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"old\",\"model\":\"gpt-5.4\"}}\n",
    )
    .expect("write archived rollout");

    let legacy_db = codex.join("state_5.sqlite");
    let new_db = codex.join("sqlite").join("state_5.sqlite");
    create_state_db(&legacy_db, "old", Some("gpt-5.4"));
    create_state_db(&new_db, "old", Some("gpt-5.4"));

    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .arg("sync")
        .output()
        .expect("Failed to run ucp sync");

    assert_success(&output);
    assert_eq!(
        parse_jsonl(&rollout)[0]["payload"]["model_provider"],
        "target"
    );
    assert_eq!(
        parse_jsonl(&archived_rollout)[0]["payload"]["model_provider"],
        "target"
    );
    assert_eq!(
        read_thread_model(&legacy_db),
        (Some("target".to_string()), Some("gpt-5.5".to_string()))
    );
    assert_eq!(
        read_thread_model(&new_db),
        (Some("target".to_string()), Some("gpt-5.5".to_string()))
    );

    let backups = session_backup_dirs(&home);
    assert!(backups
        .iter()
        .any(|dir| dir.join("state_5.sqlite").exists()));
    assert!(backups
        .iter()
        .any(|dir| dir.join("sqlite").join("state_5.sqlite").exists()));
    assert!(backups
        .iter()
        .any(|dir| dir.join("sessions").join("rollout-test.jsonl").exists()));
    assert!(backups.iter().any(|dir| dir
        .join("archived_sessions")
        .join("rollout-archived.jsonl")
        .exists()));

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn test_sync_rewrites_rollout_jsonl_only_with_explicit_flag() {
    let home = temp_home("sync_rewrite_rollouts");
    write_profile(&home, "target");
    let codex = home.join(".codex");
    fs::write(
        codex.join("config.toml"),
        "model_provider = \"target\"\nmodel = \"gpt-5.5\"\n",
    )
    .expect("write config");

    let sessions = codex.join("sessions");
    fs::create_dir_all(&sessions).expect("create sessions dir");
    let rollout = sessions.join("rollout-test.jsonl");
    fs::write(
        &rollout,
        concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"old\",\"model\":\"gpt-5.4\"}}\n",
            "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.4\",\"collaboration_mode\":{\"settings\":{\"model_provider\":\"old\",\"model\":\"gpt-5.4\"}}}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"collaboration_mode\":{\"settings\":{\"model\":\"gpt-5.4\"}}}}\n"
        ),
    )
    .expect("write rollout");

    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .args(["sync", "--rewrite-rollouts"])
        .output()
        .expect("Failed to run ucp sync --rewrite-rollouts");

    assert_success(&output);
    let rewritten = fs::read_to_string(&rollout).unwrap();
    assert!(rewritten.contains("\"model_provider\":\"target\""));
    assert!(rewritten.contains("\"model\":\"gpt-5.5\""));
    assert!(!rewritten.contains("\"model_provider\":\"old\""));
    assert!(!rewritten.contains("\"model\":\"gpt-5.4\""));

    let backups = session_backup_dirs(&home);
    assert!(backups
        .iter()
        .any(|dir| dir.join("sessions").join("rollout-test.jsonl").exists()));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--rewrite-rollouts is enabled"));

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn test_sqlite_backup_includes_wal_shm_and_prunes_old_backups() {
    let home = temp_home("sqlite_backup_prune");
    write_profile(&home, "target");
    let codex = home.join(".codex");
    fs::write(
        codex.join("config.toml"),
        "model_provider = \"target\"\nmodel = \"gpt-5.5\"\n",
    )
    .expect("write config");

    for day in 1..=4 {
        fs::create_dir_all(codex.join(format!(".sessions_backup_2000010{day}_000000")))
            .expect("create old backup dir");
    }

    let db_path = codex.join("state_5.sqlite");
    let wal_conn = create_state_db_with_wal(&db_path, "old", Some("gpt-5.4"));

    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .arg("sync")
        .output()
        .expect("Failed to run ucp sync");

    assert_success(&output);
    let backups = session_backup_dirs(&home);
    assert!(backups.len() <= 3);
    assert!(!codex.join(".sessions_backup_20000101_000000").exists());
    assert!(backups.iter().any(|dir| dir.join("state_5.sqlite").exists()
        && dir.join("state_5.sqlite-wal").exists()
        && dir.join("state_5.sqlite-shm").exists()));

    drop(wal_conn);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn test_profile_completion_filters_by_prefix() {
    let home = temp_home("completion_test");
    let providers = home.join(".codex").join("providers");
    fs::create_dir_all(&providers).expect("create providers dir");
    fs::write(
        providers.join("openai_team_1.toml"),
        r#"[provider]
model_provider = "openai_team_1"
name = "OpenAI Free"
model = "gpt-5.5"
"#,
    )
    .expect("write openai profile");
    fs::write(providers.join("switch.toml"), "not valid toml =")
        .expect("write malformed switch profile");

    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .args(["__complete", "profile", "openai_"])
        .output()
        .expect("Failed to run completion helper");

    let _ = fs::remove_dir_all(&home);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("openai_team_1"));
    assert!(!stdout.contains("switch"));
}

#[test]
fn test_remove_deletes_profile_and_auth_snapshot() {
    let home = temp_home("remove_test");
    write_profile(&home, "old_profile");
    let auth_snapshot = home
        .join(".codex")
        .join("providers")
        .join("old_profile.auth.json");
    fs::write(&auth_snapshot, "{}\n").expect("write auth snapshot");

    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .args(["remove", "old_profile"])
        .output()
        .expect("Failed to run remove");

    assert!(output.status.success());
    assert!(!home
        .join(".codex")
        .join("providers")
        .join("old_profile.toml")
        .exists());
    assert!(!auth_snapshot.exists());

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn test_delete_alias_keeps_auth_snapshot() {
    let home = temp_home("delete_alias_test");
    write_profile(&home, "old_profile");
    let auth_snapshot = home
        .join(".codex")
        .join("providers")
        .join("old_profile.auth.json");
    fs::write(&auth_snapshot, "{}\n").expect("write auth snapshot");

    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .args(["delete", "old_profile", "--keep-auth"])
        .output()
        .expect("Failed to run delete alias");

    assert!(output.status.success());
    assert!(!home
        .join(".codex")
        .join("providers")
        .join("old_profile.toml")
        .exists());
    assert!(auth_snapshot.exists());

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn test_remove_refuses_active_profile_without_force() {
    let home = temp_home("remove_active_test");
    write_profile(&home, "active_profile");
    fs::write(
        home.join(".codex").join(".ucp_state.json"),
        r#"{"last_provider":"active_profile","last_profile_name":"active_profile","last_sync":"2026-05-27T00:00:00+08:00"}"#,
    )
    .expect("write state");

    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .args(["remove", "active_profile"])
        .output()
        .expect("Failed to run remove");

    assert!(!output.status.success());
    assert!(home
        .join(".codex")
        .join("providers")
        .join("active_profile.toml")
        .exists());

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn test_zsh_completion_mentions_dynamic_profiles() {
    let output = Command::new(ucp_bin())
        .args(["completions", "zsh"])
        .output()
        .expect("Failed to generate zsh completion");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("#compdef ucp"));
    assert!(stdout.contains("ucp __complete profile"));
}
