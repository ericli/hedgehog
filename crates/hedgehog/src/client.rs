//! Talking to a hedgehog daemon: the local one, or a remote one (the Mac at
//! home) reached over Tailscale.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use hedgehog_core::api::ErrorBody;
use hedgehog_core::{API_PORT, Paths};
use reqwest::{Method, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RemotesFile {
    pub default: Option<String>,
    #[serde(default)]
    pub remotes: BTreeMap<String, Remote>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Remote {
    pub url: String,
    pub token: String,
}

impl RemotesFile {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(toml::to_string_pretty(self)?.as_bytes())?;
        Ok(())
    }
}

pub struct Client {
    base: String,
    token: String,
    http: reqwest::Client,
    /// Talking to the daemon on this machine (enables `db`, `token`, ...).
    pub local: bool,
    /// The saved remote's name, when talking to one.
    pub remote: Option<String>,
    /// What to call this daemon in messages.
    pub label: String,
}

impl Client {
    /// `--remote`/`$HEDGEHOG_REMOTE`, else the default remote, else this Mac.
    pub fn resolve(paths: &Paths, remote: Option<&str>) -> Result<Self> {
        let remotes = RemotesFile::load(&paths.remotes())?;
        let chosen = remote.map(str::to_string).or(remotes.default.clone());
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        if let Some(name) = chosen.filter(|name| name != "local") {
            let entry = remotes
                .remotes
                .get(&name)
                .with_context(|| format!("no remote called `{name}`. See `hedgehog remote ls`"))?;
            return Ok(Self {
                base: entry.url.trim_end_matches('/').to_string(),
                token: entry.token.clone(),
                http,
                local: false,
                remote: Some(name.clone()),
                label: name,
            });
        }
        let token = std::fs::read_to_string(paths.token())
            .map(|token| token.trim().to_string())
            .map_err(|_| {
                anyhow!(
                    "hedgehog isn't set up on this Mac. Run `hedgehog setup` here, or point at your home Mac with \
                     `hedgehog remote add home http://<mac>:{API_PORT} --token <token>`"
                )
            })?;
        Ok(Self {
            base: format!("http://127.0.0.1:{API_PORT}"),
            token,
            http,
            local: true,
            remote: None,
            label: "this Mac".to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        build: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) -> Result<Response> {
        let request = build(
            self.http
                .request(method, self.url(path))
                .bearer_auth(&self.token),
        );
        let response = request.send().await.map_err(|error| {
            if error.is_connect() || error.is_timeout() {
                anyhow!(
                    "could not reach hedgehog on {} ({}). Is the daemon running? Try `hedgehog status`",
                    self.label,
                    self.base
                )
            } else {
                anyhow!(error)
            }
        })?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<ErrorBody>(&text)
            .map(|body| body.error)
            .unwrap_or_else(|_| format!("{status}: {text}"));
        bail!(message)
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Ok(self
            .request(Method::GET, path, |request| request)
            .await?
            .json()
            .await?)
    }

    pub async fn send<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: &B,
    ) -> Result<Response> {
        self.request(method, path, |request| request.json(body))
            .await
    }

    pub async fn send_empty(&self, method: Method, path: &str) -> Result<Response> {
        self.request(method, path, |request| request).await
    }
}
