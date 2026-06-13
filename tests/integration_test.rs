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
            rollout_path TEXT
        )",
        [],
    )
    .expect("create threads table");
    conn.execute(
        "INSERT INTO threads (id, model_provider, model, rollout_path)
         VALUES (?1, ?2, ?3, ?4)",
        params!["thread-1", provider, model, "sessions/rollout-test.jsonl"],
    )
    .expect("insert thread row");
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
fn test_switch_preserves_rollout_jsonl_by_default_and_updates_db() {
    let home = temp_home("switch_rollout_readonly");
    write_profile(&home, "target");
    let codex = home.join(".codex");
    let sessions = codex.join("sessions");
    fs::create_dir_all(&sessions).expect("create sessions dir");
    let rollout = sessions.join("rollout-test.jsonl");
    let original_rollout = concat!(
        "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"old\",\"model\":\"gpt-5.4\"}}\n",
        "not json but should remain untouched\n",
        "{\"type\":\"function_call_output\",\"payload\":{\"output\":\"tool output stays\"}}\n"
    );
    fs::write(&rollout, original_rollout).expect("write rollout");

    let db_path = codex.join("state_5.sqlite");
    create_state_db(&db_path, "old", Some("gpt-5.4"));

    let output = Command::new(ucp_bin())
        .env("HOME", &home)
        .args(["switch", "target"])
        .output()
        .expect("Failed to run ucp switch");

    assert_success(&output);
    assert_eq!(fs::read_to_string(&rollout).unwrap(), original_rollout);
    assert_eq!(
        read_thread_model(&db_path),
        (Some("target".to_string()), Some("gpt-5.5".to_string()))
    );

    let backups = session_backup_dirs(&home);
    assert!(backups
        .iter()
        .any(|dir| dir.join("state_5.sqlite").exists()));
    assert!(!backups
        .iter()
        .any(|dir| dir.join("rollout-test.jsonl").exists()));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Rollout JSONL: left unchanged"));

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn test_sync_updates_legacy_and_new_sqlite_paths_without_rollout_rewrite() {
    let home = temp_home("sync_dual_db_readonly");
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
    let original_rollout =
        "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"old\",\"model\":\"gpt-5.4\"}}\n";
    fs::write(&rollout, original_rollout).expect("write rollout");

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
    assert_eq!(fs::read_to_string(&rollout).unwrap(), original_rollout);
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
            "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.4\",\"collaboration_mode\":{\"settings\":{\"model_provider\":\"old\",\"model\":\"gpt-5.4\"}}}}\n"
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

    let backups = session_backup_dirs(&home);
    assert!(backups
        .iter()
        .any(|dir| dir.join("rollout-test.jsonl").exists()));
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
