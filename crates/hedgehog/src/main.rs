mod client;
mod commands;
mod db;
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
    /// Create an app by cloning a Git repository on the hedgehog Mac.
    New {
        /// https://github.com/you/repo, git@github.com:you/repo, or a path on the Mac.
        git_url: String,
        /// App name. Defaults to the repository's name.
        #[arg(long)]
        name: Option<String>,
        /// Branch to deploy. Defaults to the repository's default branch.
        #[arg(long)]
        branch: Option<String>,
    },
    /// Show how to make GitHub deploy an app on every push.
    Webhook {
        app: String,
        /// Replace the webhook secret (update it in GitHub afterwards).
        #[arg(long)]
        rotate: bool,
    },
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
    /// Open the dashboard in your browser, signed in as an admin.
    Dashboard {
        /// Print a one-time sign-in link instead of opening it.
        #[arg(long)]
        no_open: bool,
    },
    /// Pair a device (phone, laptop browser) so it can open your apps.
    Pair {
        /// A name to recognise the device by later.
        #[arg(long)]
        label: Option<String>,
        /// Also let the device manage hedgehog from the dashboard (deploy,
        /// restart, env vars, devices), not just open apps.
        #[arg(long)]
        admin: bool,
    },
    /// List paired devices, or revoke one.
    Devices {
        #[command(subcommand)]
        action: Option<DevicesAction>,
    },
    /// A local dev Postgres for working on apps, and psql shortcuts.
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
    /// Show the daemon, Docker, and tunnel status.
    Status,
    /// Check that everything hedgehog needs is installed.
    Doctor,
    /// Print this Mac's API token and how to deploy to it from another machine.
    Token {
        /// Replace the token with a new one (if it leaked). The old one stops
        /// working immediately.
        #[arg(long)]
        rotate: bool,
    },
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
enum DbAction {
    /// Start a dev Postgres on localhost (same version hedgehog runs apps with).
    Up {
        /// Port on localhost (default 5432, or the last one you picked).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Stop the dev Postgres. Its data is kept.
    Down,
    /// Give a project its own dev database and add DATABASE_URL to its .env.
    Init {
        /// Project folder (default: the current one).
        path: Option<PathBuf>,
        /// Database name. Defaults to the app name `hedgehog deploy` would use.
        #[arg(long)]
        name: Option<String>,
        /// Replace a DATABASE_URL that's already in .env.
        #[arg(long)]
        force: bool,
    },
    /// Print a project's dev DATABASE_URL.
    Url {
        path: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
    },
    /// Empty a project's dev database (keeps its URL).
    Reset {
        path: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, short)]
        yes: bool,
    },
    /// Open psql on a deployed app's database (on the hedgehog Mac), or with
    /// --dev on a project's dev database.
    Psql {
        app: Option<String>,
        #[arg(long)]
        dev: bool,
    },
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
    // Exit quietly when piped into something like `head` that stops reading,
    // instead of panicking on a broken pipe.
    // SAFETY: called before any threads start; SIG_DFL is always valid.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
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
