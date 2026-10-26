//! Postgres for hedgehog apps, each with its own database and role. Admin
//! work goes through `docker exec ... psql`.
//!
//! Two instances use the same code: the shared one the daemon runs for
//! deployed apps (reachable only on the hedgehog Docker network), and a dev
//! one (`hedgehog db up`) published on `localhost` for working on apps.

use std::collections::HashMap;
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

/// Where Postgres keeps its Unix socket and the socket's lock file. It's a
/// tmpfs, so a lock file a crash leaves behind goes away when the container
/// stops, instead of staying in it and failing every later start.
const SOCKET_DIR: &str = "/var/run/postgresql";

/// Runs the image's usual entrypoint, first removing a `postmaster.pid` that
/// holds no PID. When Docker's VM loses writes (the Mac's disk filled up),
/// the file can come back full of zeros, and Postgres refuses to start
/// ("bogus data in lock file") until someone deletes it. Only this container
/// uses the data volume, so a file like that is always a leftover. One with a
/// PID is left alone: Postgres checks those itself.
const ENTRYPOINT: &str = r#"pid="$PGDATA/postmaster.pid"
if [ -e "$pid" ] && ! head -n 1 "$pid" | grep -qE '^[1-9][0-9]*$'; then
  echo "hedgehog: removing $pid, which an unclean stop left without a PID" >&2
  rm -f "$pid"
fi
exec docker-entrypoint.sh postgres"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    /// For deployed apps, on the `hedgehog` network.
    Shared,
    /// For development, published on `127.0.0.1:<port>`.
    Dev { port: u16 },
}

impl Flavor {
    fn container(self) -> &'static str {
        match self {
            Flavor::Shared => CONTAINER,
            Flavor::Dev { .. } => DEV_CONTAINER,
        }
    }
}

