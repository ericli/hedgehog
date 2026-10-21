//! Background loops: notice folders appearing in `~/Hedgehog/apps` (and new
//! commits in them), keep containers, gateway listeners, and tunnel routes in
//! line with the database, and take nightly database backups.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use hedgehog_core::Manifest;
use hedgehog_core::api::AppStatus;
use hedgehog_core::names::app_name_from_dir;
use tracing::{info, warn};

use crate::Daemon;
use crate::deploy::Source;
use crate::docker::{ContainerState, DOWN_AFTER, HealthChange};
use crate::gateway::Upstream;
use crate::git::revision as git_revision;
use crate::secrets::now;
use crate::store::AppRecord;

const SCAN_EVERY: Duration = Duration::from_secs(5);
const RECONCILE_EVERY: Duration = Duration::from_secs(30);
/// A new folder must stop changing for this long before it is deployed, so a
/// `git clone` or Finder copy finishes first.
const SETTLE_FOR: Duration = Duration::from_secs(4);
const BACKUP_EVERY: i64 = 24 * 60 * 60;

pub async fn run(daemon: Arc<Daemon>) {
    let mut scanner = Scanner::default();
    let mut last_reconcile = Instant::now();
    loop {
        tokio::time::sleep(SCAN_EVERY).await;
        let (docker_ok, came_back) = check_docker(&daemon).await;
        // Deploys and reconciling need Docker. While it isn't answering they
        // wait, rather than fail or pile up behind timeouts.
        if docker_ok {
            if let Err(error) = scanner.scan(&daemon).await {
                warn!("scanning apps failed: {error:#}");
            }
            if came_back || last_reconcile.elapsed() >= RECONCILE_EVERY {
                reconcile(&daemon, false).await;
                last_reconcile = Instant::now();
            }
        }
        maybe_backup(&daemon);
        let max = crate::env_number("HEDGEHOG_LOG_MAX_BYTES", 10 * 1024 * 1024);
        if let Ok(true) = crate::rotate_log(&daemon.paths.daemon_log(), max) {
            info!("rotated the daemon log");
        }
    }
}

/// Pings Docker and records the result, logging once when it goes down and
/// once when it comes back. Returns whether it answered, and whether it just
/// came back.
async fn check_docker(daemon: &Daemon) -> (bool, bool) {
    let ok = daemon.docker.ping().await;
    match daemon.docker_health.record(ok, Instant::now(), now()) {
        Some(HealthChange::WentDown { .. }) => warn!(
            "Docker hasn't answered for {}s, so apps are probably down. \
             Quitting and reopening Docker Desktop (or OrbStack) usually fixes it",
            DOWN_AFTER.as_secs()
        ),
        Some(HealthChange::CameBack { down_for }) => {
            let minutes = down_for.as_secs() / 60;
            info!(
                down_for = %format!("{}h{}m", minutes / 60, minutes % 60),
                "Docker is answering again; bringing apps back"
            );
            return (true, true);
        }
        None => {}
    }
    (ok, false)
}

/// Cheap "has this folder changed" signal: entry count plus newest mtime of
/// its top-level entries.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    entries: usize,
    newest: Option<SystemTime>,
}

fn fingerprint(dir: &Path) -> Option<Fingerprint> {
    let mut entries = 0;
    let mut newest = std::fs::metadata(dir).and_then(|meta| meta.modified()).ok();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        entries += 1;
        let modified = entry.metadata().and_then(|meta| meta.modified()).ok();
        newest = newest.max(modified);
    }
    Some(Fingerprint { entries, newest })
}

/// Visible subfolders of the apps folder.
fn app_folders(apps: &Path) -> Result<Vec<PathBuf>> {
    let mut folders = Vec::new();
    for entry in std::fs::read_dir(apps)?.flatten() {
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if !hidden && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            folders.push(entry.path());
        }
    }
    folders.sort();
    Ok(folders)
}

#[derive(Default)]
struct Scanner {
    /// New folders waiting to settle.
    pending: HashMap<PathBuf, (Fingerprint, Instant)>,
    /// Folders already warned about, so the log isn't spammed.
    warned: HashSet<PathBuf>,
}

