use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use hedgehog_core::api::{
    AppInfo, AppStatus, DeployEvent, DeviceInfo, EnvUpdate, PairRequest, PairResponse,
    PublicUpdate, Status,
};
use hedgehog_core::env::{EnvMap, read_dotenv_file};
use hedgehog_core::names::{app_name_from_dir, is_valid_app_name};
use hedgehog_core::{API_PORT, Manifest, Paths};
use reqwest::Method;

use crate::client::{Client, Remote, RemotesFile};
use crate::pack::pack;
use crate::style::{bold, dim, green, red, yellow};
use crate::{Cli, Command, DaemonAction, DevicesAction, EnvAction, RemoteAction};

/// An error that was already shown to the user; exit non-zero quietly.
#[derive(Debug)]
pub struct Reported;

impl std::fmt::Display for Reported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("already reported")
    }
}

impl std::error::Error for Reported {}

pub async fn run(cli: Cli) -> Result<()> {
    let paths = Paths::from_env()?;
    let remote = cli.remote.as_deref();
    let client = || Client::resolve(&paths, remote);
    match cli.command {
        Command::Setup => setup(&paths).await,
        Command::Deploy { path, name } => deploy(&client()?, path, name).await,
        Command::Redeploy { app } => {
            let client = client()?;
            let response = client
                .send_empty(Method::POST, &format!("/v1/apps/{app}/deploy"))
                .await?;
            follow_events(response).await
        }
        Command::Ls => list(&client()?).await,
        Command::Logs {
            app,
            follow,
            lines,
            build,
        } => logs(&client()?, &app, follow, lines, build).await,
        Command::Env { app, action } => env(&client()?, &app, action).await,
        Command::Restart { app } | Command::Start { app } => {
            let response = client()?
                .send_empty(Method::POST, &format!("/v1/apps/{app}/restart"))
                .await?;
            follow_events(response).await
        }
        Command::Stop { app } => {
            client()?
                .send_empty(Method::POST, &format!("/v1/apps/{app}/stop"))
                .await?;
            println!("Stopped {app}. Start it again with `hedgehog start {app}`.");
            Ok(())
        }
        Command::Rm { app, purge, yes } => remove(&client()?, &app, purge, yes).await,
        Command::Open { app } => open(&client()?, &app).await,
        Command::Public { app, state } => {
            let public = state == "on";
            client()?
                .send(
                    Method::PUT,
                    &format!("/v1/apps/{app}/public"),
                    &PublicUpdate { public },
                )
                .await?;
            if public {
                println!("{app} is now public: anyone with its URL can open it.");
            } else {
                println!("{app} is private again: only your paired devices can open it.");
            }
            Ok(())
        }
        Command::Pair { label } => pair(&client()?, label).await,
        Command::Devices { action } => devices(&client()?, action).await,
        Command::Db { app } => db(&client()?, &app),
        Command::Status => status(&client()?).await,
        Command::Doctor => doctor(&paths, remote).await,
        Command::Token => token(&paths).await,
        Command::Remote { action } => remote_command(&paths, action),
        Command::Daemon { action } => daemon(&paths, action).await,
    }
}

fn human_size(bytes: usize) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{:.1} KB", bytes as f64 / 1e3),
        _ => format!("{:.1} MB", bytes as f64 / 1e6),
    }
}

async fn deploy(client: &Client, path: Option<PathBuf>, name: Option<String>) -> Result<()> {
    let dir = match path {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    let dir = dir
        .canonicalize()
        .with_context(|| format!("{} doesn't exist", dir.display()))?;
    let manifest = Manifest::load(&dir)?;
    let folder = dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let name = match name.or(manifest.name.clone()) {
        Some(name) if is_valid_app_name(&name) => name,
        Some(name) => {
            bail!("`{name}` isn't a valid app name: use lowercase letters, digits, and dashes")
        }
        None => app_name_from_dir(&folder)
            .with_context(|| format!("can't make an app name from `{folder}`; pass --name"))?,
    };

    let packing_dir = dir.clone();
    let packed = tokio::task::spawn_blocking(move || pack(&packing_dir)).await??;
    println!(
        "{} {name} from {} ({} files, {}) to {}",
        bold("Deploying"),
        dir.display(),
        packed.files,
        human_size(packed.bytes.len()),
        client.label
    );
    if packed.skipped_env {
        println!(
            "{}",
            dim(&format!(
                "note: .env is gitignored, so it wasn't uploaded. Send its values with `hedgehog env {name} import .env`"
            ))
        );
    }
    let response = client
        .request(
            Method::POST,
            &format!("/v1/apps/{name}/deploy"),
            |request| {
                request
                    .header("content-type", "application/gzip")
                    .body(packed.bytes)
            },
        )
        .await?;
    follow_events(response).await
}

/// Prints a deploy's NDJSON progress stream until it finishes.
async fn follow_events(response: reqwest::Response) -> Result<()> {
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await {
        buffer.extend_from_slice(&chunk?);
        while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=newline).collect();
            let Ok(event) = serde_json::from_slice::<DeployEvent>(&line) else {
                continue;
            };
            match event {
                DeployEvent::Step { message } => println!("{} {message}", bold("==>")),
                DeployEvent::Log { line } => println!("    {}", dim(&line)),
                DeployEvent::Done { url } => {
                    match url {
                        Some(url) => println!("\n{} {}", green("✓ Live at"), bold(&url)),
                        None => println!("\n{}", green("✓ Deployed")),
                    }
                    return Ok(());
                }
                DeployEvent::Failed { error } => {
                    eprintln!("\n{} {error}", red("✗ Deploy failed:"));
                    return Err(Reported.into());
                }
            }
        }
    }
    bail!(
        "lost the connection before the deploy finished. Check `hedgehog ls` and `hedgehog logs <app> --build`"
    )
}

