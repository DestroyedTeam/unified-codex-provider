use anyhow::{Context, Result};
use chrono::Local;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{auth, config, migrate, provider, service, sync};

pub struct SetupOptions {
    pub install_service: bool,
}

pub fn run_setup(options: SetupOptions) -> Result<()> {
    println!("UCP setup");
    println!("---------");

    let codex_dir = config::codex_dir();
    fs::create_dir_all(&codex_dir)?;
    println!("Codex home: {}", codex_dir.display());

    if has_legacy_configs(&codex_dir)? && provider::list_profile_names()?.is_empty() {
        let backup = backup_codex_config(&codex_dir)?;
        println!("Backup: {}", backup.display());
        migrate::init_migrate()?;
    } else {
        println!("Migration: skipped");
    }

    if options.install_service {
        if cfg!(target_os = "macos") {
            service::install_launch_agent()?;
        } else {
            println!("Service: skipped (macOS LaunchAgent only)");
        }
    } else {
        println!("Service: skipped (--no-service)");
    }

    println!();
    run_doctor()?;
    Ok(())
}

pub fn run_doctor() -> Result<()> {
    let codex_dir = config::codex_dir();
    let profiles = provider::list_profile_names().unwrap_or_default();
    let state = sync::load_state();
    let current_provider = config::read_current_model_provider().unwrap_or(None);
    let duplicate_auth = auth::find_duplicate_chatgpt_auth_snapshots().unwrap_or_default();

    println!("UCP doctor");
    println!("----------");
    print_check("ucp binary", &current_exe_label());
    print_check(
        "Codex CLI",
        command_in_path("codex")
            .as_deref()
            .unwrap_or("not found in PATH"),
    );
    print_check("Codex home", &path_status(&codex_dir));
    print_check("config.toml", &path_status(&config::config_toml_path()));
    print_check("auth.json", &path_status(&auth::auth_json_path()));
    print_check("profiles", &format!("{}", profiles.len()));
    print_check(
        "active provider",
        current_provider.as_deref().unwrap_or("(none)"),
    );
    print_check(
        "state profile",
        state.last_profile_name.as_deref().unwrap_or("(unknown)"),
    );

    if cfg!(target_os = "macos") {
        let installed = service::launch_agent_installed().unwrap_or(false);
        print_check(
            "LaunchAgent",
            if installed {
                "installed"
            } else {
                "not installed"
            },
        );
    } else {
        print_check("LaunchAgent", "not supported on this platform");
    }

    if duplicate_auth.is_empty() {
        print_check("duplicate ChatGPT auth", "none");
    } else {
        for duplicate in duplicate_auth {
            print_check(
                "duplicate ChatGPT auth",
                &format!("profiles: {}", duplicate.profiles.join(", ")),
            );
        }
    }

    Ok(())
}

fn has_legacy_configs(codex_dir: &Path) -> Result<bool> {
    if !codex_dir.exists() {
        return Ok(false);
    }

    for entry in fs::read_dir(codex_dir)? {
        let name = entry?.file_name().to_string_lossy().to_string();
        if name.starts_with("config_") && name.ends_with(".toml") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn backup_codex_config(codex_dir: &Path) -> Result<PathBuf> {
    let stamp = Local::now().format("%Y%m%d%H%M%S");
    let backup_dir = codex_dir.join(format!(".ucp_setup_backup_{stamp}"));
    fs::create_dir_all(&backup_dir)?;

    for entry in fs::read_dir(codex_dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "sessions"
            || name == "archived_sessions"
            || name.starts_with(".ucp_setup_backup_")
        {
            continue;
        }

        let dest = backup_dir.join(&name);
        if path.is_dir() {
            copy_dir_all(&path, &dest)?;
        } else if path.is_file() {
            fs::copy(&path, &dest).with_context(|| format!("Cannot back up {}", path.display()))?;
        }
    }

    Ok(backup_dir)
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)
                .with_context(|| format!("Cannot back up {}", src_path.display()))?;
        }
    }
    Ok(())
}

fn print_check(name: &str, value: &str) {
    println!("{:<24} {}", name, value);
}

fn current_exe_label() -> String {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "(unknown)".to_string())
}

fn path_status(path: &Path) -> String {
    if path.exists() {
        path.display().to_string()
    } else {
        format!("missing ({})", path.display())
    }
}

fn command_in_path(command: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(command);
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }

    let output = Command::new(command).arg("--version").output().ok()?;
    if output.status.success() {
        Some(command.to_string())
    } else {
        None
    }
}
