//! Backups: each app's database (`pg_dump`) and its `/data` folder, written
//! nightly to `<state>/backups/<app>/` and, if you set a destination, copied
//! there too (an external disk or iCloud Drive), so a dead SSD doesn't take
//! everything with it. The newest few of each kind are kept in both places.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use hedgehog_core::api::BackupReport;
use tracing::{info, warn};

use crate::Daemon;
use crate::secrets::now;
use crate::store::Store;

/// How many backups of each kind to keep per app.
pub const KEPT: usize = 7;
const DUMP_SUFFIX: &str = ".dump";
const DATA_SUFFIX: &str = "-data.tar.gz";
const DESTINATION: &str = "backup.destination";
pub const LAST: &str = "backup.last";
const LAST_ERROR: &str = "backup.last_error";

pub fn destination(store: &Store) -> Option<PathBuf> {
    store
        .setting(DESTINATION)
        .ok()
        .flatten()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

pub fn last_error(store: &Store) -> Option<String> {
    store
        .setting(LAST_ERROR)
        .ok()
        .flatten()
        .filter(|error| !error.is_empty())
}

/// Sets (or with `None`, clears) the folder backups are also copied to.
pub fn set_destination(store: &Store, folder: Option<&str>) -> Result<()> {
    let Some(folder) = folder else {
        return store.delete_setting(DESTINATION);
    };
    let expanded = match (folder.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest).display().to_string(),
        _ => folder.to_string(),
    };
    let folder = expanded.as_str();
    let path = Path::new(folder);
    if !path.is_absolute() {
        bail!("the backup folder must be an absolute path, like /Volumes/Backup/hedgehog");
    }
    std::fs::create_dir_all(path).with_context(|| {
        format!(
            "could not create {}. If it's on an external disk or in iCloud Drive, macOS may need to allow it: \
             System Settings → Privacy & Security → Full Disk Access → add the hedgehog binary",
            path.display()
        )
    })?;
    store.set_setting(DESTINATION, folder)
}

/// Packs a folder into a `.tar.gz`. Returns false (and writes nothing) if
/// the folder is missing or empty.
pub fn archive_dir(source: &Path, archive: &Path) -> Result<bool> {
    let empty = std::fs::read_dir(source).map_or(true, |mut entries| entries.next().is_none());
    if empty {
        return Ok(false);
    }
    let file =
        File::create(archive).with_context(|| format!("could not write {}", archive.display()))?;
    let mut tar = tar::Builder::new(GzEncoder::new(file, Compression::default()));
    tar.follow_symlinks(false);
    tar.append_dir_all(".", source)
        .with_context(|| format!("could not archive {}", source.display()))?;
    tar.into_inner()?.finish()?;
    Ok(true)
}

/// Keeps the newest [`KEPT`] dumps and the newest [`KEPT`] data archives.
pub fn prune(dir: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    let names: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect();
    for suffix in [DUMP_SUFFIX, DATA_SUFFIX] {
        let mut kind: Vec<&String> = names.iter().filter(|name| name.ends_with(suffix)).collect();
        // Names start with a Unix timestamp, so they sort oldest first.
        kind.sort();
        let excess = kind.len().saturating_sub(KEPT);
        for old in kind.into_iter().take(excess) {
            std::fs::remove_file(dir.join(old))?;
        }
    }
    Ok(())
}

/// Copies an app's new backup files into `<destination>/<app>/` and prunes it.
pub fn copy_to(destination: &Path, app: &str, files: &[PathBuf]) -> Result<()> {
    let dir = destination.join(app);
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    for file in files {
        let name = file.file_name().context("backup file has no name")?;
        std::fs::copy(file, dir.join(name))
            .with_context(|| format!("could not copy {} to {}", file.display(), dir.display()))?;
    }
    prune(&dir)
}

