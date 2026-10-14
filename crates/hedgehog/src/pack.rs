//! Packs an app folder into a `.tar.gz` for upload, skipping whatever git
//! would ignore (so `node_modules`, build output, and secrets in `.env` stay
//! home unless you say otherwise).

use std::path::Path;

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use ignore::WalkBuilder;

/// Never worth uploading, even when not gitignored.
const ALWAYS_SKIP: &[&str] = &[".git", "node_modules", ".venv", "__pycache__", ".DS_Store"];

pub struct Packed {
    pub bytes: Vec<u8>,
    pub files: usize,
    /// There is a `.env` that was left out because it's ignored.
    pub skipped_env: bool,
}

pub fn pack(dir: &Path) -> Result<Packed> {
    let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    archive.follow_symlinks(false);
    let walker = WalkBuilder::new(dir)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .require_git(false)
        .add_custom_ignore_filename(".hedgehogignore")
        .filter_entry(|entry| !ALWAYS_SKIP.iter().any(|skip| entry.file_name() == *skip))
        .build();
    let mut files = 0;
    let mut included_env = false;
    for entry in walker {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(dir)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            archive.append_dir(relative, path)?;
        } else {
            archive
                .append_path_with_name(path, relative)
                .with_context(|| format!("could not add {}", path.display()))?;
            files += 1;
            included_env |= relative == Path::new(".env");
        }
    }
    let bytes = archive.into_inner()?.finish()?;
    Ok(Packed {
        bytes,
        files,
        skipped_env: dir.join(".env").is_file() && !included_env,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use flate2::read::GzDecoder;

    use super::*;

    fn names(packed: &Packed) -> BTreeSet<String> {
        let mut archive = tar::Archive::new(GzDecoder::new(&packed.bytes[..]));
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                entry
                    .unwrap()
                    .path()
                    .unwrap()
                    .to_string_lossy()
                    .trim_end_matches('/')
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn respects_gitignore_and_skips_junk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "dist/\n.env\n").unwrap();
        std::fs::write(root.join("index.js"), "console.log(1)").unwrap();
        std::fs::write(root.join(".env"), "SECRET=1").unwrap();
        std::fs::create_dir_all(root.join("dist")).unwrap();
        std::fs::write(root.join("dist/out.js"), "built").unwrap();
        std::fs::create_dir_all(root.join("node_modules/x")).unwrap();
        std::fs::write(root.join("node_modules/x/index.js"), "dep").unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/app.js"), "app").unwrap();

        let packed = pack(root).unwrap();
        let names = names(&packed);
        assert!(names.contains("index.js"));
        assert!(names.contains("src/app.js"));
        assert!(names.contains(".gitignore"));
        assert!(!names.iter().any(|name| name.starts_with("dist")));
        assert!(!names.iter().any(|name| name.starts_with("node_modules")));
        assert!(
            !names
                .iter()
                .any(|name| name.starts_with(".git/") || name == ".git")
        );
        assert!(!names.contains(".env"));
        assert!(packed.skipped_env);
        assert_eq!(packed.files, 3);
    }
}
