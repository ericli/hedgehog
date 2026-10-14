//! Runs the daemon at login via a per-user LaunchAgent. A LaunchAgent (not a
//! LaunchDaemon) because Docker Desktop and OrbStack run per user too, which
//! means the Mac needs to log in automatically after a reboot.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use hedgehog_core::Paths;

pub const LABEL: &str = "dev.hedgehog.daemon";

pub fn plist_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn uid() -> Result<String> {
    let output = Command::new("id").arg("-u").output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn domain_target() -> Result<String> {
    Ok(format!("gui/{}/{LABEL}", uid()?))
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// launchd starts jobs with a bare PATH; include where Docker, Railpack,
/// git, and Tailscale usually live, plus whatever PATH installed us.
pub fn service_path(current: Option<&str>) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut dirs: Vec<String> = current
        .unwrap_or_default()
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(str::to_string)
        .collect();
    for dir in [
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
        format!("{home}/.local/bin"),
        format!("{home}/.orbstack/bin"),
        format!("{home}/.cargo/bin"),
        "/Applications/Docker.app/Contents/Resources/bin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
        "/usr/sbin".to_string(),
        "/sbin".to_string(),
    ] {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs.join(":")
}

pub fn render_plist(exe: &Path, log: &Path, path_env: &str, hedgehog_home: Option<&str>) -> String {
    let mut env = format!(
        "    <key>PATH</key>\n    <string>{}</string>\n",
        xml_escape(path_env)
    );
    if let Some(home) = hedgehog_home {
        env.push_str(&format!(
            "    <key>HEDGEHOG_HOME</key>\n    <string>{}</string>\n",
            xml_escape(home)
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>daemon</string>
    <string>run</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
{env}  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        exe = xml_escape(&exe.display().to_string()),
        log = xml_escape(&log.display().to_string()),
    )
}

/// Writes the LaunchAgent for this executable and (re)loads it.
pub fn install(paths: &Paths, exe: &Path) -> Result<PathBuf> {
    let plist = plist_path()?;
    std::fs::create_dir_all(plist.parent().unwrap())?;
    std::fs::create_dir_all(&paths.state)?;
    let path_env = service_path(std::env::var("PATH").ok().as_deref());
    let hedgehog_home = std::env::var("HEDGEHOG_HOME").ok();
    std::fs::write(
        &plist,
        render_plist(
            exe,
            &paths.daemon_log(),
            &path_env,
            hedgehog_home.as_deref(),
        ),
    )?;
    let _ = Command::new("launchctl")
        .args(["bootout", &domain_target()?])
        .output();
    let output = Command::new("launchctl")
        .args(["bootstrap", &format!("gui/{}", uid()?)])
        .arg(&plist)
        .output()?;
    if !output.status.success() {
        bail!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(plist)
}

pub fn uninstall() -> Result<()> {
    let _ = Command::new("launchctl")
        .args(["bootout", &domain_target()?])
        .output();
    let plist = plist_path()?;
    if plist.exists() {
        std::fs::remove_file(plist)?;
    }
    Ok(())
}

pub fn is_loaded() -> bool {
    domain_target()
        .ok()
        .and_then(|target| {
            Command::new("launchctl")
                .args(["print", &target])
                .output()
                .ok()
        })
        .is_some_and(|output| output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_plist() {
        let plist = render_plist(
            Path::new("/Users/me/.cargo/bin/hedgehog"),
            Path::new("/tmp/hedgehog.log"),
            "/usr/bin:/bin",
            None,
        );
        assert!(plist.contains("<string>dev.hedgehog.daemon</string>"));
        assert!(plist.contains("<string>/Users/me/.cargo/bin/hedgehog</string>"));
        assert!(plist.contains("<string>daemon</string>"));
        assert!(!plist.contains("HEDGEHOG_HOME"));
    }

    #[test]
    fn path_keeps_order_and_adds_defaults() {
        let path = service_path(Some("/custom/bin:/usr/bin"));
        assert!(path.starts_with("/custom/bin:/usr/bin:"));
        assert!(path.contains("/opt/homebrew/bin"));
        assert_eq!(path.split(':').filter(|dir| *dir == "/usr/bin").count(), 1);
    }
}
