//! Deploying: build an image, start it next to the running version, wait
//! until it answers HTTP, then switch traffic over and retire the old one.
//! A failed deploy never takes down what was already running.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use hedgehog_core::Manifest;
use hedgehog_core::api::{AppStatus, DeployEvent};
use hedgehog_core::env::{EnvMap, merge, read_app_dotenv};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::Daemon;
use crate::build::{self, builder_name};
use crate::docker::{self, ContainerState, RunSpec};
use crate::gateway::Upstream;
use crate::secrets::{now, random_token};
use crate::store::{AppRecord, Deployed};

const HEALTH_TIMEOUT: Duration = Duration::from_secs(120);

pub type Events = mpsc::UnboundedSender<DeployEvent>;

pub enum Source {
    /// Build from the app's folder as it is.
    Folder,
    /// Replace the folder with an uploaded `.tar.gz` first.
    Upload(PathBuf),
}

fn step(events: &Events, message: impl Into<String>) {
    let _ = events.send(DeployEvent::Step {
        message: message.into(),
    });
}

fn new_deploy_id() -> String {
    format!("{:x}{}", now(), random_token(2))
}

/// The commit an app folder is at, if it is a git repo.
pub async fn git_revision(dir: &Path) -> Option<String> {
    if !dir.join(".git").exists() {
        return None;
    }
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// True once something on `port` answers like an HTTP server. Any status
/// counts; a bare TCP accept does not, because Docker Desktop's port proxy
/// accepts connections before the app is listening.
async fn http_responds(port: u16) -> bool {
    let attempt = async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nUser-Agent: hedgehog-health\r\nConnection: close\r\n\r\n")
            .await?;
        let mut head = [0u8; 5];
        stream.read_exact(&mut head).await?;
        Ok::<_, std::io::Error>(&head == b"HTTP/")
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(3), attempt).await,
        Ok(Ok(true))
    )
}

impl Daemon {
    /// Deploys `app`, reporting progress on `events`.
    pub async fn deploy(self: &Arc<Self>, app: &str, source: Source, events: Events) -> Result<()> {
        let lock = self.lock(app);
        let _guard = lock.lock().await;
        let result = self.deploy_locked(app, source, &events).await;
        self.finish(app, result, &events)
    }

    /// Starts the current image again with fresh env (after `env set`, or
    /// when its container went missing). Rebuilds if there is no image.
    pub async fn restart(self: &Arc<Self>, app: &str, events: Events) -> Result<()> {
        let lock = self.lock(app);
        let _guard = lock.lock().await;
        let record = self.record(app)?;
        let result = match (&record.image, &record.builder) {
            (Some(image), Some(builder)) if self.docker.image_exists(image).await => {
                self.store.set_stopped(app, false)?;
                let manifest = Manifest::load(&record.source_dir)?;
                self.start_version(
                    &record,
                    &manifest,
                    image,
                    builder,
                    record.revision.clone(),
                    &events,
                )
                .await
            }
            _ => self.deploy_locked(app, Source::Folder, &events).await,
        };
        self.finish(app, result, &events)
    }

    fn finish(&self, app: &str, result: Result<Option<String>>, events: &Events) -> Result<()> {
        match result {
            Ok(url) => {
                let _ = events.send(DeployEvent::Done { url });
                Ok(())
            }
            Err(error) => {
                let message = format!("{error:#}");
                // Keep "running" if the previous version is still serving.
                let serving = self
                    .gateway
                    .upstream(app)
                    .is_some_and(|upstream| upstream.port.is_some());
                let status = if serving {
                    AppStatus::Running
                } else {
                    AppStatus::Failed
                };
                let _ = self.store.set_status(app, status, Some(&message));
                let _ = events.send(DeployEvent::Failed { error: message });
                Err(error)
            }
        }
    }

    fn record(&self, app: &str) -> Result<AppRecord> {
        self.store
            .app(app)?
            .with_context(|| format!("there is no app called {app}"))
    }

