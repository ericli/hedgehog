//! Git operations on app folders. Prompts are always off, because a
//! background service can't answer them: a repo that needs a password fails
//! quickly instead of hanging.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::build::run_logged;
use crate::secrets::random_token;

/// `git pull` should be quick; longer means it's stuck on a login.
const PULL_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const CLONE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CREDENTIALS_HINT: &str = "If the repo is private, make sure this Mac can fetch it without a password prompt (an SSH key, or `gh auth login`)";

fn git() -> Command {
    let mut command = Command::new("git");
    command
        .args(["-c", "protocol.ext.allow=never"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    command
}

/// Accepts https, ssh (`ssh://` or `user@host:path`), and local paths.
/// Rejects anything that could be read as an option or that uses git's
/// `transport::address` helpers, which can run commands.
pub fn validate_url(url: &str) -> Result<(), String> {
    let bad = url.is_empty()
        || url.starts_with('-')
        || url.contains("::")
        || url.chars().any(|c| c.is_whitespace() || c.is_control());
    let scp_like = url.contains('@')
        && url
            .split_once(':')
            .is_some_and(|(host, _)| !host.contains('/'));
    let known = ["https://", "ssh://", "file://", "/"]
        .iter()
        .any(|prefix| url.starts_with(prefix));
    if bad || !(known || scp_like) {
        return Err(format!(
            "`{url}` doesn't look like a Git URL. Use https://…, git@host:owner/repo, or a path on the Mac"
        ));
    }
    Ok(())
}

/// Branch names hedgehog will pass to git.
pub fn validate_branch(branch: &str) -> Result<(), String> {
    let ok = !branch.is_empty()
        && !branch.starts_with('-')
        && !branch.contains("..")
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
    ok.then_some(())
        .ok_or_else(|| format!("`{branch}` isn't a branch name hedgehog can use"))
}

/// Clones into a staging folder next to `dest`, then moves it into place, so
/// the folder scanner never sees a half-finished clone.
pub async fn clone(
    url: &str,
    branch: Option<&str>,
    staging_root: &Path,
    dest: &Path,
    log: &(dyn Fn(String) + Send + Sync),
) -> Result<()> {
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    std::fs::create_dir_all(staging_root)?;
    let staging = staging_root.join(format!("clone-{}", random_token(4)));
    let mut command = git();
    command.arg("clone");
    if let Some(branch) = branch {
        command.args(["--branch", branch, "--single-branch"]);
    }
    command.arg("--").arg(url).arg(&staging);
    if let Err(error) = run_logged(command, log, CLONE_TIMEOUT).await {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error).context(format!("git clone failed. {CREDENTIALS_HINT}"));
    }
    std::fs::rename(&staging, dest)
        .with_context(|| format!("could not move the clone to {}", dest.display()))?;
    Ok(())
}

/// Fast-forwards an app folder to its upstream branch.
pub async fn pull(dir: &Path, log: &(dyn Fn(String) + Send + Sync)) -> Result<()> {
    let mut command = git();
    command.arg("-C").arg(dir).args(["pull", "--ff-only"]);
    run_logged(command, log, PULL_TIMEOUT)
        .await
        .context(format!("git pull failed. {CREDENTIALS_HINT}"))
}

async fn output(dir: &Path, args: &[&str]) -> Option<String> {
    if !dir.join(".git").exists() {
        return None;
    }
    let output = git().arg("-C").arg(dir).args(args).output().await.ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|text| !text.is_empty())
}

/// The commit an app folder is at, if it is a git repo.
pub async fn revision(dir: &Path) -> Option<String> {
    output(dir, &["rev-parse", "HEAD"]).await
}

/// The branch an app folder has checked out, if any.
pub async fn current_branch(dir: &Path) -> Option<String> {
    output(dir, &["symbolic-ref", "--short", "HEAD"]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_urls() {
        for ok in [
            "https://github.com/me/budget.git",
            "git@github.com:me/budget.git",
            "ssh://git@github.com/me/budget.git",
            "/Users/me/code/budget",
            "file:///Users/me/code/budget",
        ] {
            assert!(validate_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "--upload-pack=touch /tmp/x",
            "ext::sh -c touch% /tmp/x",
            "fd::7",
            "http://github.com/me/budget",
            "github.com/me/budget",
            "https://github.com/me/bud get",
        ] {
            assert!(validate_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn validates_branches() {
        assert!(validate_branch("main").is_ok());
        assert!(validate_branch("feature/x-1.2").is_ok());
        assert!(validate_branch("-x").is_err());
        assert!(validate_branch("a..b").is_err());
        assert!(validate_branch("a b").is_err());
    }

    #[tokio::test]
    async fn clones_pulls_and_reads_branches() {
        let root = tempfile::tempdir().unwrap();
        let run = |dir: &Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.email=t@t", "-c", "user.name=t"])
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        let origin = root.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        run(&origin, &["init", "-q", "-b", "main"]);
        std::fs::write(origin.join("app.txt"), "one").unwrap();
        run(&origin, &["add", "-A"]);
        run(&origin, &["commit", "-qm", "one"]);

        let dest = root.path().join("apps/budget");
        let staging = root.path().join("apps/.incoming");
        clone(
            origin.to_str().unwrap(),
            Some("main"),
            &staging,
            &dest,
            &|_| {},
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("app.txt")).unwrap(),
            "one"
        );
        assert_eq!(current_branch(&dest).await.as_deref(), Some("main"));
        let first = revision(&dest).await.unwrap();
        assert!(
            std::fs::read_dir(&staging).unwrap().next().is_none(),
            "staging is cleaned up"
        );

        std::fs::write(origin.join("app.txt"), "two").unwrap();
        run(&origin, &["commit", "-qam", "two"]);
        pull(&dest, &|_| {}).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("app.txt")).unwrap(),
            "two"
        );
        assert_ne!(revision(&dest).await.unwrap(), first);

        let missing = clone(
            "/no/such/repo",
            None,
            &staging,
            &root.path().join("x"),
            &|_| {},
        )
        .await;
        assert!(missing.is_err());
        assert!(!root.path().join("x").exists());
    }
}
