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

/// The daemon's environment: PATH, plus every `HEDGEHOG_*` setting from the
/// shell that ran `hedgehog daemon install` (e.g. `HEDGEHOG_MAX_BUILDS=1`).
pub fn service_env(
    path_env: String,
    vars: impl IntoIterator<Item = (String, String)>,
) -> Vec<(String, String)> {
    let mut env = vec![("PATH".to_string(), path_env)];
    let mut settings: Vec<(String, String)> = vars
        .into_iter()
        .filter(|(key, _)| key.starts_with("HEDGEHOG_") && key != "HEDGEHOG_REMOTE")
        .collect();
    settings.sort();
    env.extend(settings);
    env
}

/// A launchd domain can get stuck in "on-demand-only" mode, where it holds
/// back RunAtLoad and KeepAlive spawns, so a crashed daemon would stay down.
/// Calendar events still count as demand, so the empty StartCalendarInterval
/// (every minute) restarts it within a minute; a running daemon is left alone.
pub fn render_plist(exe: &Path, log: &Path, vars: &[(String, String)]) -> String {
    let env: String = vars
        .iter()
        .map(|(key, value)| {
            format!(
                "    <key>{}</key>\n    <string>{}</string>\n",
                xml_escape(key),
                xml_escape(value)
            )
        })
        .collect();
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
  <key>StartCalendarInterval</key>
  <dict/>
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
    let env = service_env(
        service_path(std::env::var("PATH").ok().as_deref()),
        std::env::vars(),
    );
    std::fs::write(&plist, render_plist(exe, &paths.daemon_log(), &env))?;
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
    // On macOS 26 a freshly bootstrapped Background job can load without ever
    // spawning, despite RunAtLoad, so start it explicitly. Without -k: there's
    // nothing to kill yet, and a job that did spawn is left alone.
    match Command::new("launchctl")
        .args(["kickstart", &domain_target()?])
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => eprintln!(
            "warning: launchctl kickstart failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => eprintln!("warning: launchctl kickstart failed: {error}"),
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
        let env = service_env(
            "/usr/bin:/bin".to_string(),
            [
                ("HOME".to_string(), "/Users/me".to_string()),
                ("HEDGEHOG_MAX_BUILDS".to_string(), "1".to_string()),
                ("HEDGEHOG_REMOTE".to_string(), "home".to_string()),
                ("HEDGEHOG_DOCKER_CREDENTIALS".to_string(), "1".to_string()),
            ],
        );
        let plist = render_plist(
            Path::new("/Users/me/.cargo/bin/hedgehog"),
            Path::new("/tmp/hedgehog.log"),
            &env,
        );
        assert!(plist.contains("<string>dev.hedgehog.daemon</string>"));
        assert!(plist.contains("<string>/Users/me/.cargo/bin/hedgehog</string>"));
        assert!(plist.contains("<string>daemon</string>"));
        assert!(plist.contains("<key>PATH</key>"));
        assert!(
            plist.contains("<key>StartCalendarInterval</key>\n  <dict/>"),
            "restarts a crashed daemon when the domain holds KeepAlive spawns"
        );
        assert!(plist.contains("<key>HEDGEHOG_MAX_BUILDS</key>\n    <string>1</string>"));
        assert!(plist.contains("<key>HEDGEHOG_DOCKER_CREDENTIALS</key>"));
        assert!(!plist.contains("HEDGEHOG_REMOTE"), "a CLI-only setting");
        assert!(
            !plist.contains("<key>HOME</key>"),
            "only hedgehog settings pass through"
        );
    }

    #[test]
    fn path_keeps_order_and_adds_defaults() {
        let path = service_path(Some("/custom/bin:/usr/bin"));
        assert!(path.starts_with("/custom/bin:/usr/bin:"));
        assert!(path.contains("/opt/homebrew/bin"));
        assert_eq!(path.split(':').filter(|dir| *dir == "/usr/bin").count(), 1);
    }
}
