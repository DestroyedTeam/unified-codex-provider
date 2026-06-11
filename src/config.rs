use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use toml_edit::DocumentMut;

use crate::provider::ProviderProfile;

/// Get the codex directory path
pub fn codex_dir() -> PathBuf {
    dirs::home_dir()
        .expect("Cannot determine home directory")
        .join(".codex")
}

/// Get the common.toml path
pub fn common_toml_path() -> PathBuf {
    codex_dir().join("common.toml")
}

/// Get the config.toml path
pub fn config_toml_path() -> PathBuf {
    codex_dir().join("config.toml")
}

/// Read the current model_provider from config.toml
pub fn read_current_model_provider() -> Result<Option<String>> {
    let path = config_toml_path();
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path)?;
    let doc = content
        .parse::<DocumentMut>()
        .context("Failed to parse config.toml")?;

    Ok(doc
        .get("model_provider")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

/// Merge common.toml + provider profile -> config.toml
///
/// Output order:
///   1. Provider top-level scalars (model_provider, model, ...)
///   2. Common top-level scalars (notify, ...)
///   3. Per-provider scalar config_overrides (e.g. model_catalog_json) -- BEFORE any sections
///   4. [model_providers.xxx] section
///   5. All common table sections (marketplaces, plugins, projects, ...)
pub fn merge_and_write_config(profile: &ProviderProfile) -> Result<()> {
    let common_path = common_toml_path();
    let config_path = config_toml_path();

    let common_content = if common_path.exists() {
        fs::read_to_string(&common_path).context("Failed to read common.toml")?
    } else {
        String::new()
    };

    let common_doc = common_content
        .parse::<DocumentMut>()
        .context("Failed to parse common.toml")?;

    let p = &profile.provider;
    let mut output = String::new();

    // 1. Provider top-level scalars
    output.push_str(&format!("model_provider = \"{}\"\n", p.model_provider));
    output.push_str(&format!("model = \"{}\"\n", p.model));
    if let Some(window) = p.model_context_window {
        output.push_str(&format!("model_context_window = {}\n", window));
    }
    if let Some(limit) = p.model_auto_compact_token_limit {
        output.push_str(&format!("model_auto_compact_token_limit = {}\n", limit));
    }
    output.push('\n');

    // 2. Common top-level scalars
    for (key, item) in common_doc.iter() {
        if item.is_table() || item.is_array_of_tables() {
            continue;
        }
        if matches!(
            key,
            "model_provider" | "model" | "model_context_window" | "model_auto_compact_token_limit"
        ) {
            continue;
        }
        output.push_str(&format!("{} = {}\n", key, item.to_string().trim()));
    }

    if let Some(ref effort) = p.model_reasoning_effort {
        if !common_doc.contains_key("model_reasoning_effort") {
            output.push_str(&format!("model_reasoning_effort = \"{}\"\n", effort));
        }
    }
    if let Some(disable) = p.disable_response_storage {
        if disable && !common_doc.contains_key("disable_response_storage") {
            output.push_str("disable_response_storage = true\n");
        }
    }

    output.push('\n');

    // 3. Per-provider scalar config_overrides -- must be BEFORE any [section] headers
    //    so TOML treats them as top-level keys, not sub-keys of [model_providers.*].
    if let Some(ref overrides) = profile.config_overrides {
        for (key, value) in overrides.iter() {
            if !key.starts_with('[') {
                output.push_str(&format!("{} = {}\n", key, value));
            }
        }
        let has_scalar = overrides.keys().any(|k| !k.starts_with('['));
        if has_scalar {
            output.push('\n');
        }
    }

    // 4. [model_providers.xxx] section
    // Codex only has "openai" as a built-in provider. Any other model_provider
    // value MUST be registered in [model_providers] or Codex will fail with
    // "Model provider `xxx` not found".
    let needs_provider_section = p.model_provider != "openai";
    if needs_provider_section {
        output.push_str("[model_providers]\n");
        output.push_str(&format!("[model_providers.{}]\n", p.model_provider));
        output.push_str(&format!("name = \"{}\"\n", p.name));
        // wire_api: default to "responses" for OpenAI-compatible providers
        let wire_api = p.wire_api.as_deref().unwrap_or("responses");
        output.push_str(&format!("wire_api = \"{}\"\n", wire_api));
        // requires_openai_auth: default true for chatgpt-auth providers
        let requires_auth = p.requires_openai_auth.unwrap_or(true);
        output.push_str(&format!("requires_openai_auth = {}\n", requires_auth));
        if let Some(ref base_url) = p.base_url {
            output.push_str(&format!("base_url = \"{}\"\n", base_url));
        }
        output.push('\n');
    }
    for (key, item) in common_doc.iter() {
        if !item.is_table() && !item.is_array_of_tables() {
            continue;
        }
        if key == "model_providers" {
            continue;
        }
        let mut section_doc = DocumentMut::new();
        section_doc.insert(key, item.clone());
        output.push_str(&section_doc.to_string());
    }

    // Backup and write
    if config_path.exists() {
        let backup = codex_dir().join("config.toml.bak");
        fs::copy(&config_path, &backup)?;
    }

    fs::write(&config_path, &output)?;
    Ok(())
}

/// Extract common (non-provider) sections from a config.toml content.
/// Returns everything except model_provider, model, model_context_window,
/// model_auto_compact_token_limit, and [model_providers.*].
pub fn extract_common_config(content: &str) -> Result<String> {
    let doc = content
        .parse::<DocumentMut>()
        .context("Failed to parse config content")?;

    let mut common = doc.clone();

    // Remove provider-specific keys
    common.remove("model_provider");
    common.remove("model");
    common.remove("model_context_window");
    common.remove("model_auto_compact_token_limit");
    common.remove("model_providers");
    // Remove per-provider overrides that should not bleed into common
    common.remove("model_catalog_json");

    Ok(common.to_string())
}
