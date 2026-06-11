use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// A provider profile representing a model provider configuration.
///
/// Structure:
/// ```toml
/// [provider]
/// model_provider = "a2o_proxy"
/// name = "A2O Proxy"
/// model = "gpt-5.5-pro"
/// base_url = "http://..."
/// ...
///
/// [auth]
/// OPENAI_API_KEY = "sk-..."
///
/// # Optional: per-provider config overrides (extra plugins, mcp_servers, etc.)
/// # These get merged ON TOP of common.toml when switching to this provider.
/// [config_overrides]
/// # Raw TOML content that supplements common.toml
/// # Example: disable a plugin that doesn't work with this provider
/// # [config_overrides."plugins.\"some-plugin@marketplace\""]
/// # enabled = false
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub provider: ProviderConfig,
    #[serde(default)]
    pub auth: HashMap<String, String>,
    /// Optional per-provider config overrides.
    /// Keys are dotted TOML paths, values are the override content.
    /// These get applied on top of common.toml during merge.
    #[serde(default)]
    pub config_overrides: Option<HashMap<String, toml::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// The key used in config.toml's model_provider field
    pub model_provider: String,
    /// Display name for the provider
    pub name: String,
    /// Model identifier
    pub model: String,
    /// API base URL (optional for openai native)
    pub base_url: Option<String>,
    /// Wire API type (e.g., "responses", "chat")
    pub wire_api: Option<String>,
    /// Whether it requires openai auth
    pub requires_openai_auth: Option<bool>,
    /// Context window size
    pub model_context_window: Option<u64>,
    /// Auto compact token limit
    pub model_auto_compact_token_limit: Option<u64>,
    /// Reasoning effort
    pub model_reasoning_effort: Option<String>,
    /// Disable response storage
    pub disable_response_storage: Option<bool>,
}

/// Get the providers directory path
pub fn providers_dir() -> PathBuf {
    dirs::home_dir()
        .expect("Cannot determine home directory")
        .join(".codex")
        .join("providers")
}

/// Get a provider profile TOML path by name.
pub fn profile_path(name: &str) -> PathBuf {
    providers_dir().join(format!("{}.toml", name))
}

/// List all registered provider profiles
pub fn list_providers() -> Result<Vec<(String, ProviderProfile)>> {
    let dir = providers_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut providers = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map_or(false, |e| e == "toml") {
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
            let profile = load_profile(&path)
                .with_context(|| format!("Failed to load profile: {}", path.display()))?;
            providers.push((name, profile));
        }
    }
    providers.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(providers)
}

/// List all registered provider profile names.
pub fn list_profile_names() -> Result<Vec<String>> {
    Ok(list_providers()?
        .into_iter()
        .map(|(name, _)| name)
        .collect())
}

/// List profile names from provider TOML filenames without parsing profile content.
pub fn list_profile_file_names() -> Result<Vec<String>> {
    let dir = providers_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut names = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map_or(false, |e| e == "toml") {
            if let Some(stem) = path.file_stem() {
                names.push(stem.to_string_lossy().to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Load a provider profile from a TOML file
pub fn load_profile(path: &Path) -> Result<ProviderProfile> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Cannot read profile: {}", path.display()))?;
    let profile: ProviderProfile = toml::from_str(&content)
        .with_context(|| format!("Cannot parse profile: {}", path.display()))?;
    Ok(profile)
}

/// Load a provider profile by name
pub fn load_profile_by_name(name: &str) -> Result<ProviderProfile> {
    let path = profile_path(name);
    if !path.exists() {
        anyhow::bail!(
            "Provider profile '{}' not found at {}",
            name,
            path.display()
        );
    }
    load_profile(&path)
}

/// Save a provider profile
pub fn save_profile(name: &str, profile: &ProviderProfile) -> Result<()> {
    let dir = providers_dir();
    fs::create_dir_all(&dir)?;
    let path = profile_path(name);
    let content = toml::to_string_pretty(profile)?;
    fs::write(&path, content)?;
    Ok(())
}

/// Delete a provider profile TOML file.
pub fn delete_profile(name: &str) -> Result<PathBuf> {
    let path = profile_path(name);
    if !path.exists() {
        anyhow::bail!(
            "Provider profile '{}' not found at {}",
            name,
            path.display()
        );
    }
    fs::remove_file(&path).with_context(|| format!("Cannot delete profile: {}", path.display()))?;
    Ok(path)
}

/// Identify which provider profile matches the current config.toml's model_provider
pub fn identify_current_provider(
    model_provider: &str,
) -> Result<Option<(String, ProviderProfile)>> {
    let providers = list_providers()?;
    for (name, profile) in providers {
        if profile.provider.model_provider == model_provider {
            return Ok(Some((name, profile)));
        }
    }
    Ok(None)
}
