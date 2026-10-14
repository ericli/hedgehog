mod client;
mod commands;
mod pack;
mod style;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Drop a repo on your Mac, get a private URL.
///
/// Folders in ~/Hedgehog/apps are built, run in Docker with their own Postgres
/// database, and published through OpenTunnel for your paired devices only.
#[derive(Parser)]
#[command(name = "hedgehog", version, propagate_version = true)]
struct Cli {
    /// Which hedgehog to talk to (see `hedgehog remote ls`). Defaults to the
    /// default remote, or this Mac.
    #[arg(long, short, global = true, env = "HEDGEHOG_REMOTE")]
    remote: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Set this Mac up to host apps: checks tools, starts the daemon at login,
    /// and pairs your phone.
    Setup,
    /// Upload a folder (default: the current one) and deploy it.
    Deploy {
        path: Option<PathBuf>,
        /// App name. Defaults to `name` in hedgehog.toml, then the folder name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Rebuild an app from the folder already on the hedgehog Mac.
    Redeploy { app: String },
    /// List apps.
    #[command(alias = "list")]
    Ls,
    /// Show an app's output (or its last build with --build).
    Logs {
        app: String,
        #[arg(long, short)]
        follow: bool,
        #[arg(long, short = 'n', default_value_t = 200)]
        lines: usize,
        #[arg(long)]
        build: bool,
    },
    /// Show or change an app's environment variables.
    Env {
        app: String,
        #[command(subcommand)]
        action: Option<EnvAction>,
    },
    /// Start an app again with its current build (picks up env changes).
    Restart { app: String },
    /// Start an app that was stopped.
    Start { app: String },
    /// Stop an app until you start it again.
    Stop { app: String },
    /// Remove an app. Its folder goes to the Trash; data and database stay
    /// unless you --purge.
    Rm {
        app: String,
        #[arg(long)]
        purge: bool,
        #[arg(long, short)]
        yes: bool,
    },
    /// Open an app in your browser.
    Open { app: String },
    /// Let anyone with the URL open an app (on), or only your devices (off).
    Public {
        app: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Pair a device (phone, laptop browser) so it can open your apps.
    Pair {
        /// A name to recognise the device by later.
        #[arg(long)]
        label: Option<String>,
    },
    /// List paired devices, or revoke one.
    Devices {
        #[command(subcommand)]
        action: Option<DevicesAction>,
    },
    /// Open psql on an app's database (run on the hedgehog Mac).
    Db { app: String },
    /// Show the daemon, Docker, and tunnel status.
    Status,
    /// Check that everything hedgehog needs is installed.
    Doctor,
    /// Print this Mac's API token and how to deploy to it from a laptop.
    Token,
    /// Manage the hedgehog Macs this CLI can talk to.
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },
    /// Run or install the background daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand)]
enum EnvAction {
    /// Set variables: KEY=value ...
    Set { pairs: Vec<String> },
    /// Remove variables: KEY ...
    Unset { keys: Vec<String> },
    /// Set every variable from a dotenv file.
    Import { file: PathBuf },
}

#[derive(Subcommand)]
enum DevicesAction {
    /// Stop a device from opening your apps.
    Revoke { id: String },
}

#[derive(Subcommand)]
enum RemoteAction {
    /// Remember a hedgehog Mac, e.g. `hedgehog remote add home http://mac-mini:7700 --token ...`.
    Add {
        name: String,
        url: String,
        #[arg(long)]
        token: String,
        /// Don't make it the default.
        #[arg(long)]
        no_default: bool,
    },
    /// List remotes.
    Ls,
    /// Make a remote the default (`local` for this Mac).
    Use { name: String },
    /// Forget a remote.
    Rm { name: String },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Run the daemon in the foreground.
    Run {
        /// Keep apps on 127.0.0.1 instead of publishing them with OpenTunnel.
        #[arg(long, env = "HEDGEHOG_NO_TUNNEL")]
        no_tunnel: bool,
        /// Extra control API addresses (default: this Mac's Tailscale IP).
        #[arg(long)]
        listen: Vec<SocketAddr>,
    },
    /// Start the daemon at login (LaunchAgent).
    Install,
    /// Stop the daemon and remove the LaunchAgent.
    Uninstall,
    /// Show the daemon's log.
    Logs {
        #[arg(long, short)]
        follow: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // OpenTunnel (and our HTTP client) use rustls with the ring provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("could not start the async runtime");
    match runtime.block_on(commands::run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if !error.is::<commands::Reported>() {
                eprintln!("{} {error:#}", style::red("error:"));
            }
            ExitCode::FAILURE
        }
    }
}
