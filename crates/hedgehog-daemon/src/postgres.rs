//! Postgres for hedgehog apps, each with its own database and role. Admin
//! work goes through `docker exec ... psql`.
//!
//! Two instances use the same code: the shared one the daemon runs for
//! deployed apps (reachable only on the hedgehog Docker network), and a dev
//! one (`hedgehog db up`) published on `localhost` for working on apps.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bollard::models::{
    ContainerCreateBody, HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::CreateContainerOptionsBuilder;

use crate::docker::{ContainerState, Docker, NETWORK};
use crate::secrets::random_password;
use crate::store::Store;

pub const CONTAINER: &str = "hedgehog-postgres";
pub const DEV_CONTAINER: &str = "hedgehog-dev-postgres";
pub const DEFAULT_DEV_PORT: u16 = 5432;
pub const IMAGE: &str = "postgres:17-alpine";
const BACKUPS_KEPT: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    /// For deployed apps, on the `hedgehog` network.
    Shared,
    /// For development, published on `127.0.0.1:<port>`.
    Dev { port: u16 },
}

pub struct Postgres {
    docker: Docker,
    flavor: Flavor,
}

/// Postgres identifiers are double-quoted so names with hyphens work. App
/// names are already restricted to `[a-z0-9-]`, so there is nothing to escape.
fn ident(name: &str) -> String {
    format!("\"{name}\"")
}

fn password_key(app: &str) -> String {
    format!("postgres.password.{app}")
}

pub fn database_url(app: &str, password: &str) -> String {
    format!("postgres://{app}:{password}@{CONTAINER}:5432/{app}")
}

pub fn dev_database_url(app: &str, password: &str, port: u16) -> String {
    format!("postgres://{app}:{password}@localhost:{port}/{app}")
}

impl Postgres {
    /// The instance deployed apps use.
    pub fn new(docker: Docker) -> Self {
        Self {
            docker,
            flavor: Flavor::Shared,
        }
    }

    /// The local development instance on `127.0.0.1:<port>`.
    pub fn dev(docker: Docker, port: u16) -> Self {
        Self {
            docker,
            flavor: Flavor::Dev { port },
        }
    }

