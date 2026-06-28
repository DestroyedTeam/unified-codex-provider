use anyhow::{Context, Result};
use chrono::Local;
use std::fs;

use crate::auth;
use crate::config;
use crate::provider;
use crate::sessions;

/// State file to track last sync
fn state_file_path() -> std::path::PathBuf {
    config::codex_dir().join(".ucp_state.json")
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct SyncState {
    pub last_provider: Option<String>,
    pub last_sync: Option<String>,
    pub last_profile_name: Option<String>,
}

pub fn load_state() -> SyncState {
    let path = state_file_path();
    if !path.exists() {
        return SyncState::default();
    }
    fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

pub fn save_state(provider: &str, profile_name: &str) -> Result<()> {
    let state = SyncState {
        last_provider: Some(provider.to_string()),
        last_sync: Some(Local::now().to_rfc3339()),
        last_profile_name: Some(profile_name.to_string()),
    };
    let content = serde_json::to_string_pretty(&state)?;
    fs::write(state_file_path(), content)?;
    Ok(())
}

pub fn clear_state() -> Result<()> {
    let path = state_file_path();
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Full switch operation: config + auth + sessions
///
/// Before switching away, captures the current state:
/// 1. Updates common.toml from current config.toml (preserves new projects, plugins, etc.)
/// 2. Updates the current provider's auth snapshot (preserves refreshed tokens)
pub fn switch_provider(profile_name: &str, rewrite_rollouts: bool) -> Result<()> {
    let profile = provider::load_profile_by_name(profile_name)
        .with_context(|| format!("Cannot load profile '{}'", profile_name))?;

    println!(
        "Switching to provider: {} ({})",
        profile_name, profile.provider.name
    );

    // === CAPTURE CURRENT STATE BEFORE SWITCHING ===
    capture_current_state()?;

    // 1. Merge and write config.toml
    println!("  Writing config.toml...");
    config::merge_and_write_config(&profile)?;

    // 2. Write auth.json
    println!("  Writing auth.json...");
    auth::write_auth(&profile, profile_name)?;

    // 3. Unify session visibility with metadata-only rollout updates by default.
    println!("  Syncing session visibility...");
    let session_summary = sessions::unify_sessions(
        &profile.provider.model_provider,
        &profile.provider.model,
        sessions::SessionSyncOptions {
            full_rollout_rewrite: rewrite_rollouts,
        },
    )?;
    println!(
        "  Rollouts: {} modified, {} skipped, {} metadata lines, {} errors; DB: {} rows, {} errors",
        session_summary.rollouts_modified,
        session_summary.rollouts_skipped,
        session_summary.metadata_lines_updated,
        session_summary.rollout_errors,
        session_summary.db_records_updated,
        session_summary.db_errors
    );

    // 4. Save state
    save_state(&profile.provider.model_provider, profile_name)?;

    println!("✓ Switch complete: now using '{}'", profile_name);
    Ok(())
}

/// Capture the current state before switching away:
/// - Update common.toml from current config.toml (new projects, plugins, mcp_servers)
/// - Update the current provider's auth snapshot (refreshed tokens)
fn capture_current_state() -> Result<()> {
    let state = load_state();

    // Update auth snapshot for the current provider
    if let Some(ref current_profile_name) = state.last_profile_name {
        let auth_path = auth::auth_json_path();
        if auth_path.exists() {
            auth::save_auth_snapshot_from(&auth_path, current_profile_name)?;
            println!("  Captured auth snapshot for '{}'", current_profile_name);
        }
    }

    // Update common.toml from current config.toml
    let config_path = config::config_toml_path();
    if config_path.exists() {
        let current_content = fs::read_to_string(&config_path)?;
        let new_common = config::extract_common_config(&current_content)?;
        let common_path = config::common_toml_path();
        fs::write(&common_path, &new_common)?;
        println!("  Updated common.toml from current config");
    }

    Ok(())
}

/// Auto-sync: triggered by LaunchAgent when auth.json or config.toml changes.
///
/// Key logic:
/// - If provider hasn't changed (same model_provider as last sync):
///   → auth.json changed = token refresh → UPDATE the snapshot (not overwrite auth)
///   → config.toml changed = Codex added projects/plugins → UPDATE common.toml
///   → stale session database rows from an older failed sync → RECONCILE indexes
/// - If provider changed (different model_provider):
///   → Full switch detected externally → update session visibility indexes to match
pub fn auto_sync() -> Result<()> {
    auto_sync_with_options(false)
}

pub fn auto_sync_with_options(rewrite_rollouts: bool) -> Result<()> {
    let current_provider = config::read_current_model_provider()?;
    let current_provider = match current_provider {
        Some(p) => p,
        None => {
            println!("No model_provider found in config.toml, nothing to sync.");
            return Ok(());
        }
    };

    let state = load_state();

    // Case 1: Provider hasn't changed — this is a token refresh or config update
    if state.last_provider.as_deref() == Some(&current_provider) {
        let profile_name = state.last_profile_name.as_deref().unwrap_or("unknown");
        println!(
            "Provider unchanged ({}), capturing updates...",
            profile_name
        );

        // Update auth snapshot (token refresh)
        let auth_path = auth::auth_json_path();
        if auth_path.exists() {
            auth::save_auth_snapshot_from(&auth_path, profile_name)?;
        }

        // Update common.toml (new projects, plugins, etc.)
        let config_path = config::config_toml_path();
        if config_path.exists() {
            let content = fs::read_to_string(&config_path)?;
            let new_common = config::extract_common_config(&content)?;
            fs::write(config::common_toml_path(), &new_common)?;
        }

        // Reconcile the SQLite session index even when provider is unchanged.
        // Rollout JSONL receives metadata-only updates unless explicitly requested.
        let matched_profile = provider::load_profile_by_name(profile_name)
            .ok()
            .or_else(|| {
                provider::identify_current_provider(&current_provider)
                    .ok()
                    .flatten()
                    .map(|(_, profile)| profile)
            });
        if let Some(profile) = matched_profile {
            let summary = sessions::unify_sessions(
                &current_provider,
                &profile.provider.model,
                sessions::SessionSyncOptions {
                    full_rollout_rewrite: rewrite_rollouts,
                },
            )?;
            if summary.rollouts_modified > 0
                || summary.rollout_errors > 0
                || summary.db_records_updated > 0
                || summary.db_errors > 0
            {
                println!(
                    "  Sessions: {} rollout(s), {} metadata line(s), {} DB row(s), {} error(s)",
                    summary.rollouts_modified,
                    summary.metadata_lines_updated,
                    summary.db_records_updated,
                    summary.rollout_errors + summary.db_errors
                );
            }
        } else {
            println!(
                "Warning: could not resolve profile for '{}'; skipped session reconcile.",
                current_provider
            );
        }

        // Update last_sync timestamp
        save_state(&current_provider, profile_name)?;
        println!("✓ Snapshots updated for '{}'", profile_name);
        return Ok(());
    }

    // Case 2: Provider changed — someone switched externally (e.g., cc-switch)
    println!(
        "Provider changed: {:?} → {}",
        state.last_provider, current_provider
    );

    // First, save the OLD provider's auth snapshot if we know who it was
    // (already done if they used `ucp switch`, but handle external switches)

    // Find matching profile for the NEW provider
    let matched = provider::identify_current_provider(&current_provider)?;
    let (profile_name, matched_profile) = match matched {
        Some(m) => m,
        None => {
            println!(
                "Warning: model_provider '{}' does not match any registered profile.",
                current_provider
            );
            println!("Run 'ucp add' to register it, or 'ucp init' to migrate.");
            return Ok(());
        }
    };

    // Update common.toml from the new config
    let config_path = config::config_toml_path();
    if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        let new_common = config::extract_common_config(&content)?;
        fs::write(config::common_toml_path(), &new_common)?;
    }

    // Save the new auth as snapshot
    let auth_path = auth::auth_json_path();
    if auth_path.exists() {
        auth::save_auth_snapshot_from(&auth_path, &profile_name)?;
    }

    // Unify session visibility to the new provider.
    println!("  Syncing session visibility to '{}'...", profile_name);
    let target_model = &matched_profile.provider.model;
    let summary = sessions::unify_sessions(
        &current_provider,
        target_model,
        sessions::SessionSyncOptions {
            full_rollout_rewrite: rewrite_rollouts,
        },
    )?;
    if summary.rollouts_modified > 0
        || summary.rollout_errors > 0
        || summary.db_records_updated > 0
        || summary.db_errors > 0
    {
        println!(
            "  Sessions: {} rollout(s), {} metadata line(s), {} DB row(s), {} error(s)",
            summary.rollouts_modified,
            summary.metadata_lines_updated,
            summary.db_records_updated,
            summary.rollout_errors + summary.db_errors
        );
    }

    save_state(&current_provider, &profile_name)?;
    println!("✓ Sync complete for '{}'", profile_name);
    Ok(())
}

/// Manual sync: force full sync based on current config.toml
pub fn manual_sync(rewrite_rollouts: bool) -> Result<()> {
    println!("Running manual sync...");
    auto_sync_with_options(rewrite_rollouts)
}

/// Show current status
pub fn show_status() -> Result<()> {
    let current_provider = config::read_current_model_provider()?;
    let state = load_state();

    println!("┌─────────────────────────────────────────┐");
    println!("│ UnifiedCodexProvider Status              │");
    println!("├─────────────────────────────────────────┤");

    match &current_provider {
        Some(p) => println!("│ Active provider: {:<22} │", p),
        None => println!("│ Active provider: (none)                 │"),
    };

    match &state.last_profile_name {
        Some(n) => println!("│ Profile name:    {:<22} │", n),
        None => println!("│ Profile name:    (unknown)              │"),
    };

    match &state.last_sync {
        Some(t) => {
            let short = if t.len() > 19 { &t[..19] } else { t };
            println!("│ Last sync:       {:<22} │", short);
        }
        None => println!("│ Last sync:       (never)                │"),
    };

    // Consistency check
    let consistent = match (&current_provider, &state.last_provider) {
        (Some(cp), Some(lp)) => cp == lp,
        _ => false,
    };
    let status = if consistent {
        "✓ consistent"
    } else {
        "⚠ inconsistent"
    };
    println!("│ State:           {:<22} │", status);
    println!("└─────────────────────────────────────────┘");

    if !consistent {
        println!("\nRun 'ucp sync' to fix inconsistency.");
    }

    auth::print_duplicate_chatgpt_auth_warnings()?;

    Ok(())
}