impl Scanner {
    async fn scan(&mut self, daemon: &Arc<Daemon>) -> Result<()> {
        let folders = app_folders(&daemon.paths.apps)?;
        self.pending.retain(|dir, _| folders.contains(dir));
        for dir in &folders {
            let dir_name = dir.file_name().unwrap_or_default().to_string_lossy();
            let Some(name) = app_name_from_dir(&dir_name) else {
                if self.warned.insert(dir.clone()) {
                    warn!(folder = %dir.display(), "can't make an app name from this folder name; rename it to use letters, digits, and dashes");
                }
                continue;
            };
            match daemon.store.app(&name)? {
                Some(app) if app.source_dir == *dir => self.check_revision(daemon, &app).await,
                Some(app) if app.source_dir.exists() => {
                    if self.warned.insert(dir.clone()) {
                        warn!(folder = %dir.display(), app = name, other = %app.source_dir.display(), "another folder already uses this app name");
                    }
                }
                // The app's folder was renamed to this one.
                Some(_) => daemon.store.upsert_app(&name, dir)?,
                None => self.consider_new(daemon, &name, dir).await?,
            }
        }
        for app in daemon.store.apps()? {
            let gone = app.source_dir.starts_with(&daemon.paths.apps) && !app.source_dir.exists();
            // An app that never deployed and has no folder is a failed
            // "new app"; keep it so its error stays visible.
            let ever_deployed = app.deployed_at.is_some() || app.container.is_some();
            if gone && ever_deployed && !daemon.is_busy(&app.name) {
                info!(
                    app = app.name,
                    "its folder was removed; removing the app (data and database are kept)"
                );
                if let Err(error) = daemon.remove_app(&app.name, false).await {
                    warn!(app = app.name, "could not remove: {error:#}");
                }
            }
        }
        Ok(())
    }

    async fn consider_new(&mut self, daemon: &Arc<Daemon>, name: &str, dir: &Path) -> Result<()> {
        let Some(current) = fingerprint(dir) else {
            return Ok(());
        };
        let settled = match self.pending.get(dir) {
            Some((seen, since)) if *seen == current => since.elapsed() >= SETTLE_FOR,
            _ => {
                self.pending
                    .insert(dir.to_path_buf(), (current, Instant::now()));
                false
            }
        };
        if settled {
            self.pending.remove(dir);
            daemon.store.upsert_app(name, dir)?;
            if let Some(revision) = git_revision(dir).await {
                daemon
                    .attempted_revisions
                    .lock()
                    .unwrap()
                    .insert(name.to_string(), revision);
            }
            info!(app = name, folder = %dir.display(), "found a new app");
            daemon.spawn_deploy(name.to_string(), Source::Folder, "new folder");
        }
        Ok(())
    }

    /// Redeploys when a git folder moves to a new commit (`git pull`).
    async fn check_revision(&self, daemon: &Arc<Daemon>, app: &AppRecord) {
        if app.stopped || daemon.is_busy(&app.name) {
            return;
        }
        let Some(revision) = git_revision(&app.source_dir).await else {
            return;
        };
        let deployed = app.revision.as_deref() == Some(revision.as_str());
        // Any deploy (webhook, pull, CLI) records the commit it tried, so a
        // commit that just failed isn't rebuilt here straight away.
        let attempted =
            daemon.attempted_revisions.lock().unwrap().get(&app.name) == Some(&revision);
        if !deployed && !attempted && app.status != AppStatus::New {
            daemon
                .attempted_revisions
                .lock()
                .unwrap()
                .insert(app.name.clone(), revision);
            daemon.spawn_deploy(app.name.clone(), Source::Folder, "new commit");
        }
    }
}

/// Makes reality match the database: listeners for every app, containers
/// running (or recreated), stray containers removed, tunnel routes current.
/// At `boot`, it also resumes deploys the daemon was in the middle of.
pub async fn reconcile(daemon: &Arc<Daemon>, boot: bool) {
    let apps = match daemon.store.apps() {
        Ok(apps) => apps,
        Err(error) => {
            warn!("could not read apps: {error:#}");
            return;
        }
    };
    let names: HashSet<&str> = apps.iter().map(|app| app.name.as_str()).collect();
    for app in &apps {
        let lock = daemon.lock(&app.name);
        let Ok(_guard) = lock.try_lock() else {
            continue;
        };
        if let Err(error) = reconcile_app(daemon, app, boot).await {
            warn!(app = app.name, "reconcile failed: {error:#}");
        }
    }
    for name in daemon.gateway.apps() {
        if !names.contains(name.as_str()) {
            daemon.gateway.remove_app(&name);
        }
    }
    daemon.sync_routes();
}

