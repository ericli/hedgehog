//! One shared Postgres for every app, each with its own database and role.
//! Admin work goes through `docker exec ... psql`, so nothing is published
//! on the host.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bollard::models::{ContainerCreateBody, HostConfig, RestartPolicy, RestartPolicyNameEnum};
use bollard::query_parameters::CreateContainerOptionsBuilder;

use crate::docker::{ContainerState, Docker, NETWORK};
use crate::secrets::random_password;
use crate::store::Store;

pub const CONTAINER: &str = "hedgehog-postgres";
const IMAGE: &str = "postgres:17-alpine";
const VOLUME: &str = "hedgehog-postgres";
const BACKUPS_KEPT: usize = 7;

pub struct Postgres {
    docker: Docker,
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

impl Postgres {
    pub fn new(docker: Docker) -> Self {
        Self { docker }
    }

    /// Starts the shared Postgres container, creating it the first time.
    pub async fn ensure_running(
        &self,
        store: &Store,
        log: &(dyn Fn(String) + Send + Sync),
    ) -> Result<()> {
        match self.docker.state(CONTAINER).await? {
            ContainerState::Running { .. } => {}
            ContainerState::Stopped { .. } => self.docker.start(CONTAINER).await?,
            ContainerState::Missing => {
                if !self.docker.image_exists(IMAGE).await {
                    log(format!("pulling {IMAGE} (first time only)"));
                    self.docker.pull(IMAGE).await?;
                }
                let password =
                    store.setting_or_init("postgres.superuser_password", random_password)?;
                let body = ContainerCreateBody {
                    image: Some(IMAGE.to_string()),
                    env: Some(vec![format!("POSTGRES_PASSWORD={password}")]),
                    host_config: Some(HostConfig {
                        binds: Some(vec![format!("{VOLUME}:/var/lib/postgresql/data")]),
                        restart_policy: Some(RestartPolicy {
                            name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                            maximum_retry_count: None,
                        }),
                        network_mode: Some(NETWORK.to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                let options = CreateContainerOptionsBuilder::default()
                    .name(CONTAINER)
                    .build();
                self.docker_api_create(options, body).await?;
                self.docker.start(CONTAINER).await?;
            }
        }
        self.wait_ready().await
    }

    async fn docker_api_create(
        &self,
        options: bollard::query_parameters::CreateContainerOptions,
        body: ContainerCreateBody,
    ) -> Result<()> {
        self.docker
            .api()
            .create_container(Some(options), body)
            .await
            .context("could not create the Postgres container")?;
        Ok(())
    }

    async fn wait_ready(&self) -> Result<()> {
        for _ in 0..60 {
            let ready = self
                .docker
                .exec(
                    CONTAINER,
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
                CONTAINER,
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
        Ok(database_url(app, &password))
    }

    pub async fn drop_database(&self, store: &Store, app: &str) -> Result<()> {
        if !matches!(
            self.docker.state(CONTAINER).await?,
            ContainerState::Running { .. }
        ) {
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
        if !matches!(
            self.docker.state(CONTAINER).await?,
            ContainerState::Running { .. }
        ) {
            return Ok(());
        }
        let stamp = crate::secrets::now();
        for app in apps {
            let exists = self
                .docker
                .exec(
                    CONTAINER,
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
                .exec(CONTAINER, &["pg_dump", "-U", "postgres", "-Fc", app], b"")
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
