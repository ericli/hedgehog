//! Container lifecycle through the Docker API (bollard), plus the `docker`
//! CLI for the bits the API makes awkward (buildx builds, `exec` with stdin).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use bollard::models::{
    ContainerCreateBody, HostConfig, NetworkCreateRequest, PortBinding, RestartPolicy,
    RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, InspectNetworkOptions,
    ListContainersOptionsBuilder, ListImagesOptionsBuilder, LogsOptionsBuilder,
    RemoveContainerOptionsBuilder, RemoveImageOptionsBuilder, StopContainerOptionsBuilder,
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt};
use hedgehog_core::env::EnvMap;
use tokio::io::AsyncWriteExt;

pub const NETWORK: &str = "hedgehog";
pub const LABEL_APP: &str = "dev.hedgehog.app";
pub const LABEL_DEPLOY: &str = "dev.hedgehog.deploy";

#[derive(Clone)]
pub struct Docker {
    api: bollard::Docker,
    /// Passed to `docker` CLI calls so they talk to the same engine.
    host: String,
    /// `DOCKER_CONFIG` for CLI calls; see [`prepare_cli_config`].
    config: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    Missing,
    /// `restarts` counts Docker restarting it after crashes.
    Running {
        restarts: i64,
    },
    Stopped {
        exit_code: Option<i64>,
    },
}

/// Everything needed to start one app container.
pub struct RunSpec<'a> {
    pub app: &'a str,
    pub deploy_id: &'a str,
    pub image: &'a str,
    pub env: &'a EnvMap,
    pub port: u16,
    pub data_dir: &'a Path,
}

pub fn container_name(app: &str, deploy_id: &str) -> String {
    format!("hedgehog-{app}-{deploy_id}")
}

pub fn image_name(app: &str, deploy_id: &str) -> String {
    format!("hedgehog/{app}:{deploy_id}")
}

/// `$DOCKER_HOST`, else the active `docker context`, else the default socket.
/// Docker Desktop and OrbStack each put their socket somewhere different.
fn resolve_host() -> String {
    if let Ok(host) = std::env::var("DOCKER_HOST")
        && !host.is_empty()
    {
        return host;
    }
    let from_context = std::process::Command::new("docker")
        .args([
            "context",
            "inspect",
            "--format",
            "{{.Endpoints.docker.Host}}",
        ])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|host| host.starts_with("unix://"));
    from_context.unwrap_or_else(|| "unix:///var/run/docker.sock".to_string())
}

/// Docker Desktop's credential helper asks the macOS keychain, which can
/// block forever on a permission prompt when called from a background
/// service. So hedgehog's `docker` calls use a copy of your Docker config
/// without credential helpers (public images don't need them), keeping your
/// CLI plugins such as buildx. `HEDGEHOG_DOCKER_CREDENTIALS=1` opts out.
pub fn prepare_cli_config(dir: &Path) -> Result<Option<PathBuf>> {
    if std::env::var_os("HEDGEHOG_DOCKER_CREDENTIALS").is_some() {
        return Ok(None);
    }
    let user_dir = match std::env::var_os("DOCKER_CONFIG") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".docker"),
    };
    write_cli_config(&user_dir, dir)?;
    Ok(Some(dir.to_path_buf()))
}

