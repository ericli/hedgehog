use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const MANIFEST_FILE: &str = "hedgehog.toml";

/// The port apps are told to listen on (via `$PORT`) unless they say otherwise.
pub const DEFAULT_PORT: u16 = 8080;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Builder {
    Dockerfile,
    Railpack,
}

/// Optional `hedgehog.toml` in an app's folder. Everything has a default, so
/// most apps never need one.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// App name used by `hedgehog deploy`. Defaults to the folder name.
    pub name: Option<String>,
    /// Port the app listens on inside its container, if it ignores `$PORT`.
    pub port: Option<u16>,
    /// Skip the device check so anyone with the URL can open the app.
    pub public: Option<bool>,
    /// Give the app its own Postgres database (`$DATABASE_URL`). Default on.
    pub postgres: Option<bool>,
    /// Start command, when Railpack can't work it out.
    pub start: Option<String>,
    /// Force a builder instead of auto-detecting.
    pub build: Option<Builder>,
    /// Dockerfile path relative to the app folder. Defaults to `Dockerfile`.
    pub dockerfile: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl Manifest {
    /// Reads `hedgehog.toml` from `dir`, or returns the defaults if there is none.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(MANIFEST_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
        }
    }

    pub fn port(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_PORT)
    }

    pub fn postgres(&self) -> bool {
        self.postgres.unwrap_or(true)
    }

    pub fn public(&self) -> bool {
        self.public.unwrap_or(false)
    }

    pub fn dockerfile(&self) -> &str {
        self.dockerfile.as_deref().unwrap_or("Dockerfile")
    }

    /// Dockerfile when one exists (or is forced), Railpack otherwise.
    pub fn builder(&self, dir: &Path) -> Builder {
        self.build.unwrap_or_else(|| {
            if dir.join(self.dockerfile()).is_file() {
                Builder::Dockerfile
            } else {
                Builder::Railpack
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_manifest_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = Manifest::load(dir.path()).unwrap();
        assert_eq!(manifest, Manifest::default());
        assert_eq!(manifest.port(), DEFAULT_PORT);
        assert!(manifest.postgres());
        assert!(!manifest.public());
        assert_eq!(manifest.builder(dir.path()), Builder::Railpack);
    }

    #[test]
    fn parses_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(MANIFEST_FILE),
            r#"
                port = 3000
                public = true
                postgres = false
                start = "bun run start"
                [env]
                NODE_ENV = "production"
            "#,
        )
        .unwrap();
        let manifest = Manifest::load(dir.path()).unwrap();
        assert_eq!(manifest.port(), 3000);
        assert!(manifest.public());
        assert!(!manifest.postgres());
        assert_eq!(manifest.start.as_deref(), Some("bun run start"));
        assert_eq!(manifest.env["NODE_ENV"], "production");
    }

    #[test]
    fn rejects_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(MANIFEST_FILE), "prot = 3000").unwrap();
        assert!(Manifest::load(dir.path()).is_err());
    }

    #[test]
    fn detects_dockerfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM scratch").unwrap();
        assert_eq!(Manifest::default().builder(dir.path()), Builder::Dockerfile);
        let forced = Manifest {
            build: Some(Builder::Railpack),
            ..Default::default()
        };
        assert_eq!(forced.builder(dir.path()), Builder::Railpack);
    }
}
