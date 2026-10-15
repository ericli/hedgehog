//! `hedgehog db`: a local dev Postgres that matches what hedgehog runs for
//! deployed apps (same image, one database and role per app), plus psql
//! shortcuts. Works without a daemon, so it's for your laptop too.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use hedgehog_core::Paths;
use hedgehog_daemon::docker::Docker;
use hedgehog_daemon::postgres::{self, DEFAULT_DEV_PORT, Postgres};
use hedgehog_daemon::store::Store;

use crate::DbAction;
use crate::client::Client;
use crate::commands::{confirm, resolve_app};
use crate::style::{bold, dim, green, yellow};

const PORT_SETTING: &str = "dev.port";

pub async fn run(paths: &Paths, remote: Option<&str>, action: DbAction) -> Result<()> {
    match action {
        DbAction::Up { port } => {
            let (_, _, port) = dev_postgres(paths, port).await?;
            println!(
                "{} Dev Postgres ({}) on {}",
                green("✓"),
                postgres::IMAGE,
                bold(&format!("localhost:{port}"))
            );
            println!(
                "{}",
                dim(
                    "It starts whenever Docker does. In a project folder, `hedgehog db init` gives it a database."
                )
            );
            Ok(())
        }
        DbAction::Down => {
            let docker = connect().await?;
            Postgres::dev(docker, DEFAULT_DEV_PORT).stop().await?;
            println!(
                "Stopped the dev Postgres. Its data is kept; `hedgehog db up` starts it again."
            );
            Ok(())
        }
        DbAction::Init { path, name, force } => init(paths, path, name, force).await,
        DbAction::Url { path, name } => {
            let (_, _, name) = resolve_app(path, name)?;
            let (postgres, store, _) = dev_postgres(paths, None).await?;
            println!("{}", postgres.ensure_database(&store, &name).await?);
            Ok(())
        }
        DbAction::Reset { path, name, yes } => {
            let (_, _, name) = resolve_app(path, name)?;
            if !yes && !confirm(&format!("Empty the dev database `{name}`?"))? {
                println!("Nothing changed.");
                return Ok(());
            }
            let (postgres, store, _) = dev_postgres(paths, None).await?;
            postgres.reset_database(&store, &name).await?;
            println!("{} `{name}` is empty again (same URL).", green("✓"));
            Ok(())
        }
        DbAction::Psql { app, dev } => psql(paths, remote, app, dev),
    }
}

async fn connect() -> Result<Docker> {
    let docker = Docker::connect()?;
    if !docker.ping().await {
        bail!("Docker isn't running. Start Docker Desktop or OrbStack and try again");
    }
    Ok(docker)
}

/// Starts the dev Postgres (on `port`, else the last port used, else 5432).
async fn dev_postgres(paths: &Paths, port: Option<u16>) -> Result<(Postgres, Store, u16)> {
    std::fs::create_dir_all(&paths.state)?;
    let store = Store::open(&paths.dev_database())?;
    let port = match port {
        Some(port) => port,
        None => store
            .setting(PORT_SETTING)?
            .and_then(|port| port.parse().ok())
            .unwrap_or(DEFAULT_DEV_PORT),
    };
    let postgres = Postgres::dev(connect().await?, port);
    postgres
        .ensure_running(&store, &|message| println!("{}", dim(&message)))
        .await?;
    store.set_setting(PORT_SETTING, &port.to_string())?;
    Ok((postgres, store, port))
}

async fn init(
    paths: &Paths,
    path: Option<PathBuf>,
    name: Option<String>,
    force: bool,
) -> Result<()> {
    let (dir, manifest, name) = resolve_app(path, name)?;
    let (postgres, store, _) = dev_postgres(paths, None).await?;
    let url = postgres.ensure_database(&store, &name).await?;
    println!("{} Database `{name}` is ready", green("✓"));

    let env_path = dir.join(".env");
    let contents = match std::fs::read_to_string(&env_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", env_path.display()));
        }
    };
    // A URL hedgehog wrote before (e.g. before `db up --port`) is ours to update.
    let own_prefix = format!("postgres://{name}:");
    let replace = |existing: &str| {
        force || (existing.starts_with(&own_prefix) && existing.contains("@localhost:"))
    };
    let (updated, change) = upsert_env(&contents, "DATABASE_URL", &url, replace);
    match change {
        EnvChange::Added | EnvChange::Replaced => {
            std::fs::write(&env_path, updated)?;
            let verb = if change == EnvChange::Added {
                "Added DATABASE_URL to"
            } else {
                "Updated DATABASE_URL in"
            };
            println!("{} {verb} .env", green("✓"));
        }
        EnvChange::Unchanged => println!("{} DATABASE_URL in .env is already set", green("✓")),
        EnvChange::Kept(existing) => {
            println!(
                "{}",
                yellow(&format!(
                    "Left .env alone: it already sets DATABASE_URL ({}). Use --force to replace it.",
                    redact(&existing)
                ))
            );
        }
    }
    println!("  {}", dim(&url));
    if manifest.postgres == Some(false) {
        println!(
            "{}",
            yellow(
                "note: hedgehog.toml has `postgres = false`, so this app won't get a database when deployed."
            )
        );
    }
    if is_git_repo(&dir) && !is_git_ignored(&dir, ".env") {
        println!(
            "{}",
            yellow(
                "note: .env isn't gitignored. Add it to .gitignore so it stays on this machine."
            )
        );
    }
    println!(
        "{}",
        dim(
            "When deployed, hedgehog sets the app's own DATABASE_URL, so this one only applies locally."
        )
    );
    Ok(())
}

