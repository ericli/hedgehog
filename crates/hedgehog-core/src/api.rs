//! JSON types for the daemon's control API (`/v1/...`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    pub tunnel: TunnelInfo,
    /// The hedgehog home page (app list, device pairing).
    pub home_url: Option<String>,
    pub apps: usize,
    pub docker: bool,
    pub railpack: bool,
    pub apps_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelInfo {
    /// `off`, `starting`, `connecting`, `connected`, `reconnecting`, or `stopped`.
    pub state: String,
    pub hostname: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppStatus {
    New,
    Building,
    Running,
    Failed,
    Stopped,
}

impl AppStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Building => "building",
            Self::Running => "running",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "building" => Self::Building,
            "running" => Self::Running,
            "failed" => Self::Failed,
            "stopped" => Self::Stopped,
            _ => Self::New,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInfo {
    pub name: String,
    pub status: AppStatus,
    pub url: Option<String>,
    pub public: bool,
    pub source_dir: String,
    /// The folder is a git repo, so it can be pulled and redeployed.
    #[serde(default)]
    pub git: bool,
    pub builder: Option<String>,
    /// Git commit the running version was built from, when the folder is a repo.
    pub revision: Option<String>,
    /// Unix seconds.
    pub deployed_at: Option<i64>,
    pub last_error: Option<String>,
}

/// One line of the NDJSON stream returned while deploying.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum DeployEvent {
    Step { message: String },
    Log { line: String },
    Done { url: Option<String> },
    Failed { error: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvUpdate {
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    #[serde(default)]
    pub unset: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicUpdate {
    pub public: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PairRequest {
    pub label: Option<String>,
    /// Let the device manage deployments from the dashboard, not just open apps.
    #[serde(default)]
    pub admin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairResponse {
    pub url: String,
    pub expires_in_secs: u64,
    /// The URL as a QR code, for the dashboard.
    #[serde(default)]
    pub qr_svg: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub label: Option<String>,
    /// Can use the dashboard.
    #[serde(default)]
    pub admin: bool,
    /// Unix seconds.
    pub created_at: i64,
    /// The device making this request (dashboard only).
    #[serde(default)]
    pub current: bool,
}

/// Create an app by cloning a Git repository on the hedgehog Mac.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateApp {
    pub git_url: String,
    pub name: String,
    /// Branch to clone and deploy. Defaults to the repository's default branch.
    #[serde(default)]
    pub branch: Option<String>,
}

/// Where GitHub should send push events for an app, and the secret it signs them with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookInfo {
    pub url: String,
    pub secret: String,
    /// Pushes to this branch deploy; others are ignored.
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}
