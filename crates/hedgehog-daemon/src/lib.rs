//! The hedgehog daemon: watches `~/Hedgehog/apps`, builds and runs each app in
//! Docker with its own Postgres database, and publishes it through OpenTunnel
//! behind a gateway that only lets your paired devices in.

pub mod api;
pub mod build;
pub mod deploy;
pub mod docker;
pub mod gateway;
pub mod launchd;
mod pages;
pub mod postgres;
pub mod secrets;
pub mod store;
pub mod supervise;
pub mod tunnel;
pub mod upload;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use hedgehog_core::Paths;
use tracing::{info, warn};

use crate::docker::Docker;
use crate::gateway::Gateway;
use crate::postgres::Postgres;
use crate::store::Store;
use crate::tunnel::TunnelManager;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Options {
    /// Publish apps through OpenTunnel. Off keeps everything on `127.0.0.1`.
    pub tunnel: bool,
    /// Extra addresses for the control API besides `127.0.0.1:7700` (e.g. the
    /// Tailscale IP). Found automatically when empty.
    pub listen: Vec<SocketAddr>,
}

pub struct Daemon {
    pub paths: Paths,
    pub store: Arc<Store>,
    pub docker: Docker,
    pub postgres: Postgres,
    pub gateway: Gateway,
    pub tunnel: TunnelManager,
    pub token: String,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Daemon {
    /// Serializes deploys, restarts, and removals of one app.
    pub fn lock(&self, app: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(app.to_string())
            .or_default()
            .clone()
    }

    pub fn is_busy(&self, app: &str) -> bool {
        self.lock(app).try_lock().is_err()
    }

    /// Pushes the gateway's current listeners to the tunnel.
    pub fn sync_routes(&self) {
        let routes = tunnel::routes(self.gateway.home_port(), &self.gateway.app_ports());
        self.tunnel.set_routes(routes);
    }
}

/// Reads the API token, creating it (mode 0600) the first time.
pub fn load_or_create_token(paths: &Paths) -> Result<String> {
    let path = paths.token();
    if let Ok(token) = std::fs::read_to_string(&path) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    std::fs::create_dir_all(&paths.state)?;
    let token = secrets::random_token(32);
    write_private(&path, &token)?;
    Ok(token)
}

fn write_private(path: &std::path::Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("could not write {}", path.display()))?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

async fn connect_docker() -> Docker {
    let mut warned = false;
    loop {
        if let Ok(docker) = Docker::connect()
            && docker.ping().await
        {
            return docker;
        }
        if !warned {
            warn!("waiting for Docker to start (open Docker Desktop or OrbStack)");
            warned = true;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Runs the daemon until Ctrl-C or SIGTERM.
pub async fn run(paths: Paths, options: Options) -> Result<()> {
    for dir in [&paths.apps, &paths.data, &paths.state, &paths.logs()] {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    let token = load_or_create_token(&paths)?;
    let store = Arc::new(Store::open(&paths.database())?);
    let mut docker = connect_docker().await;
    docker.set_cli_config(docker::prepare_cli_config(&paths.state.join("docker"))?);
    docker.ensure_network().await?;
    let gateway = Gateway::start(store.clone()).await?;
    let daemon = Arc::new(Daemon {
        tunnel: TunnelManager::new(&paths.tunnel(), options.tunnel),
        postgres: Postgres::new(docker.clone()),
        paths,
        store,
        docker,
        gateway,
        token,
        locks: Mutex::default(),
    });
    daemon
        .gateway
        .set_api(api::dashboard_router(daemon.clone()));
    info!(apps = %daemon.paths.apps.display(), "hedgehog {VERSION} starting");

    // Bring apps back (listeners, containers) before the tunnel announces them.
    supervise::reconcile(&daemon, true).await;

    let tunnel_daemon = daemon.clone();
    tokio::spawn(async move {
        let routes = tunnel::routes(
            tunnel_daemon.gateway.home_port(),
            &tunnel_daemon.gateway.app_ports(),
        );
        if let Some(hostname) = tunnel_daemon.tunnel.start(routes).await {
            tunnel_daemon.gateway.set_hostname(Some(hostname));
            tunnel_daemon.sync_routes();
            info!(url = %tunnel_daemon.gateway.home_url(), "home page");
        }
    });
    tokio::spawn(supervise::run(daemon.clone()));

    let result = tokio::select! {
        result = api::serve(daemon.clone(), options.listen) => result,
        () = shutdown_signal() => Ok(()),
    };
    info!("shutting down");
    daemon.tunnel.close().await;
    result
}

async fn shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("could not listen for SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}