fn psql(paths: &Paths, remote: Option<&str>, app: Option<String>, dev: bool) -> Result<()> {
    let (container, name) = if dev {
        let (_, _, name) = resolve_app(None, app)?;
        (postgres::DEV_CONTAINER, name)
    } else {
        let app = app.context(
            "which app? `hedgehog db psql <app>`, or `--dev` for this project's dev database",
        )?;
        if !Client::resolve(paths, remote)?.local {
            bail!(
                "`hedgehog db psql` runs psql through Docker, so run it on the hedgehog Mac itself (or use --dev)"
            );
        }
        (postgres::CONTAINER, app)
    };
    let status = Command::new("docker")
        .args(["exec", "-it", container, "psql", "-U", &name, &name])
        .status()
        .context("could not run docker")?;
    if !status.success() {
        bail!("psql exited with {status}");
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum EnvChange {
    Added,
    Unchanged,
    Replaced,
    /// A different value was there and was kept.
    Kept(String),
}

/// Sets `key=value` in dotenv text, keeping everything else as it was. An
/// existing different value is only replaced if `replace` says so.
fn upsert_env(
    contents: &str,
    key: &str,
    value: &str,
    replace: impl Fn(&str) -> bool,
) -> (String, EnvChange) {
    let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();
    for line in &mut lines {
        let trimmed = line.trim_start();
        let assignment = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let Some((name, existing)) = assignment.split_once('=') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        let existing = existing.trim().trim_matches(['"', '\'']);
        if existing == value {
            return (contents.to_string(), EnvChange::Unchanged);
        }
        if !replace(existing) {
            return (contents.to_string(), EnvChange::Kept(existing.to_string()));
        }
        *line = format!("{key}={value}");
        let mut updated = lines.join("\n");
        if contents.ends_with('\n') {
            updated.push('\n');
        }
        return (updated, EnvChange::Replaced);
    }
    let mut updated = contents.to_string();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("{key}={value}\n"));
    (updated, EnvChange::Added)
}

/// Hides the password in a database URL.
fn redact(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(scheme), Some(at)) if at > scheme => {
            let credentials = &url[scheme + 3..at];
            match credentials.split_once(':') {
                Some((user, _)) => format!("{}{user}:•••{}", &url[..scheme + 3], &url[at..]),
                None => url.to_string(),
            }
        }
        _ => url.to_string(),
    }
}

fn is_git_repo(dir: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn is_git_ignored(dir: &Path, file: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["check-ignore", "-q", file])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "postgres://budget:pw@localhost:5432/budget";

    #[test]
    fn adds_to_empty_or_unterminated_files() {
        assert_eq!(
            upsert_env("", "DATABASE_URL", URL, |_| false),
            (format!("DATABASE_URL={URL}\n"), EnvChange::Added)
        );
        assert_eq!(
            upsert_env("A=1", "DATABASE_URL", URL, |_| false),
            (format!("A=1\nDATABASE_URL={URL}\n"), EnvChange::Added)
        );
    }

    #[test]
    fn ignores_comments_and_other_keys() {
        let contents = "# DATABASE_URL=old\nDATABASE_URL_POOL=x\n";
        let (updated, change) = upsert_env(contents, "DATABASE_URL", URL, |_| false);
        assert_eq!(change, EnvChange::Added);
        assert!(updated.starts_with(contents));
    }

    #[test]
    fn keeps_a_different_value_unless_forced() {
        let contents = "A=1\nexport DATABASE_URL=\"postgres://neon/db\"\nB=2\n";
        let (updated, change) = upsert_env(contents, "DATABASE_URL", URL, |_| false);
        assert_eq!(updated, contents);
        assert_eq!(change, EnvChange::Kept("postgres://neon/db".into()));

        let (updated, change) = upsert_env(contents, "DATABASE_URL", URL, |_| true);
        assert_eq!(updated, format!("A=1\nDATABASE_URL={URL}\nB=2\n"));
        assert_eq!(change, EnvChange::Replaced);
    }

    #[test]
    fn same_value_is_unchanged() {
        let contents = format!("DATABASE_URL={URL}\n");
        assert_eq!(
            upsert_env(&contents, "DATABASE_URL", URL, |_| true),
            (contents.clone(), EnvChange::Unchanged)
        );
    }

    #[test]
    fn redacts_passwords() {
        assert_eq!(
            redact("postgres://u:secret@host:5432/db"),
            "postgres://u:•••@host:5432/db"
        );
        assert_eq!(redact("postgres://host/db"), "postgres://host/db");
    }
}