pub struct Postgres {
    docker: Docker,
    flavor: Flavor,
    /// Several deploys can need Postgres at once (e.g. at startup); only one
    /// may create the container.
    starting: tokio::sync::Mutex<()>,
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
            starting: tokio::sync::Mutex::new(()),
        }
    }

    /// The local development instance on `127.0.0.1:<port>`.
    pub fn dev(docker: Docker, port: u16) -> Self {
        Self {
            docker,
            flavor: Flavor::Dev { port },
            starting: tokio::sync::Mutex::new(()),
        }
    }

    pub fn container(&self) -> &'static str {
        self.flavor.container()
    }

    pub async fn is_running(&self) -> Result<bool> {
        Ok(matches!(
            self.docker.state(self.container()).await?,
            ContainerState::Running { .. }
        ))
    }

    /// Whether a start, which can take a minute, is under way.
    pub fn is_starting(&self) -> bool {
        self.starting.try_lock().is_err()
    }

    /// Starts the Postgres container, creating it the first time. A stopped
    /// container, or a dev one on a different port, is recreated (its data
    /// volume stays).
    pub async fn ensure_running(
        &self,
        store: &Store,
        log: &(dyn Fn(String) + Send + Sync),
    ) -> Result<()> {
        let _starting = self.starting.lock().await;
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
            // Recreated rather than restarted: everything worth keeping is in
            // the data volume, and a fresh container can't trip over files a
            // crash left in the old one. This also covers one Docker keeps
            // restarting, and moves containers onto the current settings.
            ContainerState::Stopped { .. } => {
                self.docker.remove(container).await?;
                self.create(store, log).await?;
            }
            ContainerState::Missing => self.create(store, log).await?,
        }
        self.wait_ready().await
    }

    async fn create(&self, store: &Store, log: &(dyn Fn(String) + Send + Sync)) -> Result<()> {
        let container = self.container();
        if !self.docker.image_exists(IMAGE).await {
            log(format!("pulling {IMAGE} (first time only)"));
            self.docker.pull(IMAGE).await?;
        }
        let password = store.setting_or_init("postgres.superuser_password", random_password)?;
        let options = CreateContainerOptionsBuilder::default()
            .name(container)
            .build();
        let create = self
            .docker
            .api()
            .create_container(Some(options), Self::container_body(self.flavor, &password));
        match crate::docker::bounded(Duration::from_secs(30), create).await {
            // Something else (another hedgehog, or a CLI) just made it.
            Ok(_)
            | Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 409, ..
            }) => {}
            Err(error) => {
                return Err(error).context("could not create the Postgres container");
            }
        }
        if !self.is_running().await? {
            self.start(container).await?;
        }
        Ok(())
    }

    fn container_body(flavor: Flavor, password: &str) -> ContainerCreateBody {
        let restart_policy = Some(RestartPolicy {
            name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
            maximum_retry_count: None,
        });
        let volume = flavor.container();
        let mut host_config = HostConfig {
            binds: Some(vec![format!("{volume}:/var/lib/postgresql/data")]),
            tmpfs: Some(HashMap::from([(SOCKET_DIR.to_string(), String::new())])),
            restart_policy,
            log_config: Some(crate::docker::log_config()),
            ..Default::default()
        };
        let mut exposed_ports = None;
        match flavor {
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
            entrypoint: Some(vec!["sh".into(), "-c".into(), ENTRYPOINT.into()]),
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
        bail!(
            "Postgres did not become ready within 60 seconds. Last output:\n{}",
            self.docker.recent_logs(self.container(), 10).await
        )
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

    /// The app's database as a `pg_dump` custom-format archive, or `None` if
    /// Postgres isn't running or the app has no database.
    pub async fn dump(&self, app: &str) -> Result<Option<Vec<u8>>> {
        if !self.is_running().await? {
            return Ok(None);
        }
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
            return Ok(None);
        }
        let dump = self
            .docker
            .exec(
                self.container(),
                &["pg_dump", "-U", "postgres", "-Fc", app],
                b"",
            )
            .await?;
        Ok(Some(dump))
    }
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
    fn keeps_lock_files_from_outliving_a_crash() {
        let body = Postgres::container_body(Flavor::Shared, "pw");
        let host_config = body.host_config.unwrap();
        assert_eq!(
            host_config.binds.unwrap(),
            vec!["hedgehog-postgres:/var/lib/postgresql/data"]
        );
        assert!(host_config.tmpfs.unwrap().contains_key(SOCKET_DIR));
        assert_eq!(body.entrypoint.unwrap()[2], ENTRYPOINT);
    }

    /// Runs [`ENTRYPOINT`] with `postmaster.pid` holding `pid_file` (or
    /// missing), and a stand-in for the image's entrypoint. Returns whether
    /// the file is still there.
    fn run_entrypoint(pid_file: Option<&[u8]>) -> bool {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir(&data).unwrap();
        if let Some(contents) = pid_file {
            std::fs::write(data.join("postmaster.pid"), contents).unwrap();
        }
        let stand_in = dir.path().join("docker-entrypoint.sh");
        std::fs::write(&stand_in, "#!/bin/sh\necho \"started $*\"\n").unwrap();
        std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}",
            dir.path().display(),
            std::env::var("PATH").unwrap()
        );
        let output = std::process::Command::new("sh")
            .args(["-c", ENTRYPOINT])
            .env("PGDATA", &data)
            .env("PATH", path)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "started postgres\n"
        );
        data.join("postmaster.pid").exists()
    }

    #[test]
    fn removes_pid_files_without_a_pid() {
        // What Docker's VM left after losing its disk on 2026-10-25.
        assert!(!run_entrypoint(Some(&[0; 94])));
        assert!(!run_entrypoint(Some(b"")));
        assert!(!run_entrypoint(Some(b"\n\n")));
        assert!(!run_entrypoint(Some(b"0\n")));
        // Postgres decides about these itself.
        assert!(run_entrypoint(Some(
            b"1\n/var/lib/postgresql/data\n1792538739\n5432\n"
        )));
        assert!(run_entrypoint(Some(b"4321\n")));
        assert!(!run_entrypoint(None));
    }
}
