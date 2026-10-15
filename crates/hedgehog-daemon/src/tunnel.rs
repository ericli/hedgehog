//! Publishes the gateway through OpenTunnel, in-process. hedgehog keeps its
//! own tunnel identity (separate from any `opentunnel` CLI profiles), maps
//! `@` to the home page and `<app>` to each app's gateway listener.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hedgehog_core::api::TunnelInfo;
use opentunnel::{Client, ClientOptions, Route, Routes, State, Storage, Tunnel};
use tracing::{info, warn};

const PROFILE: &str = "hedgehog";

pub struct TunnelManager {
    client: Option<Client>,
    tunnel: Mutex<Option<Tunnel>>,
    /// Last routes handed to the tunnel, so unchanged sets are not resent.
    routes: Mutex<Routes>,
    info: Arc<Mutex<TunnelInfo>>,
}

/// `@` → home listener, `<app>` → that app's listener.
pub fn routes(home_port: u16, app_ports: &BTreeMap<String, u16>) -> Routes {
    let mut routes = Routes::new();
    routes.insert(
        "@".to_string(),
        Route::new(format!("127.0.0.1:{home_port}")),
    );
    for (app, port) in app_ports {
        routes.insert(app.clone(), Route::new(format!("127.0.0.1:{port}")));
    }
    routes
}

fn state_name(state: &State) -> &'static str {
    match state {
        State::WaitingRoutes | State::Connecting => "connecting",
        State::Connected => "connected",
        State::Reconnecting => "reconnecting",
        State::Stopped => "stopped",
    }
}

impl TunnelManager {
    /// `enabled = false` keeps everything local (useful offline and in tests).
    pub fn new(storage: &Path, enabled: bool) -> Self {
        let client = enabled.then(|| {
            Client::new(ClientOptions {
                api: None,
                storage: Some(Storage::at(storage)),
            })
        });
        let state = if enabled { "starting" } else { "off" };
        Self {
            client,
            tunnel: Mutex::new(None),
            routes: Mutex::default(),
            info: Arc::new(Mutex::new(TunnelInfo {
                state: state.to_string(),
                hostname: None,
                error: None,
            })),
        }
    }

    pub fn info(&self) -> TunnelInfo {
        self.info.lock().unwrap().clone()
    }

    /// Gets (or creates) hedgehog's tunnel identity and connects. Retries
    /// until it works, since the Mac may boot before the network is up.
    /// Returns the tunnel hostname, or `None` when tunnels are off.
    pub async fn start(&self, routes: Routes) -> Option<String> {
        let client = self.client.as_ref()?;
        let mut delay = Duration::from_secs(2);
        let identity = loop {
            match client.ensure(PROFILE).await {
                Ok(identity) => break identity,
                Err(error) => {
                    warn!(%error, "could not set up the OpenTunnel identity; retrying");
                    self.info.lock().unwrap().error = Some(error.to_string());
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(60));
                }
            }
        };
        *self.routes.lock().unwrap() = routes.clone();
        let tunnel = loop {
            match client.connect(PROFILE, routes.clone()) {
                Ok(tunnel) => break tunnel,
                Err(error) => {
                    warn!(%error, "could not start the tunnel; retrying");
                    self.info.lock().unwrap().error = Some(error.to_string());
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(60));
                }
            }
        };
        info!(hostname = %identity.hostname, "tunnel started");
        {
            let mut info = self.info.lock().unwrap();
            info.hostname = Some(identity.hostname.clone());
            info.state = "connecting".to_string();
            info.error = None;
        }
        let mut status = tunnel.status_watch();
        let shared = self.info.clone();
        tokio::spawn(async move {
            loop {
                {
                    let current = status.borrow_and_update();
                    let mut info = shared.lock().unwrap();
                    info.state = state_name(&current.state).to_string();
                    info.error = current.last_error.clone();
                }
                if status.changed().await.is_err() {
                    break;
                }
            }
        });
        *self.tunnel.lock().unwrap() = Some(tunnel);
        Some(identity.hostname)
    }

    /// Names of the routes currently published.
    pub fn published(&self) -> Vec<String> {
        self.routes.lock().unwrap().keys().cloned().collect()
    }

    /// Replaces the published routes if they changed.
    pub fn set_routes(&self, routes: Routes) {
        let tunnel = self.tunnel.lock().unwrap();
        let Some(tunnel) = tunnel.as_ref() else {
            return;
        };
        let mut current = self.routes.lock().unwrap();
        if *current == routes {
            return;
        }
        match tunnel.set_routes(routes.clone()) {
            Ok(()) => *current = routes,
            Err(error) => warn!(%error, "could not update tunnel routes"),
        }
    }

    pub async fn close(&self) {
        let tunnel = self.tunnel.lock().unwrap().take();
        if let Some(tunnel) = tunnel {
            let _ = tunnel.close().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_routes() {
        let apps = BTreeMap::from([("budget".to_string(), 4001), ("notes".to_string(), 4002)]);
        let routes = routes(4000, &apps);
        assert_eq!(routes.len(), 3);
        assert_eq!(routes["@"].target, "127.0.0.1:4000");
        assert_eq!(routes["budget"].target, "127.0.0.1:4001");
        for (name, route) in &routes {
            route.validate(name).unwrap();
        }
    }

    #[test]
    fn disabled_tunnel_reports_off() {
        let dir = tempfile::tempdir().unwrap();
        let manager = TunnelManager::new(dir.path(), false);
        assert_eq!(manager.info().state, "off");
    }
}
