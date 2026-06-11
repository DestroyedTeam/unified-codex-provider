use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::config::codex_dir;
use crate::provider::{providers_dir, ProviderProfile};

/// Get the auth.json path
pub fn auth_json_path() -> PathBuf {
    codex_dir().join("auth.json")
}

/// Get the raw auth snapshot path for a provider profile.
/// For providers with complex auth (like openai chatgpt login with tokens),
/// we store the full JSON as-is rather than trying to flatten it into TOML.
pub fn auth_snapshot_path(profile_name: &str) -> PathBuf {
    providers_dir().join(format!("{}.auth.json", profile_name))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateAuthSnapshot {
    pub profiles: Vec<String>,
}

/// Read the current auth.json as raw JSON
#[allow(dead_code)]
pub fn read_auth() -> Result<HashMap<String, Value>> {
    let path = auth_json_path();
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = fs::read_to_string(&path).context("Failed to read auth.json")?;
    let data: HashMap<String, Value> =
        serde_json::from_str(&content).context("Failed to parse auth.json")?;
    Ok(data)
}

/// Write auth.json for a provider switch.
///
/// Strategy:
/// 1. If a raw auth snapshot exists (providers/{name}.auth.json), use it directly
/// 2. Otherwise, build from the profile's [auth] section (simple key-value)
pub fn write_auth(profile: &ProviderProfile, profile_name: &str) -> Result<()> {
    let path = auth_json_path();
    let snapshot = auth_snapshot_path(profile_name);

    // Backup existing
    if path.exists() {
        let backup = codex_dir().join("auth.json.bak");
        fs::copy(&path, &backup)?;
    }

    if snapshot.exists() {
        // Use the raw snapshot directly
        fs::copy(&snapshot, &path)?;
    } else {
        // Build from profile's auth map
        let auth_map: HashMap<String, Value> = profile
            .auth
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();

        let content = serde_json::to_string_pretty(&auth_map)? + "\n";
        fs::write(&path, &content)?;
    }

    // Set restrictive permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

/// Save the current auth.json as a snapshot for the given profile.
/// Called during `init` migration or when capturing current state.
#[allow(dead_code)]
pub fn save_auth_snapshot(profile_name: &str) -> Result<()> {
    let src = auth_json_path();
    if !src.exists() {
        return Ok(());
    }
    let dst = auth_snapshot_path(profile_name);
    fs::copy(&src, &dst)?;
    Ok(())
}

/// Save a specific auth file as a snapshot for the given profile.
pub fn save_auth_snapshot_from(source_path: &std::path::Path, profile_name: &str) -> Result<()> {
    if !source_path.exists() {
        return Ok(());
    }
    let dst = auth_snapshot_path(profile_name);
    fs::create_dir_all(providers_dir())?;
    fs::copy(source_path, &dst)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dst, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Find ChatGPT auth snapshots that are byte-for-byte identical across profiles.
///
/// ChatGPT refresh tokens rotate: restoring the same snapshot for multiple
/// account profiles can make Codex reuse an already-spent refresh token.
pub fn find_duplicate_chatgpt_auth_snapshots() -> Result<Vec<DuplicateAuthSnapshot>> {
    let mut snapshots: HashMap<Vec<u8>, Vec<String>> = HashMap::new();

    for (name, _) in crate::provider::list_providers()? {
        let path = auth_snapshot_path(&name);
        if !path.exists() {
            continue;
        }

        let content = fs::read(&path)
            .with_context(|| format!("Failed to read auth snapshot: {}", path.display()))?;
        if !is_chatgpt_auth_snapshot(&content) {
            continue;
        }

        snapshots.entry(content).or_default().push(name);
    }

    let mut duplicates: Vec<DuplicateAuthSnapshot> = snapshots
        .into_values()
        .filter_map(|mut profiles| {
            if profiles.len() < 2 {
                return None;
            }
            profiles.sort();
            Some(DuplicateAuthSnapshot { profiles })
        })
        .collect();
    duplicates.sort_by(|a, b| a.profiles.cmp(&b.profiles));
    Ok(duplicates)
}

pub fn print_duplicate_chatgpt_auth_warnings() -> Result<()> {
    let duplicates = find_duplicate_chatgpt_auth_snapshots()?;
    if duplicates.is_empty() {
        return Ok(());
    }

    println!("\nWarnings:");
    for duplicate in duplicates {
        println!(
            "  ⚠ ChatGPT auth snapshot reused by: {}",
            duplicate.profiles.join(", ")
        );
    }
    println!("  Re-login or import a distinct auth snapshot before switching these profiles.");
    println!("  Duplicate OAuth refresh tokens can fail with refresh_token_reused.");
    Ok(())
}

fn is_chatgpt_auth_snapshot(content: &[u8]) -> bool {
    serde_json::from_slice::<Value>(content)
        .ok()
        .and_then(|value| {
            value
                .get("auth_mode")
                .and_then(Value::as_str)
                .map(|mode| mode == "chatgpt")
        })
        .unwrap_or(false)
}

/// Detect which provider the current auth.json likely belongs to
#[allow(dead_code)]
pub fn detect_auth_provider() -> Result<Option<String>> {
    let auth = read_auth()?;

    if let Some(Value::String(mode)) = auth.get("auth_mode") {
        if mode == "chatgpt" {
            return Ok(Some("openai".to_string()));
        }
    }

    Ok(None)
}
