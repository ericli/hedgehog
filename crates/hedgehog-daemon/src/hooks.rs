//! Deploy on push: GitHub calls `https://<home>/_hedgehog/hooks/<app>` on
//! every push, signed with a per-app secret, and hedgehog pulls and redeploys.
//! A webhook secret can only trigger "pull what's on the remote and deploy",
//! never upload code, so it is much less powerful than the API token.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use tracing::{info, warn};

use crate::Daemon;
use crate::deploy::Source;
use crate::secrets::random_token;

/// GitHub caps payloads at 25 MB; pushes are normally a few KB.
const MAX_PAYLOAD: usize = 25 * 1024 * 1024;

pub fn secret_key(app: &str) -> String {
    format!("webhook.secret.{app}")
}

/// The app's webhook secret, created the first time it's asked for.
pub fn secret(daemon: &Daemon, app: &str) -> anyhow::Result<String> {
    daemon
        .store
        .setting_or_init(&secret_key(app), || random_token(32))
}

pub fn rotate_secret(daemon: &Daemon, app: &str) -> anyhow::Result<String> {
    let secret = random_token(32);
    daemon.store.set_setting(&secret_key(app), &secret)?;
    Ok(secret)
}

pub fn url(daemon: &Daemon, app: &str) -> String {
    format!("{}/_hedgehog/hooks/{app}", daemon.gateway.home_url())
}

/// Checks GitHub's `X-Hub-Signature-256: sha256=<hex hmac>` header.
pub fn verify_signature(secret: &str, body: &[u8], header: &str) -> bool {
    let Some(signature) = header
        .strip_prefix("sha256=")
        .and_then(|hex| hex::decode(hex).ok())
    else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

#[derive(Deserialize)]
struct Push {
    #[serde(rename = "ref")]
    git_ref: String,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    deleted: bool,
}

pub fn router(daemon: Arc<Daemon>) -> Router {
    Router::new()
        .route(
            "/_hedgehog/hooks/{app}",
            post(receive).layer(DefaultBodyLimit::max(MAX_PAYLOAD)),
        )
        .with_state(daemon)
}

fn reply(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({ "message": message }))).into_response()
}

async fn receive(
    State(daemon): State<Arc<Daemon>>,
    Path(app): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    // Unknown apps and apps without a secret look the same to outsiders.
    let secret = match daemon.store.setting(&secret_key(&app)) {
        Ok(Some(secret)) => secret,
        _ => return reply(StatusCode::NOT_FOUND, "no webhook here"),
    };
    let signed = header("x-hub-signature-256")
        .is_some_and(|signature| verify_signature(&secret, &body, signature));
    if !signed {
        warn!(
            app,
            "rejected a webhook call with a missing or wrong signature"
        );
        return reply(StatusCode::UNAUTHORIZED, "bad signature");
    }
    match header("x-github-event") {
        Some("ping") => reply(StatusCode::OK, "pong: hedgehog is listening"),
        Some("push") => push(&daemon, &app, &body).await,
        _ => reply(StatusCode::ACCEPTED, "ignored: only push events deploy"),
    }
}

async fn push(daemon: &Arc<Daemon>, app: &str, body: &[u8]) -> Response {
    let Ok(push) = serde_json::from_slice::<Push>(body) else {
        return reply(
            StatusCode::BAD_REQUEST,
            "set the webhook's content type to application/json",
        );
    };
    let Ok(Some(record)) = daemon.store.app(app) else {
        return reply(StatusCode::NOT_FOUND, "no webhook here");
    };
    let Some(branch) = crate::git::current_branch(&record.source_dir).await else {
        return reply(
            StatusCode::CONFLICT,
            "this app's folder isn't on a git branch",
        );
    };
    if push.deleted || push.git_ref != format!("refs/heads/{branch}") {
        return reply(
            StatusCode::ACCEPTED,
            &format!("ignored: only pushes to {branch} deploy"),
        );
    }
    if !daemon.queue_pull_deploy(app) {
        return reply(StatusCode::ACCEPTED, "a deploy is already queued");
    }
    let commit = push.after.as_deref().unwrap_or("?");
    info!(app, commit, "push webhook: pulling and deploying");
    reply(StatusCode::ACCEPTED, "deploying")
}

impl Daemon {
    /// Pulls and deploys `app` in the background. While one of these is
    /// waiting to start, more pushes just ride along with it, so a burst of
    /// pushes means one queued deploy, not many. Returns false if one was
    /// already queued.
    pub fn queue_pull_deploy(self: &Arc<Self>, app: &str) -> bool {
        if !self.pending_pulls.lock().unwrap().insert(app.to_string()) {
            return false;
        }
        let daemon = self.clone();
        let app = app.to_string();
        tokio::spawn(async move {
            // Let any deploy in progress finish; it may predate the push.
            drop(daemon.lock(&app).lock().await);
            daemon.pending_pulls.lock().unwrap().remove(&app);
            let events = daemon.log_events(&app);
            let _ = daemon.deploy(&app, Source::Pull, events).await;
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_github_signatures() {
        // HMAC-SHA256("It's a Secret to Everybody", "Hello, World!"), from
        // GitHub's webhook documentation.
        let secret = "It's a Secret to Everybody";
        let body = b"Hello, World!";
        let header = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(verify_signature(secret, body, header));
        assert!(!verify_signature(secret, b"Hello, World?", header));
        assert!(!verify_signature("wrong", body, header));
        assert!(!verify_signature(secret, body, "sha1=abc"));
        assert!(!verify_signature(secret, body, "sha256=not-hex"));
    }
}