    async fn deploy_locked(
        self: &Arc<Self>,
        app: &str,
        source: Source,
        events: &Events,
    ) -> Result<Option<String>> {
        if let Source::Upload(archive) = source {
            step(events, "unpacking upload");
            let dir = self.paths.app_source(app);
            crate::upload::replace_folder(&self.paths, &archive, &dir)?;
            self.store.upsert_app(app, &dir)?;
        }
        let record = self.record(app)?;
        let dir = record.source_dir.clone();
        if !dir.is_dir() {
            bail!("{} no longer exists", dir.display());
        }
        let manifest = Manifest::load(&dir)?;
        self.store.set_status(app, AppStatus::Building, None)?;
        self.ensure_listener(&record, &manifest).await?;

        let deploy_id = new_deploy_id();
        let image = docker::image_name(app, &deploy_id);
        let revision = git_revision(&dir).await;
        let builder = manifest.builder(&dir);
        step(
            events,
            format!("building {app} with {}", builder_name(builder)),
        );

        let log_dir = self.paths.logs().join(app);
        std::fs::create_dir_all(&log_dir)?;
        let log_file = Mutex::new(std::fs::File::create(log_dir.join("build.log"))?);
        let log_events = events.clone();
        let log = move |line: String| {
            let _ = writeln!(log_file.lock().unwrap(), "{line}");
            let _ = log_events.send(DeployEvent::Log { line });
        };
        let work_dir = self.paths.state.join("build").join(&deploy_id);
        let built = build::build(&self.docker, &dir, &manifest, &image, &work_dir, &log).await;
        let _ = std::fs::remove_dir_all(&work_dir);
        built?;

        self.start_version(
            &record,
            &manifest,
            &image,
            builder_name(builder),
            revision,
            events,
        )
        .await
    }

    /// Opens the app's gateway listener early, so it has a URL even before
    /// its first deploy finishes.
    async fn ensure_listener(&self, record: &AppRecord, manifest: &Manifest) -> Result<()> {
        if self.gateway.upstream(&record.name).is_none() {
            let upstream = Upstream {
                port: None,
                public: record.public.unwrap_or(manifest.public()),
                reason: "It's being set up. Try again in a minute.".to_string(),
            };
            self.gateway.set_app(&record.name, upstream).await?;
            self.sync_routes();
        }
        Ok(())
    }

