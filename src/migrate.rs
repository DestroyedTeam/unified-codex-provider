use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use toml_edit::DocumentMut;

use crate::auth;
use crate::config::{codex_dir, common_toml_path, config_toml_path, extract_common_config};
use crate::provider::{providers_dir, ProviderConfig, ProviderProfile};

struct MigrationSource {
    config_path: String,
    auth_path: String,
    profile_name: String,
}

/// Initialize from existing scattered config files.
/// Uses the CURRENT config.toml as the source of truth for common.toml,
/// then creates provider profiles from all config_*.toml files.
pub fn init_migrate() -> Result<()> {
    let dir = codex_dir();

    let sources = discover_sources(&dir)?;
    if sources.is_empty() {
        println!("No config_*.toml files found to migrate.");
        return Ok(());
    }

    println!("Discovered {} provider configurations:", sources.len());
    for s in &sources {
        let auth_info = if s.auth_path.is_empty() {
            "(no auth)"
        } else {
            &s.auth_path
        };
        println!(
            "  - {} (config: {}, auth: {})",
            s.profile_name, s.config_path, auth_info
        );
    }

    // Use the CURRENT config.toml as the base for common.toml
    let current_config_path = config_toml_path();
    let base_source = if current_config_path.exists() {
        println!("\nUsing current config.toml as base for common.toml");
        fs::read_to_string(&current_config_path)?
    } else {
        let best = find_richest_config(&sources)?;
        println!("\nUsing '{}' as base for common.toml", best);
        fs::read_to_string(dir.join(&best))?
    };

    let common_content = extract_common_config(&base_source)?;

    let common_path = common_toml_path();
    if common_path.exists() {
        let backup = dir.join("common.toml.bak");
        fs::copy(&common_path, &backup)?;
        println!("  Backed up existing common.toml");
    }
    fs::write(&common_path, &common_content)?;
    println!("  Written common.toml");

    // Create provider profiles
    let profiles_dir = providers_dir();
    fs::create_dir_all(&profiles_dir)?;

    for source in &sources {
        match create_profile_from_source(&dir, source) {
            Ok(()) => println!("  Created profile: {}.toml", source.profile_name),
            Err(e) => eprintln!("  Error creating profile '{}': {}", source.profile_name, e),
        }
    }

    println!("\n✓ Migration complete!");
    println!("  Profiles directory: {}", profiles_dir.display());
    println!("  Common config: {}", common_path.display());
    println!("\nYou can now use 'ucp switch <name>' to switch providers.");
    Ok(())
}

fn discover_sources(dir: &std::path::Path) -> Result<Vec<MigrationSource>> {
    let mut sources = Vec::new();

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let fname = entry.file_name().to_string_lossy().to_string();

        if fname.starts_with("config_") && fname.ends_with(".toml") {
            let name = fname
                .strip_prefix("config_")
                .unwrap()
                .strip_suffix(".toml")
                .unwrap()
                .to_string();

            let auth_name = format!("auth_{}.json", name);
            let auth_path = if dir.join(&auth_name).exists() {
                auth_name
            } else {
                String::new()
            };

            sources.push(MigrationSource {
                config_path: fname,
                auth_path,
                profile_name: name,
            });
        }
    }

    Ok(sources)
}

fn find_richest_config(sources: &[MigrationSource]) -> Result<String> {
    let dir = codex_dir();
    let mut best = (0usize, String::new());

    for s in sources {
        let path = dir.join(&s.config_path);
        if let Ok(content) = fs::read_to_string(&path) {
            let key_count = content.lines().count();
            if key_count > best.0 {
                best = (key_count, s.config_path.clone());
            }
        }
    }

    if best.1.is_empty() {
        anyhow::bail!("No readable config files found");
    }
    Ok(best.1)
}

fn create_profile_from_source(dir: &std::path::Path, source: &MigrationSource) -> Result<()> {
    let config_path = dir.join(&source.config_path);
    let content = fs::read_to_string(&config_path)
        .with_context(|| format!("Cannot read {}", source.config_path))?;

    let doc = content
        .parse::<DocumentMut>()
        .with_context(|| format!("Cannot parse {}", source.config_path))?;

    let model_provider = doc
        .get("model_provider")
        .and_then(|v| v.as_str())
        .unwrap_or(&source.profile_name)
        .to_string();

    let model = doc
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let (base_url, wire_api, requires_auth, display_name) =
        extract_provider_details(&doc, &model_provider);

    let provider_config = ProviderConfig {
        model_provider: model_provider.clone(),
        name: display_name.unwrap_or_else(|| source.profile_name.clone()),
        model,
        base_url,
        wire_api,
        requires_openai_auth: requires_auth,
        model_context_window: doc
            .get("model_context_window")
            .and_then(|v| v.as_integer())
            .map(|v| v as u64),
        model_auto_compact_token_limit: doc
            .get("model_auto_compact_token_limit")
            .and_then(|v| v.as_integer())
            .map(|v| v as u64),
        model_reasoning_effort: doc
            .get("model_reasoning_effort")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        disable_response_storage: doc
            .get("disable_response_storage")
            .and_then(|v| v.as_bool()),
    };

    // Handle auth: always save the raw JSON as a snapshot,
    // and also extract simple string fields into the profile's [auth] section
    let mut auth_map = HashMap::new();
    let mut has_complex_auth = false;

    if !source.auth_path.is_empty() {
        let auth_file_path = dir.join(&source.auth_path);
        if auth_file_path.exists() {
            // Always save the raw snapshot (handles tokens, nested objects, etc.)
            auth::save_auth_snapshot_from(&auth_file_path, &source.profile_name)?;

            // Also extract simple string fields for the TOML profile
            let auth_content = fs::read_to_string(&auth_file_path)?;
            if let Ok(parsed) =
                serde_json::from_str::<HashMap<String, serde_json::Value>>(&auth_content)
            {
                for (k, v) in &parsed {
                    match v {
                        serde_json::Value::String(s) => {
                            auth_map.insert(k.clone(), s.clone());
                        }
                        serde_json::Value::Null => {
                            // Skip null values
                        }
                        _ => {
                            // Complex value (dict, array) — mark as complex
                            has_complex_auth = true;
                        }
                    }
                }
            }
        }
    }

    if has_complex_auth {
        // Add a marker so users know the real auth is in the snapshot
        auth_map.insert(
            "_note".to_string(),
            format!(
                "Full auth stored in {}.auth.json (contains tokens/complex fields)",
                source.profile_name
            ),
        );
    }

    let profile = ProviderProfile {
        provider: provider_config,
        auth: auth_map,
        config_overrides: None,
    };

    let profile_path = providers_dir().join(format!("{}.toml", source.profile_name));
    let content = toml::to_string_pretty(&profile)?;
    fs::write(&profile_path, content)?;

    Ok(())
}

fn extract_provider_details(
    doc: &DocumentMut,
    model_provider: &str,
) -> (Option<String>, Option<String>, Option<bool>, Option<String>) {
    let providers = doc.get("model_providers").and_then(|v| v.as_table());

    if let Some(providers_table) = providers {
        if let Some(entry) = providers_table
            .get(model_provider)
            .and_then(|v| v.as_table())
        {
            let base_url = entry
                .get("base_url")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let wire_api = entry
                .get("wire_api")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let requires = entry.get("requires_openai_auth").and_then(|v| v.as_bool());
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            return (base_url, wire_api, requires, name);
        }
    }

    (None, None, None, None)
}
