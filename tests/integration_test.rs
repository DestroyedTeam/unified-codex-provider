use std::path::PathBuf;
use std::process::Command;
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