    /// Runs `image` as the app's new container and switches traffic to it
    /// once it is healthy.
    async fn start_version(
        &self,
        record: &AppRecord,
        manifest: &Manifest,
        image: &str,
        builder: &str,
        revision: Option<String>,
        events: &Events,
    ) -> Result<Option<String>> {
        let app = record.name.as_str();
        self.ensure_listener(record, manifest).await?;
        let env = self.app_env(record, manifest, events).await?;
        step(events, "starting");
        let deploy_id = new_deploy_id();
        let data_dir = self.paths.app_data(app);
        let container = self
            .docker
            .run_app(&RunSpec {
                app,
                deploy_id: &deploy_id,
                image,
                env: &env,
                port: manifest.port(),
                data_dir: &data_dir,
            })
            .await?;
        let port = match self.wait_healthy(&container, manifest.port(), events).await {
            Ok(port) => port,
            Err(error) => {
                let _ = self.docker.remove(&container).await;
                return Err(error);
            }
        };

        self.store.set_deployed(
            app,
            &Deployed {
                container: container.clone(),
                image: image.to_string(),
                builder: builder.to_string(),
                revision,
            },
        )?;
        let public = record.public.unwrap_or(manifest.public());
        self.gateway
            .set_app(
                app,
                Upstream {
                    port: Some(port),
                    public,
                    reason: String::new(),
                },
            )
            .await?;
        self.sync_routes();

        for old in self.docker.app_containers(app).await? {
            if old != container {
                let _ = self.docker.remove(&old).await;
            }
        }
        let keep: Vec<&str> = [Some(image), record.image.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        if let Err(error) = self.docker.remove_images_except(app, &keep).await {
            debug!(%error, app, "could not prune old images");
        }
        Ok(self.gateway.app_url(app))
    }

    /// Env for the container: `.env` < hedgehog's values < manifest < `env set`.
    async fn app_env(
        &self,
        record: &AppRecord,
        manifest: &Manifest,
        events: &Events,
    ) -> Result<EnvMap> {
        let app = record.name.as_str();
        let dotenv = read_app_dotenv(&record.source_dir)?;
        let mut hedgehog = EnvMap::from([
            ("PORT".to_string(), manifest.port().to_string()),
            ("HOST".to_string(), "0.0.0.0".to_string()),
            ("DATA_DIR".to_string(), "/data".to_string()),
            ("HEDGEHOG_APP".to_string(), app.to_string()),
        ]);
        if let Some(url) = self.gateway.app_url(app) {
            hedgehog.insert("HEDGEHOG_URL".to_string(), url);
        }
        if manifest.postgres() {
            step(events, "preparing the database");
            let log_events = events.clone();
            let log = move |message: String| {
                let _ = log_events.send(DeployEvent::Step { message });
            };
            self.postgres.ensure_running(&self.store, &log).await?;
            let url = self.postgres.ensure_database(&self.store, app).await?;
            hedgehog.insert("DATABASE_URL".to_string(), url);
        }
        let user = self.store.env(app)?;
        Ok(merge(&[&dotenv, &hedgehog, &manifest.env, &user]))
    }

    async fn wait_healthy(&self, container: &str, app_port: u16, events: &Events) -> Result<u16> {
        let started = Instant::now();
        let mut announced = false;
        loop {
            match self.docker.state(container).await? {
                ContainerState::Running { restarts: 0 } => {}
                ContainerState::Running { .. } => bail!(
                    "the app crashed before it started serving (Docker restarted it). Last output:\n{}",
                    self.recent_logs(container, 30).await
                ),
                ContainerState::Stopped { exit_code } => {
                    let code = exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "?".into());
                    bail!(
                        "the app exited (code {code}) before it started serving. Last output:\n{}",
                        self.recent_logs(container, 30).await
                    );
                }
                ContainerState::Missing => bail!("the app's container disappeared while starting"),
            }
            if let Some(port) = self.docker.host_port(container).await?
                && http_responds(port).await
            {
                return Ok(port);
            }
            if !announced && started.elapsed() > Duration::from_secs(10) {
                step(
                    events,
                    format!("waiting for the app to answer HTTP on port {app_port}"),
                );
                announced = true;
            }
            if started.elapsed() > HEALTH_TIMEOUT {
                bail!(
                    "the app never answered HTTP on port {app_port}. Make sure it listens on $PORT \
                     (or set `port` in hedgehog.toml). Last output:\n{}",
                    self.recent_logs(container, 30).await
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn recent_logs(&self, container: &str, lines: usize) -> String {
        let chunks: Vec<_> = self.docker.logs(container, false, lines).collect().await;
        let text: String = chunks
            .into_iter()
            .filter_map(Result::ok)
            .map(|chunk| String::from_utf8_lossy(&chunk).into_owned())
            .collect();
        text.trim_end().to_string()
    }

    pub async fn stop_app(&self, app: &str) -> Result<()> {
        let lock = self.lock(app);
        let _guard = lock.lock().await;
        let record = self.record(app)?;
        if let Some(container) = &record.container {
            self.docker.stop(container).await?;
        }
        self.store.set_stopped(app, true)?;
        if let Some(mut upstream) = self.gateway.upstream(app) {
            upstream.port = None;
            upstream.reason = "It was stopped with `hedgehog stop`.".to_string();
            self.gateway.set_app(app, upstream).await?;
        }
        Ok(())
    }

    /// Forgets an app: removes its containers and images and moves its folder
    /// to the Trash. `purge` also deletes its data folder and database.
    pub async fn remove_app(&self, app: &str, purge: bool) -> Result<()> {
        let lock = self.lock(app);
        let _guard = lock.lock().await;
        let record = self.record(app)?;
        for container in self.docker.app_containers(app).await? {
            self.docker.remove(&container).await?;
        }
        self.gateway.remove_app(app);
        self.sync_routes();
        self.store.delete_app(app)?;
        if record.source_dir.starts_with(&self.paths.apps) && record.source_dir.exists() {
            let trashed = crate::upload::move_to_trash(&self.paths, &record.source_dir)?;
            info!(app, to = %trashed.display(), "moved app folder to the Trash");
        }
        let _ = self.docker.remove_images_except(app, &[]).await;
        if purge {
            self.postgres.drop_database(&self.store, app).await?;
            let data = self.paths.app_data(app);
            if data.exists() {
                std::fs::remove_dir_all(&data)?;
            }
        }
        Ok(())
    }

    /// Deploys in the background, logging progress to the daemon log.
    pub fn spawn_deploy(self: &Arc<Self>, app: String, source: Source, why: &'static str) {
        let daemon = self.clone();
        tokio::spawn(async move {
            info!(app, why, "deploying");
            let (events, mut rx) = mpsc::unbounded_channel();
            let name = app.clone();
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    match event {
                        DeployEvent::Step { message } => info!(app = name, "{message}"),
                        DeployEvent::Log { line } => debug!(app = name, "{line}"),
                        DeployEvent::Done { url } => {
                            info!(app = name, url = url.unwrap_or_default(), "deployed")
                        }
                        DeployEvent::Failed { error } => {
                            warn!(app = name, "deploy failed: {error}")
                        }
                    }
                }
            });
            let _ = daemon.deploy(&app, source, events).await;
        });
    }

    pub fn spawn_restart(self: &Arc<Self>, app: String) {
        let daemon = self.clone();
        tokio::spawn(async move {
            let (events, _rx) = mpsc::unbounded_channel();
            if let Err(error) = daemon.restart(&app, events).await {
                warn!(app, "restart failed: {error:#}");
            }
        });
    }
}
