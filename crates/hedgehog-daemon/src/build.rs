//! Turns an app folder into a Docker image: its own Dockerfile when it has
//! one, otherwise Railpack (Railway's open-source builder) works out how.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hedgehog_core::Manifest;
use hedgehog_core::manifest::Builder;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

use crate::docker::Docker;

const RAILPACK_FRONTEND: &str = "ghcr.io/railwayapp/railpack-frontend";
/// A build that takes longer than this is assumed stuck (e.g. waiting on a
/// registry login) and is killed.
const STEP_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub fn builder_name(builder: Builder) -> &'static str {
    match builder {
        Builder::Dockerfile => "dockerfile",
        Builder::Railpack => "railpack",
    }
}

/// Builds `dir` into `image`, sending each output line to `log`.
pub async fn build(
    docker: &Docker,
    dir: &Path,
    manifest: &Manifest,
    image: &str,
    work_dir: &Path,
    log: &(dyn Fn(String) + Send + Sync),
) -> Result<Builder> {
    let builder = manifest.builder(dir);
    let mut command = docker.cli();
    command.args([
        "buildx",
        "build",
        "--load",
        "--progress",
        "plain",
        "-t",
        image,
    ]);
    match builder {
        Builder::Dockerfile => {
            command.arg("-f").arg(dir.join(manifest.dockerfile()));
        }
        Builder::Railpack => {
            std::fs::create_dir_all(work_dir)?;
            let plan = work_dir.join("railpack-plan.json");
            let mut prepare = Command::new("railpack");
            prepare
                .arg("prepare")
                .arg(dir)
                .arg("--plan-out")
                .arg(&plan)
                .arg("--info-out")
                .arg(work_dir.join("railpack-info.json"));
            if let Some(start) = &manifest.start {
                prepare.arg("--start-cmd").arg(start);
            }
            run_logged(prepare, log, STEP_TIMEOUT)
                .await
                .context("railpack could not work out how to build this app. Add a Dockerfile, or set `start` in hedgehog.toml")?;
            command
                .arg("--build-arg")
                .arg(format!("BUILDKIT_SYNTAX={RAILPACK_FRONTEND}"))
                .arg("-f")
                .arg(&plan);
        }
    }
    command.arg(dir);
    run_logged(command, log, STEP_TIMEOUT)
        .await
        .context("the image build failed")?;
    Ok(builder)
}

pub async fn railpack_installed() -> bool {
    Command::new("railpack")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// Runs a command, streaming stdout and stderr lines to `log`.
pub(crate) async fn run_logged(
    mut command: Command,
    log: &(dyn Fn(String) + Send + Sync),
    timeout: Duration,
) -> Result<()> {
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("could not run `{program}`. Is it installed and on PATH?"))?;
    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(forward_lines(stdout, tx.clone()));
    tokio::spawn(forward_lines(stderr, tx));
    let finished = async {
        while let Some(line) = rx.recv().await {
            log(line);
        }
        child.wait().await
    };
    // On timeout `child` is dropped here, which kills it (`kill_on_drop`).
    let status = match tokio::time::timeout(timeout, finished).await {
        Ok(status) => status?,
        Err(_) => bail!(
            "`{program}` ran for over {} minutes and was stopped",
            timeout.as_secs() / 60
        ),
    };
    if !status.success() {
        bail!("`{program}` exited with {status}");
    }
    Ok(())
}

async fn forward_lines(
    stream: impl AsyncRead + Unpin,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if tx.send(line).is_err() {
            break;
        }
    }
}