async fn reconcile_app(daemon: &Arc<Daemon>, app: &AppRecord, boot: bool) -> Result<()> {
    let manifest = Manifest::load(&app.source_dir).unwrap_or_default();
    let public = app.public.unwrap_or(manifest.public());
    let offline = |reason: &str| Upstream {
        port: None,
        public,
        reason: reason.to_string(),
    };

    if app.stopped {
        daemon
            .gateway
            .set_app(&app.name, offline("It was stopped with `hedgehog stop`."))
            .await?;
        return Ok(());
    }
    let Some(container) = &app.container else {
        let reason = match app.status {
            AppStatus::Failed => "Its last deploy failed.",
            _ => "It's being set up. Try again in a minute.",
        };
        daemon.gateway.set_app(&app.name, offline(reason)).await?;
        // A deploy that was interrupted by a daemon restart, or never started.
        if boot && matches!(app.status, AppStatus::New | AppStatus::Building) {
            daemon.spawn_deploy(app.name.clone(), Source::Folder, "unfinished deploy");
        }
        return Ok(());
    };

    match daemon.docker.state(container).await? {
        ContainerState::Running { .. } => {
            let port = daemon.docker.host_port(container).await?;
            daemon
                .gateway
                .set_app(
                    &app.name,
                    Upstream {
                        port,
                        public,
                        reason: "It's starting.".into(),
                    },
                )
                .await?;
            if app.status == AppStatus::Building {
                daemon
                    .store
                    .set_status(&app.name, AppStatus::Running, None)?;
            }
        }
        ContainerState::Stopped { .. } => {
            daemon
                .gateway
                .set_app(&app.name, offline("It's restarting."))
                .await?;
            daemon.docker.start(container).await?;
        }
        ContainerState::Missing => {
            daemon
                .gateway
                .set_app(&app.name, offline("It's being recreated."))
                .await?;
            daemon.store.set_container(&app.name, None)?;
            daemon.spawn_restart(app.name.clone());
            return Ok(());
        }
    }
    for other in daemon.docker.app_containers(&app.name).await? {
        if other != *container {
            daemon.docker.remove(&other).await?;
        }
    }
    Ok(())
}

fn maybe_backup(daemon: &Arc<Daemon>) {
    let last: i64 = daemon
        .store
        .setting(crate::backup::LAST)
        .ok()
        .flatten()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    if now() - last < BACKUP_EVERY {
        return;
    }
    // Recorded up front so a slow backup isn't started twice.
    let _ = daemon
        .store
        .set_setting(crate::backup::LAST, &now().to_string());
    let daemon = daemon.clone();
    tokio::spawn(async move {
        daemon.run_backup().await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_big_logs() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("daemon.log");
        assert!(!crate::rotate_log(&log, 10).unwrap(), "missing log");
        std::fs::write(&log, "short").unwrap();
        assert!(!crate::rotate_log(&log, 10).unwrap());
        std::fs::write(&log, "a line that is long enough").unwrap();
        assert!(crate::rotate_log(&log, 10).unwrap());
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("daemon.log.1")).unwrap(),
            "a line that is long enough"
        );
    }

    #[test]
    fn lists_visible_folders_only() {
        let root = tempfile::tempdir().unwrap();
        for dir in ["budget", ".incoming", "notes"] {
            std::fs::create_dir(root.path().join(dir)).unwrap();
        }
        std::fs::write(root.path().join("README.md"), "hi").unwrap();
        let names: Vec<_> = app_folders(root.path())
            .unwrap()
            .into_iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["budget", "notes"]);
    }

    #[test]
    fn fingerprint_changes_with_contents() {
        let root = tempfile::tempdir().unwrap();
        let before = fingerprint(root.path()).unwrap();
        std::fs::write(root.path().join("a.txt"), "a").unwrap();
        let after = fingerprint(root.path()).unwrap();
        assert_ne!(before, after);
        assert_eq!(after.entries, 1);
    }
}
