use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{auth, provider};

const REFRESH_MARGIN_HOURS: i64 = 72;

#[derive(Debug, Clone)]
pub struct AuthRefreshOptions {
    pub profile_name: Option<String>,
    pub all: bool,
    pub force: bool,
    pub dry_run: bool,
    pub min_interval_hours: u64,
    pub max_stale_days: u64,
    pub quiet: bool,
}

impl Default for AuthRefreshOptions {
    fn default() -> Self {
        Self {
            profile_name: None,
            all: true,
            force: false,
            dry_run: false,
            min_interval_hours: 24,
            max_stale_days: 14,
            quiet: false,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AuthRefreshSummary {
    pub scanned: usize,
    pub refreshed: usize,
    pub fresh: usize,
    pub would_refresh: usize,
    pub skipped_non_chatgpt: usize,
    pub skipped_stale: usize,
    pub missing_snapshot: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RefreshDecision {
    Refresh(String),
    Fresh(String),
    SkipNonChatGpt(String),
    SkipStale(String),
    MissingSnapshot(String),
}

pub fn refresh_auth_snapshots(options: AuthRefreshOptions) -> Result<AuthRefreshSummary> {
    let targets = refresh_targets(&options)?;
    let mut summary = AuthRefreshSummary::default();

    for (name, profile) in targets {
        summary.scanned += 1;
        let snapshot_path = auth::auth_snapshot_path(&name);
        let decision = decide_snapshot(
            &snapshot_path,
            options.force,
            Duration::hours(options.min_interval_hours as i64),
            Duration::days(options.max_stale_days as i64),
        );

        match decision {
            Ok(RefreshDecision::MissingSnapshot(reason)) => {
                summary.missing_snapshot += 1;
                print_profile(&options, &name, "missing", &reason);
            }
            Ok(RefreshDecision::SkipNonChatGpt(reason)) => {
                summary.skipped_non_chatgpt += 1;
                print_profile(&options, &name, "skip", &reason);
            }
            Ok(RefreshDecision::SkipStale(reason)) => {
                summary.skipped_stale += 1;
                print_profile(&options, &name, "stale", &reason);
            }
            Ok(RefreshDecision::Fresh(reason)) => {
                summary.fresh += 1;
                print_profile(&options, &name, "fresh", &reason);
            }
            Ok(RefreshDecision::Refresh(reason)) => {
                if options.dry_run {
                    summary.would_refresh += 1;
                    print_profile(&options, &name, "would refresh", &reason);
                    continue;
                }

                match refresh_one_snapshot(&name, &profile, &snapshot_path) {
                    Ok(()) => {
                        summary.refreshed += 1;
                        print_profile(&options, &name, "refreshed", &reason);
                    }
                    Err(err) => {
                        summary.failed += 1;
                        print_profile(&options, &name, "failed", &err.to_string());
                    }
                }
            }
            Err(err) => {
                summary.failed += 1;
                print_profile(&options, &name, "failed", &err.to_string());
            }
        }
    }

    Ok(summary)
}

pub fn print_summary(summary: &AuthRefreshSummary) {
    println!(
        "Auth refresh: {} scanned, {} refreshed, {} fresh, {} stale skipped, {} non-ChatGPT skipped, {} missing, {} failed{}",
        summary.scanned,
        summary.refreshed,
        summary.fresh,
        summary.skipped_stale,
        summary.skipped_non_chatgpt,
        summary.missing_snapshot,
        summary.failed,
        if summary.would_refresh > 0 {
            format!(", {} would refresh", summary.would_refresh)
        } else {
            String::new()
        }
    );
}

fn refresh_targets(
    options: &AuthRefreshOptions,
) -> Result<Vec<(String, provider::ProviderProfile)>> {
    if let Some(name) = &options.profile_name {
        let profile = provider::load_profile_by_name(name)?;
        return Ok(vec![(name.clone(), profile)]);
    }

    let _all = options.all;
    provider::list_providers()
}

fn decide_snapshot(
    path: &Path,
    force: bool,
    min_interval: Duration,
    max_stale: Duration,
) -> Result<RefreshDecision> {
    if !path.exists() {
        return Ok(RefreshDecision::MissingSnapshot(format!(
            "no auth snapshot at {}",
            path.display()
        )));
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read auth snapshot: {}", path.display()))?;
    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse auth snapshot: {}", path.display()))?;

    if value.get("auth_mode").and_then(Value::as_str) != Some("chatgpt") {
        return Ok(RefreshDecision::SkipNonChatGpt(
            "auth_mode is not chatgpt".to_string(),
        ));
    }

    if value
        .pointer("/tokens/refresh_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .is_none()
    {
        return Ok(RefreshDecision::SkipNonChatGpt(
            "missing ChatGPT refresh token".to_string(),
        ));
    }

    let now = Utc::now();
    let last_refresh = parse_last_refresh(&value);
    let access_exp = jwt_timestamp(&value, "access_token", "exp");

    if !force {
        if let Some(last_refresh) = last_refresh {
            if now.signed_duration_since(last_refresh) > max_stale {
                return Ok(RefreshDecision::SkipStale(format!(
                    "last refresh is older than {} days",
                    max_stale.num_days()
                )));
            }
        } else if let Some(access_exp) = access_exp {
            if now.signed_duration_since(access_exp) > max_stale {
                return Ok(RefreshDecision::SkipStale(format!(
                    "access token expired more than {} days ago",
                    max_stale.num_days()
                )));
            }
        } else {
            return Ok(RefreshDecision::SkipStale(
                "cannot determine snapshot age".to_string(),
            ));
        }
    }

    if force {
        return Ok(RefreshDecision::Refresh("forced".to_string()));
    }

    if let Some(last_refresh) = last_refresh {
        if now.signed_duration_since(last_refresh) >= min_interval {
            return Ok(RefreshDecision::Refresh(format!(
                "last refresh is at least {} hours old",
                min_interval.num_hours()
            )));
        }
    }

    if let Some(access_exp) = access_exp {
        if access_exp <= now + Duration::hours(REFRESH_MARGIN_HOURS) {
            return Ok(RefreshDecision::Refresh(format!(
                "access token expires within {} hours",
                REFRESH_MARGIN_HOURS
            )));
        }
    }

    Ok(RefreshDecision::Fresh(
        "snapshot is recent and token is not near expiry".to_string(),
    ))
}

fn refresh_one_snapshot(
    profile_name: &str,
    profile: &provider::ProviderProfile,
    snapshot_path: &Path,
) -> Result<()> {
    let before_content = fs::read_to_string(snapshot_path)
        .with_context(|| format!("failed to read auth snapshot: {}", snapshot_path.display()))?;
    let before_value: Value = serde_json::from_str(&before_content)
        .with_context(|| format!("failed to parse auth snapshot: {}", snapshot_path.display()))?;

    let temp_dir = TempCodexHome::create()?;
    let temp_auth = temp_dir.path.join("auth.json");
    fs::write(&temp_auth, &before_content)?;
    write_minimal_config(&temp_dir.path, profile)?;
    force_expired_tokens(&temp_auth)?;

    let output = Command::new(codex_binary())
        .args(["doctor", "--json"])
        .env("CODEX_HOME", &temp_dir.path)
        .output()
        .context("failed to run `codex doctor --json` for auth refresh")?;

    let after_content = fs::read_to_string(&temp_auth)
        .with_context(|| format!("failed to read refreshed auth: {}", temp_auth.display()))?;
    let after_value: Value = serde_json::from_str(&after_content)
        .context("Codex wrote an invalid auth.json while refreshing")?;

    if !auth_was_refreshed(&before_value, &after_value) {
        let stderr = redact_output(&output.stderr);
        let status = output.status.code().map_or_else(
            || "terminated by signal".to_string(),
            |code| format!("exit {code}"),
        );
        return Err(anyhow!(
            "Codex did not produce fresher tokens ({status}){}",
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        ));
    }

    fs::write(snapshot_path, after_content)?;
    set_private_permissions(snapshot_path)?;

    let state = crate::sync::load_state();
    if state.last_profile_name.as_deref() == Some(profile_name) {
        auth::write_auth(profile, profile_name)?;
    }

    Ok(())
}

fn write_minimal_config(dir: &Path, profile: &provider::ProviderProfile) -> Result<()> {
    let model = if profile.provider.model.trim().is_empty() {
        "gpt-5.5"
    } else {
        profile.provider.model.as_str()
    };
    let content = format!(
        "model_provider = \"openai\"\nmodel = \"{}\"\n",
        toml_escape(model)
    );
    fs::write(dir.join("config.toml"), content)?;
    Ok(())
}

fn force_expired_tokens(path: &Path) -> Result<()> {
    let content = fs::read_to_string(path)?;
    let mut value: Value = serde_json::from_str(&content)?;
    let expired_at = Utc::now().timestamp() - 60;

    for token_name in ["access_token", "id_token"] {
        if let Some(token) = value
            .pointer(&format!("/tokens/{token_name}"))
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            if let Ok(expired) = rewrite_jwt_timestamp(&token, "exp", expired_at) {
                if let Some(slot) = value.pointer_mut(&format!("/tokens/{token_name}")) {
                    *slot = Value::String(expired);
                }
            }
        }
    }

    fs::write(path, serde_json::to_string_pretty(&value)? + "\n")?;
    set_private_permissions(path)?;
    Ok(())
}

fn auth_was_refreshed(before: &Value, after: &Value) -> bool {
    let before_last = parse_last_refresh(before);
    let after_last = parse_last_refresh(after);
    if let (Some(before_last), Some(after_last)) = (before_last, after_last) {
        if after_last > before_last {
            return true;
        }
    }

    let before_access_exp = jwt_timestamp(before, "access_token", "exp");
    let after_access_exp = jwt_timestamp(after, "access_token", "exp");
    matches!((before_access_exp, after_access_exp), (Some(before), Some(after)) if after > before)
}

fn parse_last_refresh(value: &Value) -> Option<DateTime<Utc>> {
    value
        .get("last_refresh")
        .and_then(Value::as_str)
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn jwt_timestamp(value: &Value, token_name: &str, claim: &str) -> Option<DateTime<Utc>> {
    let token = value.pointer(&format!("/tokens/{token_name}"))?.as_str()?;
    let timestamp = jwt_payload(token).ok()?.get(claim)?.as_i64()?;
    DateTime::<Utc>::from_timestamp(timestamp, 0)
}

fn rewrite_jwt_timestamp(token: &str, claim: &str, timestamp: i64) -> Result<String> {
    let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
    if parts.len() != 3 {
        return Err(anyhow!("not a jwt"));
    }

    let mut payload = jwt_payload(token)?;
    payload[claim] = Value::Number(timestamp.into());
    let raw = serde_json::to_vec(&payload)?;
    parts[1] = URL_SAFE_NO_PAD.encode(raw);
    Ok(parts.join("."))
}

fn jwt_payload(token: &str) -> Result<Value> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("not a jwt"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .context("failed to decode jwt payload")?;
    serde_json::from_slice(&bytes).context("failed to parse jwt payload")
}

fn codex_binary() -> String {
    std::env::var("UCP_CODEX_BIN").unwrap_or_else(|_| "codex".to_string())
}

fn print_profile(options: &AuthRefreshOptions, name: &str, status: &str, detail: &str) {
    if !options.quiet {
        println!("  {status:<13} {name}: {detail}");
    }
}

fn set_private_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn redact_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .take(5)
        .map(redact_line)
        .collect::<Vec<_>>()
        .join("; ")
}

fn redact_line(line: &str) -> String {
    line.split_whitespace()
        .map(|part| {
            if part.len() >= 30 {
                "<redacted>".to_string()
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

struct TempCodexHome {
    path: PathBuf,
}

impl TempCodexHome {
    fn create() -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock before unix epoch")?
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ucp_auth_refresh_{}_{}", std::process::id(), nanos));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for TempCodexHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chatgpt_auth(last_refresh: &str, exp: i64) -> Value {
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": jwt(exp),
                "id_token": jwt(exp),
                "refresh_token": "refresh-token",
                "account_id": "account"
            },
            "last_refresh": last_refresh
        })
    }

    fn jwt(exp: i64) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp},"iat":{}}}"#, exp - 3600));
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn stale_snapshots_are_skipped() {
        let dir = TempCodexHome::create().unwrap();
        let path = dir.path.join("auth.json");
        fs::write(
            &path,
            serde_json::to_string(&chatgpt_auth("2000-01-01T00:00:00Z", 946684800)).unwrap(),
        )
        .unwrap();

        let decision =
            decide_snapshot(&path, false, Duration::hours(24), Duration::days(14)).unwrap();
        assert!(matches!(decision, RefreshDecision::SkipStale(_)));
    }

    #[test]
    fn recent_snapshots_are_fresh() {
        let dir = TempCodexHome::create().unwrap();
        let path = dir.path.join("auth.json");
        let now = Utc::now();
        fs::write(
            &path,
            serde_json::to_string(&chatgpt_auth(
                &now.to_rfc3339(),
                (now + Duration::days(10)).timestamp(),
            ))
            .unwrap(),
        )
        .unwrap();

        let decision =
            decide_snapshot(&path, false, Duration::hours(24), Duration::days(14)).unwrap();
        assert!(matches!(decision, RefreshDecision::Fresh(_)));
    }
}