fn status_label(app: &AppInfo) -> String {
    let text = app.status.as_str();
    match app.status {
        AppStatus::Running => green(text),
        AppStatus::Failed => red(text),
        AppStatus::Building | AppStatus::New => yellow(text),
        AppStatus::Stopped => dim(text),
    }
}

async fn list(client: &Client) -> Result<()> {
    let apps: Vec<AppInfo> = client.get("/v1/apps").await?;
    if apps.is_empty() {
        println!(
            "No apps yet. Drop a folder into ~/Hedgehog/apps on the hedgehog Mac, or run `hedgehog deploy` in one."
        );
        return Ok(());
    }
    let width = apps
        .iter()
        .map(|app| app.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{}",
        dim(&format!(
            "{:width$}  {:9} {:8} URL",
            "NAME", "STATUS", "ACCESS"
        ))
    );
    for app in &apps {
        let status = status_label(app);
        let padding = " ".repeat(9usize.saturating_sub(app.status.as_str().len()));
        let access = if app.public { "public" } else { "private" };
        println!(
            "{:width$}  {status}{padding} {access:8} {}",
            app.name,
            app.url.as_deref().unwrap_or("-")
        );
        if let Some(error) = &app.last_error {
            let first = error.lines().next().unwrap_or_default();
            println!(
                "{:width$}  {}",
                "",
                dim(&format!("↳ last deploy failed: {first}"))
            );
        }
    }
    Ok(())
}

async fn logs(client: &Client, app: &str, follow: bool, lines: usize, build: bool) -> Result<()> {
    let path = format!("/v1/apps/{app}/logs?follow={follow}&tail={lines}&build={build}");
    let response = client.send_empty(Method::GET, &path).await?;
    let mut stream = response.bytes_stream();
    let mut stdout = std::io::stdout();
    while let Some(chunk) = stream.next().await {
        stdout.write_all(&chunk?)?;
        stdout.flush()?;
    }
    Ok(())
}

fn parse_pairs(pairs: &[String]) -> Result<EnvMap> {
    pairs
        .iter()
        .map(|pair| {
            let (key, value) = pair
                .split_once('=')
                .with_context(|| format!("`{pair}` should look like KEY=value"))?;
            Ok((key.to_string(), value.to_string()))
        })
        .collect()
}

async fn env(client: &Client, app: &str, action: Option<EnvAction>) -> Result<()> {
    let path = format!("/v1/apps/{app}/env");
    let update = match action {
        None => {
            let env: EnvMap = client.get(&path).await?;
            if env.is_empty() {
                println!("{app} has no variables set with hedgehog.");
            }
            for (key, value) in env {
                println!("{key}={value}");
            }
            return Ok(());
        }
        Some(EnvAction::Set { pairs }) => EnvUpdate {
            set: parse_pairs(&pairs)?,
            unset: vec![],
        },
        Some(EnvAction::Unset { keys }) => EnvUpdate {
            set: EnvMap::new(),
            unset: keys,
        },
        Some(EnvAction::Import { file }) => EnvUpdate {
            set: read_dotenv_file(&file)?,
            unset: vec![],
        },
    };
    let changed = update.set.len() + update.unset.len();
    let response: serde_json::Value = client
        .send(Method::PATCH, &path, &update)
        .await?
        .json()
        .await?;
    let restarting = response["restarting"].as_bool().unwrap_or(false);
    println!(
        "Updated {changed} variable{} for {app}.{}",
        if changed == 1 { "" } else { "s" },
        if restarting {
            " Restarting it to apply them."
        } else {
            ""
        }
    );
    Ok(())
}

