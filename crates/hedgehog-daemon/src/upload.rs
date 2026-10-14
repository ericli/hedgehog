//! Swapping an uploaded snapshot in as an app's folder, and moving folders
//! out of the way when an app is removed.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use hedgehog_core::Paths;

use crate::secrets::random_token;

/// Unpacks a `.tar.gz` and atomically-enough replaces `dest` with it. Work
/// happens in `apps/.incoming`, which the folder scanner ignores.
pub fn replace_folder(paths: &Paths, archive: &Path, dest: &Path) -> Result<()> {
    let incoming = paths.incoming();
    std::fs::create_dir_all(&incoming)?;
    let staging = incoming.join(format!("new-{}", random_token(4)));
    let mut tar = tar::Archive::new(GzDecoder::new(File::open(archive)?));
    tar.set_overwrite(true);
    tar.unpack(&staging)
        .context("could not unpack the upload")?;
    if dest.exists() {
        let old = incoming.join(format!("old-{}", random_token(4)));
        std::fs::rename(dest, &old)?;
        std::fs::rename(&staging, dest)?;
        std::fs::remove_dir_all(&old)?;
    } else {
        std::fs::rename(&staging, dest)?;
    }
    Ok(())
}

/// Moves a folder to `~/.Trash` (so it can be recovered), falling back to
/// `apps/.trash` if macOS won't let us.
pub fn move_to_trash(paths: &Paths, dir: &Path) -> Result<PathBuf> {
    move_to(dir, system_trash().as_deref(), &paths.apps.join(".trash"))
}

/// `~/.Trash`, unless hedgehog is running against a scratch `$HEDGEHOG_HOME`.
fn system_trash() -> Option<PathBuf> {
    if std::env::var_os("HEDGEHOG_HOME").is_some() {
        return None;
    }
    let trash = PathBuf::from(std::env::var_os("HOME")?).join(".Trash");
    trash.is_dir().then_some(trash)
}

fn move_to(dir: &Path, preferred: Option<&Path>, fallback: &Path) -> Result<PathBuf> {
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "app".to_string());
    let unique = format!("{name}-hedgehog-{}", random_token(3));
    if let Some(preferred) = preferred {
        let target = preferred.join(&unique);
        if std::fs::rename(dir, &target).is_ok() {
            return Ok(target);
        }
    }
    std::fs::create_dir_all(fallback)?;
    let target = fallback.join(unique);
    std::fs::rename(dir, &target)
        .with_context(|| format!("could not move {} out of the way", dir.display()))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    fn archive_with(dir: &Path, files: &[(&str, &str)]) -> PathBuf {
        let path = dir.join("upload.tar.gz");
        let mut builder = tar::Builder::new(GzEncoder::new(
            File::create(&path).unwrap(),
            Compression::fast(),
        ));
        for (name, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, contents.as_bytes())
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        path
    }

    #[test]
    fn replaces_folder_contents() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path().to_path_buf());
        let dest = paths.app_source("budget");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("stale.txt"), "old").unwrap();

        let archive = archive_with(root.path(), &[("index.js", "new"), ("src/app.js", "app")]);
        replace_folder(&paths, &archive, &dest).unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("index.js")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("src/app.js")).unwrap(),
            "app"
        );
        assert!(!dest.join("stale.txt").exists());
        let leftovers: Vec<_> = std::fs::read_dir(paths.incoming()).unwrap().collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn moves_folders_out_of_the_way() {
        let root = tempfile::tempdir().unwrap();
        let trash = root.path().join("trash");
        let fallback = root.path().join("fallback");
        std::fs::create_dir_all(&trash).unwrap();

        let dir = root.path().join("budget");
        std::fs::create_dir_all(&dir).unwrap();
        let moved = move_to(&dir, Some(&trash), &fallback).unwrap();
        assert!(moved.starts_with(&trash) && moved.exists() && !dir.exists());

        std::fs::create_dir_all(&dir).unwrap();
        let missing = root.path().join("no-such-trash");
        let moved = move_to(&dir, Some(&missing), &fallback).unwrap();
        assert!(moved.starts_with(&fallback) && moved.exists());
    }
}
