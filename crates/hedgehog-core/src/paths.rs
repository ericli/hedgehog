use std::path::PathBuf;

use anyhow::{Context, Result};

/// Where hedgehog keeps things on the machine that runs the daemon.
#[derive(Debug, Clone)]
pub struct Paths {
    /// Folders dropped here become apps: `~/Hedgehog/apps/<name>`.
    pub apps: PathBuf,
    /// Persistent data mounted into each app at `/data`: `~/Hedgehog/data/<name>`.
    pub data: PathBuf,
    /// Daemon state: database, API token, tunnel identity, logs, backups.
    pub state: PathBuf,
}

impl Paths {
    /// Uses `$HEDGEHOG_HOME` when set (handy for tests and trying things out),
    /// otherwise `~/Hedgehog` for apps and data and the platform data dir
    /// (`~/Library/Application Support/hedgehog` on macOS) for state.
    pub fn from_env() -> Result<Self> {
        if let Some(root) = std::env::var_os("HEDGEHOG_HOME") {
            return Ok(Self::under(PathBuf::from(root)));
        }
        let home = dirs::home_dir().context("could not find your home directory")?;
        let state = dirs::data_dir()
            .map(|dir| dir.join("hedgehog"))
            .unwrap_or_else(|| home.join(".hedgehog"));
        Ok(Self {
            apps: home.join("Hedgehog").join("apps"),
            data: home.join("Hedgehog").join("data"),
            state,
        })
    }

    pub fn under(root: PathBuf) -> Self {
        Self {
            apps: root.join("apps"),
            data: root.join("data"),
            state: root.join("state"),
        }
    }

    pub fn database(&self) -> PathBuf {
        self.state.join("state.db")
    }

    /// Settings for the local dev Postgres (`hedgehog db up`).
    pub fn dev_database(&self) -> PathBuf {
        self.state.join("dev.db")
    }

    pub fn token(&self) -> PathBuf {
        self.state.join("token")
    }

    pub fn tunnel(&self) -> PathBuf {
        self.state.join("opentunnel")
    }

    pub fn logs(&self) -> PathBuf {
        self.state.join("logs")
    }

    pub fn backups(&self) -> PathBuf {
        self.state.join("backups")
    }

    pub fn daemon_log(&self) -> PathBuf {
        self.state.join("daemon.log")
    }

    /// Remotes the CLI knows about.
    pub fn remotes(&self) -> PathBuf {
        self.state.join("remotes.toml")
    }

    /// Uploads land here before replacing an app's folder. Dot-folders are
    /// ignored by the apps scanner.
    pub fn incoming(&self) -> PathBuf {
        self.apps.join(".incoming")
    }

    pub fn app_source(&self, app: &str) -> PathBuf {
        self.apps.join(app)
    }

    pub fn app_data(&self, app: &str) -> PathBuf {
        self.data.join(app)
    }
}
