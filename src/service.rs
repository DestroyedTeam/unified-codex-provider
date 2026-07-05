use anyhow::{bail, Context, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::config;

const LABEL: &str = "com.codex.unified-provider-sync";

pub fn install_launch_agent() -> Result<()> {
    ensure_macos("installing the LaunchAgent")?;

    let ucp_bin = std::env::current_exe().context("Cannot resolve current ucp executable")?;
    let home = home_dir()?;
    let codex_dir = config::codex_dir();
    fs::create_dir_all(&codex_dir)?;

    let launch_agents_dir = home.join("Library").join("LaunchAgents");
    fs::create_dir_all(&launch_agents_dir)?;
    let plist_path = plist_path()?;
    fs::write(&plist_path, render_plist(&ucp_bin, &home))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&plist_path, fs::Permissions::from_mode(0o600))?;
    }

    bootout_launch_agent();
    run_launchctl(&[
        "bootstrap",
        &launch_domain(),
        &plist_path.display().to_string(),
    ])
    .context("Failed to load LaunchAgent")?;
    run_launchctl(&["enable", &format!("{}/{}", launch_domain(), LABEL)])
        .context("Failed to enable LaunchAgent")?;

    println!("Installed LaunchAgent: {}", plist_path.display());
    Ok(())
}

pub fn uninstall_launch_agent() -> Result<()> {
    ensure_macos("uninstalling the LaunchAgent")?;

    bootout_launch_agent();
    let plist = plist_path()?;
    if plist.exists() {
        fs::remove_file(&plist).with_context(|| format!("Cannot remove {}", plist.display()))?;
        println!("Removed LaunchAgent: {}", plist.display());
    } else {
        println!("LaunchAgent not installed: {}", plist.display());
    }
    Ok(())
}

pub fn show_launch_agent_status() -> Result<()> {
    ensure_macos("checking the LaunchAgent")?;

    let plist = plist_path()?;
    println!("LaunchAgent: {}", LABEL);
    println!("Plist: {}", plist.display());
    println!("Installed: {}", if plist.exists() { "yes" } else { "no" });

    let target = format!("{}/{}", launch_domain(), LABEL);
    let output = Command::new("launchctl")
        .args(["print", &target])
        .output()?;
    println!(
        "Loaded: {}",
        if output.status.success() { "yes" } else { "no" }
    );
    Ok(())
}

pub fn launch_agent_installed() -> Result<bool> {
    Ok(plist_path()?.exists())
}

fn ensure_macos(action: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        Ok(())
    } else {
        bail!("{} is only supported on macOS", action)
    }
}

fn render_plist(ucp_bin: &PathBuf, home: &PathBuf) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>

    <key>ProgramArguments</key>
    <array>
        <string>{ucp_bin}</string>
        <string>sync</string>
        <string>--auto</string>
        <string>--refresh-auth</string>
    </array>

    <key>WatchPaths</key>
    <array>
        <string>{home}/.codex/auth.json</string>
        <string>{home}/.codex/config.toml</string>
    </array>

    <key>RunAtLoad</key>
    <true/>

    <key>StartInterval</key>
    <integer>86400</integer>

    <key>StandardOutPath</key>
    <string>{home}/.codex/.ucp_sync_stdout.log</string>

    <key>StandardErrorPath</key>
    <string>{home}/.codex/.ucp_sync_stderr.log</string>

    <key>ThrottleInterval</key>
    <integer>5</integer>
</dict>
</plist>
"#,
        label = LABEL,
        ucp_bin = xml_escape(&ucp_bin.display().to_string()),
        home = xml_escape(&home.display().to_string())
    )
}

fn bootout_launch_agent() {
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{}/{}", launch_domain(), LABEL)])
        .status();
}

fn run_launchctl(args: &[&str]) -> Result<()> {
    let status = Command::new("launchctl").args(args).status()?;
    if !status.success() {
        bail!("launchctl {:?} exited with {}", args, status);
    }
    Ok(())
}

fn plist_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("Cannot determine home directory")
}

fn launch_domain() -> String {
    format!("gui/{}", current_uid())
}

fn current_uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "0".to_string())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