fn confirm(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

async fn remove(client: &Client, app: &str, purge: bool, yes: bool) -> Result<()> {
    if purge
        && !yes
        && !confirm(&format!(
            "Delete {app} including its data folder and database?"
        ))?
    {
        println!("Nothing removed.");
        return Ok(());
    }
    client
        .send_empty(Method::DELETE, &format!("/v1/apps/{app}?purge={purge}"))
        .await?;
    if purge {
        println!("Removed {app}, its data, and its database. Its folder is in the Trash.");
    } else {
        println!("Removed {app}. Its folder is in the Trash; its data and database are kept.");
    }
    Ok(())
}

async fn open(client: &Client, app: &str) -> Result<()> {
    let apps: Vec<AppInfo> = client.get("/v1/apps").await?;
    let url = apps
        .into_iter()
        .find(|candidate| candidate.name == app)
        .with_context(|| format!("there is no app called {app}"))?
        .url
        .with_context(|| format!("{app} has no URL yet"))?;
    std::process::Command::new("open").arg(&url).status()?;
    println!("{url}");
    Ok(())
}

fn print_qr(text: &str) -> Result<()> {
    let code = qrcode::QrCode::new(text.as_bytes())?;
    let rendered = code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build();
    println!("{rendered}");
    Ok(())
}

async fn pair(client: &Client, label: Option<String>) -> Result<()> {
    let response: PairResponse = client
        .send(Method::POST, "/v1/pair", &PairRequest { label })
        .await?
        .json()
        .await?;
    show_pairing(&response)
}

fn show_pairing(response: &PairResponse) -> Result<()> {
    println!(
        "Scan this with your phone's camera, or open the link on the device you want to pair:\n"
    );
    print_qr(&response.url)?;
    println!("{}\n", bold(&response.url));
    println!(
        "{}",
        dim(&format!(
            "The link works once and expires in {} minutes.",
            response.expires_in_secs / 60
        ))
    );
    if response.url.starts_with("http://127.0.0.1") {
        println!(
            "{}",
            yellow("The tunnel is off, so this link only works on the hedgehog Mac itself.")
        );
    }
    Ok(())
}

async fn devices(client: &Client, action: Option<DevicesAction>) -> Result<()> {
    if let Some(DevicesAction::Revoke { id }) = action {
        client
            .send_empty(Method::DELETE, &format!("/v1/devices/{id}"))
            .await?;
        println!("Revoked {id}. That device can no longer open your private apps.");
        return Ok(());
    }
    let devices: Vec<DeviceInfo> = client.get("/v1/devices").await?;
    if devices.is_empty() {
        println!("No paired devices. Pair one with `hedgehog pair`.");
    }
    for device in devices {
        println!(
            "{}  {}",
            device.id,
            device.label.as_deref().unwrap_or("(no label)")
        );
    }
    Ok(())
}

fn db(client: &Client, app: &str) -> Result<()> {
    if !client.local {
        bail!("`hedgehog db` opens psql through Docker, so run it on the hedgehog Mac itself");
    }
    let status = std::process::Command::new("docker")
        .args(["exec", "-it", "hedgehog-postgres", "psql", "-U", app, app])
        .status()
        .context("could not run docker")?;
    if !status.success() {
        bail!("psql exited with {status}");
    }
    Ok(())
}

fn tick(ok: bool) -> String {
    if ok { green("ok") } else { red("missing") }
}

async fn status(client: &Client) -> Result<()> {
    let status: Status = client.get("/v1/status").await?;
    print_status(client, &status);
    Ok(())
}

fn print_status(client: &Client, status: &Status) {
    println!(
        "{} {} on {}",
        bold("hedgehog"),
        status.version,
        client.label
    );
    println!("  docker    {}", tick(status.docker));
    println!(
        "  railpack  {}",
        if status.railpack {
            green("ok")
        } else {
            yellow("missing (needed for apps without a Dockerfile)")
        }
    );
    let tunnel = match status.tunnel.state.as_str() {
        "connected" => green("connected"),
        "off" => dim("off"),
        other => yellow(other),
    };
    println!("  tunnel    {tunnel}");
    if let Some(error) = &status.tunnel.error {
        println!("            {}", dim(error));
    }
    if let Some(url) = &status.home_url {
        println!("  home      {url}");
    }
    println!("  apps      {} in {}", status.apps, status.apps_dir);
}

fn command_works(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

struct Checks {
    docker_cli: bool,
    docker_engine: bool,
    buildx: bool,
    railpack: bool,
    git: bool,
}

fn run_checks() -> Checks {
    let docker_cli = command_works("docker", &["--version"]);
    Checks {
        docker_cli,
        docker_engine: docker_cli && command_works("docker", &["info"]),
        buildx: docker_cli && command_works("docker", &["buildx", "version"]),
        railpack: command_works("railpack", &["--version"]),
        git: command_works("git", &["--version"]),
    }
}

fn print_checks(checks: &Checks) {
    println!("  docker CLI     {}", tick(checks.docker_cli));
    println!(
        "  docker engine  {}",
        if checks.docker_engine {
            green("ok")
        } else {
            red("not running (open Docker Desktop or OrbStack)")
        }
    );
    println!("  docker buildx  {}", tick(checks.buildx));
    println!("  railpack       {}", tick(checks.railpack));
    println!("  git            {}", tick(checks.git));
    if !checks.docker_cli {
        println!(
            "\n{}",
            dim("Install Docker Desktop (docker.com) or OrbStack (`brew install orbstack`).")
        );
    }
    if !checks.railpack {
        println!(
            "\n{}",
            dim(
                "Railpack builds apps that have no Dockerfile. Install it with:\n  curl -sSL https://railpack.com/install.sh | sh"
            )
        );
    }
}

async fn doctor(paths: &Paths, remote: Option<&str>) -> Result<()> {
    println!("{}", bold("This Mac"));
    let checks = run_checks();
    print_checks(&checks);
    println!(
        "  daemon         {}",
        if hedgehog_daemon::launchd::is_loaded() {
            green("installed")
        } else {
            dim("not installed here")
        }
    );
    match Client::resolve(paths, remote) {
        Ok(client) => match client.get::<Status>("/v1/status").await {
            Ok(status) => {
                println!();
                print_status(&client, &status);
            }
            Err(error) => println!("\n{} {error:#}", yellow("daemon:")),
        },
        Err(error) => println!("\n{} {error:#}", yellow("daemon:")),
    }
    Ok(())
}

async fn tailscale_name() -> Option<String> {
    for program in [
        "tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ] {
        let Ok(output) = tokio::process::Command::new(program)
            .args(["status", "--json"])
            .output()
            .await
        else {
            continue;
        };
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
        let name = status["Self"]["DNSName"].as_str()?.trim_end_matches('.');
        return (!name.is_empty()).then(|| name.to_string());
    }
    None
}

async fn token(paths: &Paths) -> Result<()> {
    let token = std::fs::read_to_string(paths.token())
        .map(|token| token.trim().to_string())
        .context("this Mac isn't set up yet. Run `hedgehog setup` first")?;
    let tailscale = tailscale_name().await;
    let host = tailscale.as_deref().unwrap_or("<this-mac>");
    println!("{token}\n");
    println!("On your laptop, run:\n");
    println!("  hedgehog remote add home http://{host}:{API_PORT} --token {token}");
    if tailscale.is_none() {
        println!(
            "\n{}",
            yellow("Tailscale isn't running here, so other machines can't reach this Mac yet.")
        );
    }
    Ok(())
}

fn remote_command(paths: &Paths, action: RemoteAction) -> Result<()> {
    let file = paths.remotes();
    let mut remotes = RemotesFile::load(&file)?;
    match action {
        RemoteAction::Add {
            name,
            url,
            token,
            no_default,
        } => {
            if name == "local" {
                bail!("`local` always means this Mac; pick another name");
            }
            let url = if url.contains("://") {
                url
            } else {
                format!("http://{url}")
            };
            remotes.remotes.insert(
                name.clone(),
                Remote {
                    url: url.clone(),
                    token,
                },
            );
            if !no_default {
                remotes.default = Some(name.clone());
            }
            remotes.save(&file)?;
            println!("Added {name} ({url}).");
            if !no_default {
                println!("Commands now go to {name}. Use `--remote local` for this Mac.");
            }
        }
        RemoteAction::Ls => {
            let default = remotes.default.clone().unwrap_or_else(|| "local".into());
            let marker = |name: &str| if name == default { "*" } else { " " };
            println!("{} local  {}", marker("local"), dim("this Mac"));
            for (name, remote) in &remotes.remotes {
                println!("{} {name}  {}", marker(name), dim(&remote.url));
            }
        }
        RemoteAction::Use { name } => {
            if name != "local" && !remotes.remotes.contains_key(&name) {
                bail!("no remote called `{name}`");
            }
            remotes.default = (name != "local").then_some(name.clone());
            remotes.save(&file)?;
            println!("Commands now go to {name}.");
        }
        RemoteAction::Rm { name } => {
            if remotes.remotes.remove(&name).is_none() {
                bail!("no remote called `{name}`");
            }
            if remotes.default.as_deref() == Some(name.as_str()) {
                remotes.default = None;
            }
            remotes.save(&file)?;
            println!("Removed {name}.");
        }
    }
    Ok(())
}

async fn daemon(paths: &Paths, action: DaemonAction) -> Result<()> {
    match action {
        DaemonAction::Run { no_tunnel, listen } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info,hyper=warn,bollard=warn".into()),
                )
                .with_target(false)
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
                .init();
            hedgehog_daemon::run(
                paths.clone(),
                hedgehog_daemon::Options {
                    tunnel: !no_tunnel,
                    listen,
                },
            )
            .await
        }
        DaemonAction::Install => install_daemon(paths),
        DaemonAction::Uninstall => {
            hedgehog_daemon::launchd::uninstall()?;
            println!(
                "Stopped the daemon and removed it from login items. Your apps folder is untouched."
            );
            Ok(())
        }
        DaemonAction::Logs { follow } => {
            let mut command = std::process::Command::new("tail");
            command.args(["-n", "200"]);
            if follow {
                command.arg("-f");
            }
            command.arg(paths.daemon_log()).status()?;
            Ok(())
        }
    }
}

