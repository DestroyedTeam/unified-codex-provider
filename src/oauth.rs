use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::auth;
use crate::provider::{self, ProviderConfig, ProviderProfile};

const DEFAULT_OPENAI_MODEL: &str = "gpt-5.5";
const DEFAULT_OPENAI_PROVIDER: &str = "openai";
const DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;
const DEFAULT_AUTO_COMPACT_LIMIT: u64 = 920_000;

pub struct LoginOptions {
    pub name: Option<String>,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub switch_after: bool,
}

pub struct SavedLogin {
    pub profile_name: String,
    pub auth_snapshot: PathBuf,
}

pub fn login_and_save(options: LoginOptions) -> Result<SavedLogin> {
    let temp_home = TempCodexHome::new()?;
    run_codex_login(temp_home.path())?;

    let temp_auth_path = temp_home.path().join("auth.json");
    if !temp_auth_path.exists() {
        bail!("Codex login finished, but no auth.json was written");
    }

    validate_chatgpt_auth(&temp_auth_path)?;

    let profile_name = match options.name {
        Some(name) => validate_profile_name(&name)?,
        None => next_profile_name("openai")?,
    };
    let model_provider = options
        .model_provider
        .unwrap_or_else(|| DEFAULT_OPENAI_PROVIDER.to_string());
    let model = options
        .model
        .unwrap_or_else(|| DEFAULT_OPENAI_MODEL.to_string());

    let profile = ProviderProfile {
        provider: ProviderConfig {
            model_provider,
            name: profile_name.clone(),
            model,
            base_url: None,
            wire_api: None,
            requires_openai_auth: None,
            model_context_window: Some(DEFAULT_CONTEXT_WINDOW),
            model_auto_compact_token_limit: Some(DEFAULT_AUTO_COMPACT_LIMIT),
            model_reasoning_effort: Some("high".to_string()),
            disable_response_storage: Some(true),
        },
        auth: summarize_auth(&temp_auth_path, &profile_name)?,
        config_overrides: None,
    };

    provider::save_profile(&profile_name, &profile)?;
    auth::save_auth_snapshot_from(&temp_auth_path, &profile_name)?;

    let snapshot_path = auth::auth_snapshot_path(&profile_name);
    if options.switch_after {
        crate::sync::switch_provider(&profile_name)?;
    }

    Ok(SavedLogin {
        profile_name,
        auth_snapshot: snapshot_path,
    })
}

fn run_codex_login(codex_home: &Path) -> Result<()> {
    println!("Starting isolated Codex login...");
    println!("Temporary CODEX_HOME: {}", codex_home.display());

    let status = Command::new("codex")
        .arg("login")
        .env("CODEX_HOME", codex_home)
        .status()
        .context("failed to run `codex login`; is Codex CLI installed and on PATH?")?;

    if !status.success() {
        bail!("`codex login` did not complete successfully");
    }

    Ok(())
}

fn validate_chatgpt_auth(path: &Path) -> Result<()> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let mode = value.get("auth_mode").and_then(Value::as_str);
    let tokens = value.get("tokens").and_then(Value::as_object);
    if mode != Some("chatgpt") || tokens.is_none() {
        bail!("login did not produce ChatGPT auth tokens");
    }

    Ok(())
}

fn summarize_auth(path: &Path, profile_name: &str) -> Result<HashMap<String, String>> {
    let content = fs::read_to_string(path)?;
    let parsed: HashMap<String, Value> = serde_json::from_str(&content)?;
    let mut auth_map = HashMap::new();

    for key in ["auth_mode", "last_refresh"] {
        if let Some(Value::String(value)) = parsed.get(key) {
            auth_map.insert(key.to_string(), value.clone());
        }
    }

    auth_map.insert(
        "_note".to_string(),
        format!(
            "Full auth stored in {}.auth.json (contains tokens/complex fields)",
            profile_name
        ),
    );
    Ok(auth_map)
}

fn next_profile_name(prefix: &str) -> Result<String> {
    let existing = provider::list_profile_names()?;
    if !existing.iter().any(|name| name == prefix) {
        return Ok(prefix.to_string());
    }

    let mut index = 1;
    loop {
        let candidate = format!("{}_{}", prefix, index);
        if !existing.iter().any(|name| name == &candidate) {
            return Ok(candidate);
        }
        index += 1;
    }
}

fn validate_profile_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("profile name cannot be empty");
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed == "." || trimmed == ".." {
        bail!("profile name cannot contain path separators");
    }
    Ok(trimmed.to_string())
}

struct TempCodexHome {
    path: PathBuf,
}

impl TempCodexHome {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "ucp-codex-login-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempCodexHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