impl Daemon {
    /// Backs up every app now. Problems with one app don't stop the others;
    /// they're collected in the report and remembered for `hedgehog status`.
    pub async fn run_backup(&self) -> BackupReport {
        let stamp = now();
        let destination = destination(&self.store);
        let mut report = BackupReport {
            destination: destination.as_ref().map(|path| path.display().to_string()),
            ..Default::default()
        };
        let apps = match self.store.apps() {
            Ok(apps) => apps,
            Err(error) => {
                report.errors.push(format!("{error:#}"));
                return report;
            }
        };
        for app in apps {
            let name = app.name.as_str();
            let dir = self.paths.backups().join(name);
            if let Err(error) = std::fs::create_dir_all(&dir) {
                report.errors.push(format!("{name}: {error}"));
                continue;
            }
            let mut written = Vec::new();
            match self.postgres.dump(name).await {
                Ok(Some(dump)) => {
                    let path = dir.join(format!("{stamp}{DUMP_SUFFIX}"));
                    match std::fs::write(&path, dump) {
                        Ok(()) => written.push(path),
                        Err(error) => report.errors.push(format!("{name}: database: {error}")),
                    }
                }
                Ok(None) => {}
                Err(error) => report.errors.push(format!("{name}: database: {error:#}")),
            }
            let data = self.paths.app_data(name);
            let archive = dir.join(format!("{stamp}{DATA_SUFFIX}"));
            let target = archive.clone();
            match tokio::task::spawn_blocking(move || archive_dir(&data, &target)).await {
                Ok(Ok(true)) => written.push(archive),
                Ok(Ok(false)) => {}
                Ok(Err(error)) => report.errors.push(format!("{name}: data: {error:#}")),
                Err(error) => report.errors.push(format!("{name}: data: {error}")),
            }
            if let Err(error) = prune(&dir) {
                report.errors.push(format!("{name}: {error:#}"));
            }
            if let Some(destination) = &destination
                && !written.is_empty()
                && let Err(error) = copy_to(destination, name, &written)
            {
                report.errors.push(format!("{name}: copying: {error:#}"));
            }
            report
                .files
                .extend(written.iter().map(|path| path.display().to_string()));
        }
        let _ = self.store.set_setting(LAST, &stamp.to_string());
        let _ = if report.errors.is_empty() {
            self.store.delete_setting(LAST_ERROR)
        } else {
            self.store
                .set_setting(LAST_ERROR, &report.errors.join("; "))
        };
        if report.errors.is_empty() {
            info!(files = report.files.len(), "backup finished");
        } else {
            warn!(errors = %report.errors.join("; "), "backup finished with problems");
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archives_folders_and_skips_empty_ones() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let archive = root.path().join("out.tar.gz");
        assert!(!archive_dir(&data, &archive).unwrap(), "missing folder");
        std::fs::create_dir_all(&data).unwrap();
        assert!(!archive_dir(&data, &archive).unwrap(), "empty folder");
        assert!(!archive.exists());

        std::fs::create_dir_all(data.join("db")).unwrap();
        std::fs::write(data.join("db/app.sqlite"), "rows").unwrap();
        assert!(archive_dir(&data, &archive).unwrap());
        let unpacked = root.path().join("unpacked");
        tar::Archive::new(flate2::read::GzDecoder::new(File::open(&archive).unwrap()))
            .unpack(&unpacked)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(unpacked.join("db/app.sqlite")).unwrap(),
            "rows"
        );
    }

    #[test]
    fn prunes_each_kind_separately() {
        let dir = tempfile::tempdir().unwrap();
        for stamp in 100..110 {
            std::fs::write(dir.path().join(format!("{stamp}{DUMP_SUFFIX}")), "x").unwrap();
        }
        for stamp in 200..203 {
            std::fs::write(dir.path().join(format!("{stamp}{DATA_SUFFIX}")), "x").unwrap();
        }
        prune(dir.path()).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left.iter()
                .filter(|name| name.ends_with(DUMP_SUFFIX))
                .count(),
            KEPT
        );
        assert_eq!(
            left.iter()
                .filter(|name| name.ends_with(DATA_SUFFIX))
                .count(),
            3
        );
        assert_eq!(left[0], "103.dump");
    }

    #[test]
    fn copies_to_a_destination_and_prunes_it() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("external");
        for stamp in 0..9 {
            let file = root.path().join(format!("{stamp}{DUMP_SUFFIX}"));
            std::fs::write(&file, stamp.to_string()).unwrap();
            copy_to(&destination, "budget", &[file]).unwrap();
        }
        let copies = std::fs::read_dir(destination.join("budget"))
            .unwrap()
            .count();
        assert_eq!(copies, KEPT);
        assert_eq!(
            std::fs::read_to_string(destination.join("budget/8.dump")).unwrap(),
            "8"
        );
    }

    #[test]
    fn destination_must_be_absolute() {
        let store = Store::open_in_memory().unwrap();
        assert!(set_destination(&store, Some("relative/path")).is_err());
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("backups");
        set_destination(&store, Some(folder.to_str().unwrap())).unwrap();
        assert_eq!(destination(&store), Some(folder.clone()));
        assert!(folder.is_dir());
        set_destination(&store, None).unwrap();
        assert_eq!(destination(&store), None);
    }
}