fn write_cli_config(user_dir: &Path, dir: &Path) -> Result<()> {
    let mut config: serde_json::Map<String, serde_json::Value> =
        std::fs::read(user_dir.join("config.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
    for key in ["credsStore", "credHelpers", "currentContext"] {
        config.remove(key);
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("config.json"), serde_json::to_vec_pretty(&config)?)?;
    let plugins = dir.join("cli-plugins");
    if std::fs::symlink_metadata(&plugins).is_ok() {
        std::fs::remove_file(&plugins)?;
    }
    let user_plugins = user_dir.join("cli-plugins");
    if user_plugins.is_dir() {
        std::os::unix::fs::symlink(&user_plugins, &plugins)?;
    }
    Ok(())
}

impl Docker {
    pub fn connect() -> Result<Self> {
        let host = resolve_host();
        let api = bollard::Docker::connect_with_socket(&host, 120, bollard::API_DEFAULT_VERSION)
            .with_context(|| {
                format!("could not connect to Docker at {host}. Is Docker running?")
            })?;
        Ok(Self {
            api,
            host,
            config: None,
        })
    }

    pub fn set_cli_config(&mut self, config: Option<PathBuf>) {
        self.config = config;
    }

    pub fn api(&self) -> &bollard::Docker {
        &self.api
    }

    pub async fn ping(&self) -> bool {
        self.api.ping().await.is_ok()
    }

    /// A `docker` CLI command pointed at the same engine.
    pub fn cli(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("docker");
        command.env("DOCKER_HOST", &self.host);
        if let Some(config) = &self.config {
            command.env("DOCKER_CONFIG", config);
        }
        command
    }

    pub async fn ensure_network(&self) -> Result<()> {
        if self
            .api
            .inspect_network(NETWORK, None::<InspectNetworkOptions>)
            .await
            .is_ok()
        {
            return Ok(());
        }
        self.api
            .create_network(NetworkCreateRequest {
                name: NETWORK.to_string(),
                driver: Some("bridge".to_string()),
                ..Default::default()
            })
            .await
            .context("could not create the hedgehog Docker network")?;
        Ok(())
    }

    pub async fn image_exists(&self, image: &str) -> bool {
        self.api.inspect_image(image).await.is_ok()
    }

    pub async fn pull(&self, image: &str) -> Result<()> {
        let (name, tag) = image.rsplit_once(':').unwrap_or((image, "latest"));
        let options = CreateImageOptionsBuilder::default()
            .from_image(name)
            .tag(tag)
            .build();
        self.api
            .create_image(Some(options), None, None)
            .try_collect::<Vec<_>>()
            .await
            .with_context(|| format!("could not pull {image}"))?;
        Ok(())
    }

    /// Creates and starts an app container with its port published on an
    /// ephemeral `127.0.0.1` port. Returns the container name.
    pub async fn run_app(&self, spec: &RunSpec<'_>) -> Result<String> {
        let name = container_name(spec.app, spec.deploy_id);
        let port_key = format!("{}/tcp", spec.port);
        std::fs::create_dir_all(spec.data_dir)
            .with_context(|| format!("could not create {}", spec.data_dir.display()))?;
        let body = ContainerCreateBody {
            image: Some(spec.image.to_string()),
            env: Some(
                spec.env
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect(),
            ),
            exposed_ports: Some(vec![port_key.clone()]),
            labels: Some(HashMap::from([
                (LABEL_APP.to_string(), spec.app.to_string()),
                (LABEL_DEPLOY.to_string(), spec.deploy_id.to_string()),
            ])),
            host_config: Some(HostConfig {
                port_bindings: Some(HashMap::from([(
                    port_key,
                    Some(vec![PortBinding {
                        host_ip: Some("127.0.0.1".to_string()),
                        host_port: Some(String::new()),
                    }]),
                )])),
                binds: Some(vec![format!("{}:/data", spec.data_dir.display())]),
                restart_policy: Some(RestartPolicy {
                    name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: None,
                }),
                network_mode: Some(NETWORK.to_string()),
                init: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let options = CreateContainerOptionsBuilder::default().name(&name).build();
        self.api
            .create_container(Some(options), body)
            .await
            .with_context(|| format!("could not create container {name}"))?;
        self.api
            .start_container(&name, None)
            .await
            .with_context(|| format!("could not start container {name}"))?;
        Ok(name)
    }

    pub async fn state(&self, container: &str) -> Result<ContainerState> {
        match self.api.inspect_container(container, None).await {
            Ok(info) => {
                let state = info.state.unwrap_or_default();
                let up = state.running.unwrap_or(false) && !state.restarting.unwrap_or(false);
                Ok(if up {
                    ContainerState::Running {
                        restarts: info.restart_count.unwrap_or(0),
                    }
                } else {
                    ContainerState::Stopped {
                        exit_code: state.exit_code,
                    }
                })
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(ContainerState::Missing),
            Err(error) => Err(error).context("could not inspect container"),
        }
    }

    /// The `127.0.0.1` port Docker published the app on. It can change when
    /// the container restarts, so callers re-check it.
    pub async fn host_port(&self, container: &str) -> Result<Option<u16>> {
        let info = self.api.inspect_container(container, None).await?;
        let port = info
            .network_settings
            .and_then(|settings| settings.ports)
            .into_iter()
            .flat_map(|ports| ports.into_values())
            .flatten()
            .flatten()
            .find_map(|binding| binding.host_port.and_then(|port| port.parse().ok()));
        Ok(port)
    }

    /// The host port a container's first port binding asked for, whether or
    /// not it is running.
    pub async fn published_port(&self, container: &str) -> Result<Option<u16>> {
        let info = self.api.inspect_container(container, None).await?;
        let port = info
            .host_config
            .and_then(|config| config.port_bindings)
            .into_iter()
            .flat_map(|ports| ports.into_values())
            .flatten()
            .flatten()
            .find_map(|binding| binding.host_port.and_then(|port| port.parse().ok()));
        Ok(port)
    }

    pub async fn start(&self, container: &str) -> Result<()> {
        self.api
            .start_container(container, None)
            .await
            .with_context(|| format!("could not start {container}"))
    }

    pub async fn stop(&self, container: &str) -> Result<()> {
        let options = StopContainerOptionsBuilder::default().t(10).build();
        match self.api.stop_container(container, Some(options)).await {
            Ok(())
            | Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 304 | 404,
                ..
            }) => Ok(()),
            Err(error) => Err(error).with_context(|| format!("could not stop {container}")),
        }
    }

    pub async fn remove(&self, container: &str) -> Result<()> {
        let options = RemoveContainerOptionsBuilder::default().force(true).build();
        match self.api.remove_container(container, Some(options)).await {
            Ok(())
            | Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(()),
            Err(error) => Err(error).with_context(|| format!("could not remove {container}")),
        }
    }

    /// Names of every container hedgehog created for `app`, running or not.
    pub async fn app_containers(&self, app: &str) -> Result<Vec<String>> {
        let label = format!("{LABEL_APP}={app}");
        let filters = HashMap::from([("label", vec![label.as_str()])]);
        let options = ListContainersOptionsBuilder::default()
            .all(true)
            .filters(&filters)
            .build();
        let containers = self.api.list_containers(Some(options)).await?;
        Ok(containers
            .into_iter()
            .filter_map(|container| container.names?.into_iter().next())
            .map(|name| name.trim_start_matches('/').to_string())
            .collect())
    }

    /// Removes an app's old images, keeping the ones listed.
    pub async fn remove_images_except(&self, app: &str, keep: &[&str]) -> Result<()> {
        let reference = format!("hedgehog/{app}");
        let filters = HashMap::from([("reference", vec![reference.as_str()])]);
        let options = ListImagesOptionsBuilder::default()
            .filters(&filters)
            .build();
        for image in self.api.list_images(Some(options)).await? {
            for tag in image.repo_tags {
                if !keep.contains(&tag.as_str()) {
                    let options = RemoveImageOptionsBuilder::default().build();
                    let _ = self.api.remove_image(&tag, Some(options), None).await;
                }
            }
        }
        Ok(())
    }

    pub fn logs(
        &self,
        container: &str,
        follow: bool,
        tail: usize,
    ) -> impl Stream<Item = Result<Bytes>> + use<> {
        let options = LogsOptionsBuilder::default()
            .follow(follow)
            .stdout(true)
            .stderr(true)
            .tail(&tail.to_string())
            .build();
        self.api.logs(container, Some(options)).map(|item| {
            item.map(|output| output.into_bytes())
                .map_err(anyhow::Error::from)
        })
    }

    /// Runs a command in a container with `stdin` piped in; returns stdout.
    pub async fn exec(&self, container: &str, command: &[&str], stdin: &[u8]) -> Result<Vec<u8>> {
        let mut child = self
            .cli()
            .arg("exec")
            .arg("-i")
            .arg(container)
            .args(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("could not run docker. Is the Docker CLI installed?")?;
        let mut input = child.stdin.take().context("no stdin")?;
        input.write_all(stdin).await?;
        drop(input);
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            bail!(
                "`{}` failed: {}",
                command.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_config_drops_credential_helpers_and_keeps_plugins() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        std::fs::create_dir_all(user.join("cli-plugins")).unwrap();
        std::fs::write(
            user.join("config.json"),
            r#"{"auths":{"ghcr.io":{"auth":"abc"}},"credsStore":"desktop","credHelpers":{"x":"y"},"currentContext":"desktop-linux"}"#,
        )
        .unwrap();
        let ours = root.path().join("ours");
        write_cli_config(&user, &ours).unwrap();
        // Running it again (every daemon start) must also work.
        write_cli_config(&user, &ours).unwrap();

        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(ours.join("config.json")).unwrap()).unwrap();
        assert_eq!(config["auths"]["ghcr.io"]["auth"], "abc");
        assert!(config.get("credsStore").is_none());
        assert!(config.get("credHelpers").is_none());
        assert!(config.get("currentContext").is_none());
        assert_eq!(
            std::fs::read_link(ours.join("cli-plugins")).unwrap(),
            user.join("cli-plugins")
        );
    }

    #[test]
    fn cli_config_works_without_user_config() {
        let root = tempfile::tempdir().unwrap();
        let ours = root.path().join("ours");
        write_cli_config(&root.path().join("missing"), &ours).unwrap();
        assert_eq!(
            std::fs::read_to_string(ours.join("config.json")).unwrap(),
            "{}"
        );
    }
}