    pub fn container(&self) -> &'static str {
        match self.flavor {
            Flavor::Shared => CONTAINER,
            Flavor::Dev { .. } => DEV_CONTAINER,
        }
    }

    pub async fn is_running(&self) -> Result<bool> {
        Ok(matches!(
            self.docker.state(self.container()).await?,
            ContainerState::Running { .. }
        ))
    }

    /// Starts the Postgres container, creating it the first time. A dev
    /// container on a different port is recreated (its data volume stays).
    pub async fn ensure_running(
        &self,
        store: &Store,
        log: &(dyn Fn(String) + Send + Sync),
    ) -> Result<()> {
        let container = self.container();
        let mut state = self.docker.state(container).await?;
        if let Flavor::Dev { port } = self.flavor
            && state != ContainerState::Missing
            && self.docker.published_port(container).await? != Some(port)
        {
            self.docker.remove(container).await?;
            state = ContainerState::Missing;
        }
        match state {
            ContainerState::Running { .. } => {}
            ContainerState::Stopped { .. } => self.start(container).await?,
            ContainerState::Missing => {
                if !self.docker.image_exists(IMAGE).await {
                    log(format!("pulling {IMAGE} (first time only)"));
                    self.docker.pull(IMAGE).await?;
                }
                let password =
                    store.setting_or_init("postgres.superuser_password", random_password)?;
                let options = CreateContainerOptionsBuilder::default()
                    .name(container)
                    .build();
                self.docker
                    .api()
                    .create_container(Some(options), self.container_body(&password))
                    .await
                    .context("could not create the Postgres container")?;
                self.start(container).await?;
            }
        }
        self.wait_ready().await
    }

    fn container_body(&self, password: &str) -> ContainerCreateBody {
        let restart_policy = Some(RestartPolicy {
            name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
            maximum_retry_count: None,
        });
        let volume = self.container();
        let mut host_config = HostConfig {
            binds: Some(vec![format!("{volume}:/var/lib/postgresql/data")]),
            restart_policy,
            ..Default::default()
        };
        let mut exposed_ports = None;
        match self.flavor {
            Flavor::Shared => host_config.network_mode = Some(NETWORK.to_string()),
            Flavor::Dev { port } => {
                exposed_ports = Some(vec!["5432/tcp".to_string()]);
                host_config.port_bindings = Some(
                    [(
                        "5432/tcp".to_string(),
                        Some(vec![PortBinding {
                            host_ip: Some("127.0.0.1".to_string()),
                            host_port: Some(port.to_string()),
                        }]),
                    )]
                    .into(),
                );
            }
        }
        ContainerCreateBody {
            image: Some(IMAGE.to_string()),
            env: Some(vec![format!("POSTGRES_PASSWORD={password}")]),
            exposed_ports,
            host_config: Some(host_config),
            ..Default::default()
        }
    }

    async fn start(&self, container: &str) -> Result<()> {
        match (self.docker.start(container).await, self.flavor) {
            (Ok(()), _) => Ok(()),
            (Err(error), Flavor::Dev { port })
                if format!("{error:#}").contains("allocated")
                    || format!("{error:#}").contains("in use") =>
            {
                bail!(
                    "port {port} is already taken, probably by another Postgres. \
                     Pick another with `hedgehog db up --port {}`",
                    port + 1
                )
            }
            (Err(error), _) => Err(error),
        }
    }

    /// Stops the container; its data volume is kept.
    pub async fn stop(&self) -> Result<()> {
        self.docker.stop(self.container()).await
    }

    async fn wait_ready(&self) -> Result<()> {
        for _ in 0..60 {
            let ready = self
                .docker
                .exec(
                    self.container(),
                    &["pg_isready", "-U", "postgres", "-h", "127.0.0.1"],
                    b"",
                )
                .await
                .is_ok();
            if ready {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        bail!("Postgres did not become ready within 60 seconds")
    }

    async fn psql(&self, script: &str) -> Result<()> {
        self.docker
            .exec(
                self.container(),
                &["psql", "-U", "postgres", "-v", "ON_ERROR_STOP=1", "-q"],
                script.as_bytes(),
            )
            .await?;
        Ok(())
    }

    /// Creates (or repairs) the app's role and database; returns its URL.
    pub async fn ensure_database(&self, store: &Store, app: &str) -> Result<String> {
        let password = store.setting_or_init(&password_key(app), random_password)?;
        let (role, literal) = (ident(app), format!("'{app}'"));
        let script = format!(
            "DO $$ BEGIN
               IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = {literal}) THEN
                 CREATE ROLE {role} LOGIN PASSWORD '{password}';
               ELSE
                 ALTER ROLE {role} WITH LOGIN PASSWORD '{password}';
               END IF;
             END $$;
             SELECT 'CREATE DATABASE {role} OWNER {role}'
               WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = {literal}) \\gexec
             REVOKE CONNECT ON DATABASE {role} FROM PUBLIC;
             GRANT CONNECT ON DATABASE {role} TO {role};
            "
        );
        self.psql(&script)
            .await
            .with_context(|| format!("could not set up the database for {app}"))?;
        Ok(match self.flavor {
            Flavor::Shared => database_url(app, &password),
            Flavor::Dev { port } => dev_database_url(app, &password, port),
        })
    }

    /// Empties an app's database by recreating it; its role and URL stay.
    pub async fn reset_database(&self, store: &Store, app: &str) -> Result<String> {
        self.psql(&format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE);",
            ident(app)
        ))
        .await?;
        self.ensure_database(store, app).await
    }

    pub async fn drop_database(&self, store: &Store, app: &str) -> Result<()> {
        if !self.is_running().await? {
            return Ok(());
        }
        let role = ident(app);
        self.psql(&format!(
            "DROP DATABASE IF EXISTS {role} WITH (FORCE);\nDROP ROLE IF EXISTS {role};"
        ))
        .await?;
        store.delete_setting(&password_key(app))?;
        Ok(())
    }

    /// Dumps every app database to `<backups>/<app>/<timestamp>.dump`,
    /// keeping the newest few.
    pub async fn backup(&self, apps: &[String], backups: &Path) -> Result<()> {
        if !self.is_running().await? {
            return Ok(());
        }
        let stamp = crate::secrets::now();
        for app in apps {
            let exists = self
                .docker
                .exec(
                    self.container(),
                    &[
                        "psql",
                        "-U",
                        "postgres",
                        "-tAc",
                        &format!("SELECT 1 FROM pg_database WHERE datname = '{app}'"),
                    ],
                    b"",
                )
                .await?;
            if String::from_utf8_lossy(&exists).trim() != "1" {
                continue;
            }
            let dump = self
                .docker
                .exec(
                    self.container(),
                    &["pg_dump", "-U", "postgres", "-Fc", app],
                    b"",
                )
                .await?;
            let dir = backups.join(app);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join(format!("{stamp}.dump")), dump)?;
            prune_backups(&dir)?;
        }
        Ok(())
    }
}

fn prune_backups(dir: &Path) -> Result<()> {
    let mut dumps: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "dump"))
        .collect();
    dumps.sort();
    let excess = dumps.len().saturating_sub(BACKUPS_KEPT);
    for old in dumps.into_iter().take(excess) {
        std::fs::remove_file(old)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_urls() {
        assert_eq!(
            database_url("my-app", "pw"),
            "postgres://my-app:pw@hedgehog-postgres:5432/my-app"
        );
        assert_eq!(
            dev_database_url("my-app", "pw", 5433),
            "postgres://my-app:pw@localhost:5433/my-app"
        );
    }

    #[test]
    fn prunes_old_backups() {
        let dir = tempfile::tempdir().unwrap();
        for stamp in 100..110 {
            std::fs::write(dir.path().join(format!("{stamp}.dump")), "x").unwrap();
        }
        prune_backups(dir.path()).unwrap();
        let mut left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left.len(), BACKUPS_KEPT);
        assert_eq!(left[0], "103.dump");
    }
}