fn install_daemon(paths: &Paths) -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    if exe.components().any(|part| part.as_os_str() == "target") {
        println!(
            "{}",
            yellow(&format!(
                "note: installing {} from a build folder. `cargo install --path crates/hedgehog` gives the daemon a stable home.",
                exe.display()
            ))
        );
    }
    let plist = hedgehog_daemon::launchd::install(paths, &exe)?;
    println!(
        "Installed {}. The daemon starts now and at every login.",
        plist.display()
    );
    Ok(())
}

async fn wait_for<T>(timeout: Duration, mut check: impl AsyncFnMut() -> Option<T>) -> Option<T> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if let Some(value) = check().await {
            return Some(value);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    None
}

async fn setup(paths: &Paths) -> Result<()> {
    println!("{}", bold("Checking this Mac"));
    let checks = run_checks();
    print_checks(&checks);
    if !checks.docker_engine || !checks.buildx {
        bail!(
            "hedgehog needs Docker (with buildx) running. Start Docker Desktop or OrbStack and run `hedgehog setup` again"
        );
    }

    println!("\n{}", bold("Starting the daemon"));
    std::fs::create_dir_all(&paths.apps)?;
    hedgehog_daemon::load_or_create_token(paths)?;
    install_daemon(paths)?;
    let client = Client::resolve(paths, Some("local"))?;
    let status = wait_for(Duration::from_secs(60), async || {
        client.get::<Status>("/v1/status").await.ok()
    })
    .await
    .with_context(|| {
        format!(
            "the daemon didn't answer within a minute. Check `hedgehog daemon logs` ({})",
            paths.daemon_log().display()
        )
    })?;

    println!("\n{}", bold("Connecting the tunnel"));
    let status = if status.tunnel.state == "connected" {
        status
    } else {
        wait_for(Duration::from_secs(90), async || {
            let status = client.get::<Status>("/v1/status").await.ok()?;
            (status.tunnel.state == "connected").then_some(status)
        })
        .await
        .unwrap_or(status)
    };
    print_status(&client, &status);

    println!("\n{}", bold("Pair your phone"));
    let response: PairResponse = client
        .send(
            Method::POST,
            "/v1/pair",
            &PairRequest {
                label: Some("phone".into()),
            },
        )
        .await?
        .json()
        .await?;
    show_pairing(&response)?;

    println!("\n{}", bold("You're set"));
    println!(
        "  • Drop a project folder into {} and it goes live.",
        paths.apps.display()
    );
    println!("  • Or run `hedgehog deploy` inside a project folder.");
    println!("  • To deploy from your laptop, run `hedgehog token` here and follow along.");
    println!(
        "  • {}",
        dim(
            "For a home server: turn on automatic login, and set the Mac to restart after a power failure."
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_env_pairs() {
        let env = parse_pairs(&["A=1".into(), "B=x=y".into(), "C=".into()]).unwrap();
        assert_eq!(env["A"], "1");
        assert_eq!(env["B"], "x=y");
        assert_eq!(env["C"], "");
        assert!(parse_pairs(&["nope".into()]).is_err());
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2_500), "2.5 KB");
        assert_eq!(human_size(3_200_000), "3.2 MB");
    }
}
