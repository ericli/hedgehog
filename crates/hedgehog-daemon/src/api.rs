//! The control API the `hedgehog` CLI talks to. Always on `127.0.0.1:7700`,
//! and on the Tailscale IP when there is one, so a laptop can deploy to the
//! Mac at home. Every request needs the bearer token.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use futures_util::StreamExt;
use hedgehog_core::api::{
    AppInfo, CreateApp, DeployEvent, EnvUpdate, ErrorBody, PairRequest, PairResponse, PublicUpdate,
    Status, WebhookInfo,
};
use hedgehog_core::names::is_valid_app_name;
use hedgehog_core::{API_PORT, Manifest};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{info, warn};

use crate::auth::Check;
use crate::deploy::Source;
use crate::gateway::{CurrentDevice, PAIR_PATH};
use crate::secrets::{hash_token, now, random_token};
use crate::store::{AppRecord, PairGrant};
use crate::{Daemon, VERSION, build};

const PAIR_CODE_TTL: u64 = 10 * 60;

type AppState = Arc<Daemon>;

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn not_found(app: &str) -> ApiError {
    ApiError(
        StatusCode::NOT_FOUND,
        format!("there is no app called {app}"),
    )
}

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.into())
}

fn record(daemon: &Daemon, app: &str) -> ApiResult<AppRecord> {
    daemon.store.app(app)?.ok_or_else(|| not_found(app))
}

/// The control API for the CLI: every route needs the bearer token.
pub fn router(daemon: AppState) -> Router {
    routes(daemon.clone()).route_layer(middleware::from_fn_with_state(daemon, require_token))
}

/// The same routes with no token check, for the dashboard. Only the gateway
/// serves these, after checking for an admin device; see `gateway::dashboard_api`.
pub fn dashboard_router(daemon: AppState) -> Router {
    routes(daemon)
}

fn routes(daemon: AppState) -> Router<()> {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/apps", get(list_apps).post(create_app))
        .route(
            "/v1/apps/{name}/deploy",
            post(deploy).layer(DefaultBodyLimit::disable()),
        )
        .route("/v1/apps/{name}/restart", post(restart))
        .route("/v1/apps/{name}/pull", post(pull))
        .route("/v1/apps/{name}/webhook", get(webhook))
        .route("/v1/apps/{name}/webhook/rotate", post(rotate_webhook))
        .route("/v1/apps/{name}/stop", post(stop))
        .route("/v1/apps/{name}", delete(remove))
        .route("/v1/apps/{name}/logs", get(logs))
        .route("/v1/apps/{name}/env", get(get_env).patch(update_env))
        .route("/v1/apps/{name}/public", put(set_public))
        .route("/v1/pair", post(pair))
        .route("/v1/devices", get(devices))
        .route("/v1/devices/{id}", delete(revoke_device))
        .route("/v1/token/rotate", post(rotate_token))
        .with_state(daemon)
}

async fn require_token(State(daemon): State<AppState>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    match daemon.token.check(presented) {
        Check::Valid => next.run(request).await,
        Check::Invalid => {
            ApiError(StatusCode::UNAUTHORIZED, "missing or wrong token".into()).into_response()
        }
        Check::Throttled => {
            tokio::time::sleep(Duration::from_secs(1)).await;
            ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "too many wrong tokens; slow down".into(),
            )
            .into_response()
        }
    }
}

/// Binds `127.0.0.1:7700` (failing if something already has it), then keeps
/// trying to add the Tailscale address in the background.
pub async fn serve(daemon: AppState, extra: Vec<SocketAddr>) -> anyhow::Result<()> {
    let local = SocketAddr::from(([127, 0, 0, 1], API_PORT));
    let listener = TcpListener::bind(local).await.with_context(|| {
        format!("could not listen on {local}. Is another hedgehog daemon running?")
    })?;
    let app = router(daemon);
    tokio::spawn(serve_extra(app.clone(), extra));
    info!(%local, "control API listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn serve_extra(app: Router, extra: Vec<SocketAddr>) {
    let mut pending = extra;
    if pending.is_empty() {
        // Tailscale may come up after us at boot; keep looking.
        loop {
            if let Some(ip) = tailscale_ip().await {
                pending.push(SocketAddr::new(ip, API_PORT));
                break;
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
    for addr in pending {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                match TcpListener::bind(addr).await {
                    Ok(listener) => {
                        info!(%addr, "control API listening");
                        let _ = axum::serve(listener, app).await;
                        return;
                    }
                    Err(error) => {
                        warn!(%addr, %error, "could not listen yet; retrying");
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                }
            }
        });
    }
}

pub async fn tailscale_ip() -> Option<IpAddr> {
    for program in [
        "tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ] {
        let Ok(output) = tokio::process::Command::new(program)
            .args(["ip", "-4"])
            .output()
            .await
        else {
            continue;
        };
        if output.status.success() {
            return String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()?
                .trim()
                .parse()
                .ok();
        }
    }
    None
}

async fn status(State(daemon): State<AppState>) -> ApiResult<Json<Status>> {
    Ok(Json(Status {
        version: VERSION.to_string(),
        tunnel: daemon.tunnel.info(),
        home_url: Some(daemon.gateway.home_url()),
        apps: daemon.store.apps()?.len(),
        docker: daemon.docker.ping().await,
        railpack: build::railpack_installed().await,
        apps_dir: daemon.paths.apps.display().to_string(),
    }))
}

fn app_info(daemon: &Daemon, app: AppRecord) -> AppInfo {
    let manifest = Manifest::load(&app.source_dir).unwrap_or_default();
    AppInfo {
        url: daemon.gateway.app_url(&app.name),
        public: app.public.unwrap_or(manifest.public()),
        source_dir: app.source_dir.display().to_string(),
        git: app.source_dir.join(".git").exists(),
        name: app.name,
        status: app.status,
        builder: app.builder,
        revision: app.revision,
        deployed_at: app.deployed_at,
        last_error: app.last_error,
    }
}

async fn list_apps(State(daemon): State<AppState>) -> ApiResult<Json<Vec<AppInfo>>> {
    let apps = daemon.store.apps()?;
    Ok(Json(
        apps.into_iter().map(|app| app_info(&daemon, app)).collect(),
    ))
}

/// Turns deploy events into a streaming `application/x-ndjson` response.
fn event_stream(rx: mpsc::UnboundedReceiver<DeployEvent>) -> Response {
    let lines = UnboundedReceiverStream::new(rx).map(|event| {
        let line = serde_json::to_string(&event).unwrap_or_default() + "\n";
        Ok::<_, Infallible>(line)
    });
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(lines),
    )
        .into_response()
}

/// Deploys an app. With a `.tar.gz` body, that snapshot replaces the app's
/// folder first (this is how `hedgehog deploy` from a laptop works); with an
/// empty body, it rebuilds the folder that's already there.
async fn deploy(
    State(daemon): State<AppState>,
    Path(name): Path<String>,
    body: Body,
) -> ApiResult<Response> {
    if !is_valid_app_name(&name) {
        return Err(bad_request(format!(
            "`{name}` isn't a valid app name: use lowercase letters, digits, and dashes"
        )));
    }
    let upload = save_upload(&daemon, body).await?;
    if upload.is_none() && daemon.store.app(&name)?.is_none() {
        return Err(not_found(&name));
    }
    let (events, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let source = match &upload {
            Some(path) => Source::Upload(path.clone()),
            None => Source::Folder,
        };
        let _ = daemon.deploy(&name, source, events).await;
        if let Some(path) = upload {
            let _ = std::fs::remove_file(path);
        }
    });
    Ok(event_stream(rx))
}

/// Streams a request body to `apps/.incoming`; `None` if it was empty.
async fn save_upload(daemon: &Daemon, body: Body) -> ApiResult<Option<std::path::PathBuf>> {
    let incoming = daemon.paths.incoming();
    tokio::fs::create_dir_all(&incoming)
        .await
        .context("could not create the upload folder")?;
    let path = incoming.join(format!("upload-{}.tar.gz", random_token(6)));
    let mut file = tokio::fs::File::create(&path)
        .await
        .context("could not save the upload")?;
    let mut stream = body.into_data_stream();
    let mut size = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| bad_request(format!("upload interrupted: {error}")))?;
        size += chunk.len();
        file.write_all(&chunk)
            .await
            .context("could not save the upload")?;
    }
    file.flush().await.context("could not save the upload")?;
    if size == 0 {
        let _ = tokio::fs::remove_file(&path).await;
        return Ok(None);
    }
    Ok(Some(path))
}

async fn restart(State(daemon): State<AppState>, Path(name): Path<String>) -> ApiResult<Response> {
    record(&daemon, &name)?;
    let (events, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _ = daemon.restart(&name, events).await;
    });
    Ok(event_stream(rx))
}

/// Creates an app by cloning a Git repository on this Mac, then deploys it.
async fn create_app(
    State(daemon): State<AppState>,
    Json(request): Json<CreateApp>,
) -> ApiResult<Response> {
    let name = request.name.trim().to_string();
    let url = request.git_url.trim().to_string();
    if !is_valid_app_name(&name) {
        return Err(bad_request(format!(
            "`{name}` isn't a valid app name: use lowercase letters, digits, and dashes"
        )));
    }
    crate::git::validate_url(&url).map_err(bad_request)?;
    let branch = request
        .branch
        .map(|branch| branch.trim().to_string())
        .filter(|branch| !branch.is_empty());
    if let Some(branch) = &branch {
        crate::git::validate_branch(branch).map_err(bad_request)?;
    }
    if daemon.store.app(&name)?.is_some() || daemon.paths.app_source(&name).exists() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("there's already an app (or a folder) called {name}"),
        ));
    }
    let (events, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _ = daemon
            .deploy(&name, Source::Clone { url, branch }, events)
            .await;
    });
    Ok(event_stream(rx))
}

async fn webhook_info(daemon: &Daemon, app: &AppRecord, secret: String) -> ApiResult<WebhookInfo> {
    let branch = crate::git::current_branch(&app.source_dir).await;
    if branch.is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!(
                "{} isn't a git clone on a branch, so there's nothing to deploy on push",
                app.name
            ),
        ));
    }
    Ok(WebhookInfo {
        url: crate::hooks::url(daemon, &app.name),
        secret,
        branch,
    })
}

/// The app's push-webhook URL and secret (created on first request).
async fn webhook(
    State(daemon): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<WebhookInfo>> {
    let app = record(&daemon, &name)?;
    let secret = crate::hooks::secret(&daemon, &name)?;
    Ok(Json(webhook_info(&daemon, &app, secret).await?))
}

async fn rotate_webhook(
    State(daemon): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<WebhookInfo>> {
    let app = record(&daemon, &name)?;
    let secret = crate::hooks::rotate_secret(&daemon, &name)?;
    Ok(Json(webhook_info(&daemon, &app, secret).await?))
}

/// `git pull` in the app's folder, then deploy. For apps that are git clones.
async fn pull(State(daemon): State<AppState>, Path(name): Path<String>) -> ApiResult<Response> {
    let app = record(&daemon, &name)?;
    if !app.source_dir.join(".git").exists() {
        return Err(bad_request(format!(
            "{name} isn't a git repo, so there's nothing to pull. Deploy it with `hedgehog deploy`"
        )));
    }
    let (events, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _ = daemon.deploy(&name, Source::Pull, events).await;
    });
    Ok(event_stream(rx))
}

async fn stop(State(daemon): State<AppState>, Path(name): Path<String>) -> ApiResult<StatusCode> {
    record(&daemon, &name)?;
    daemon.stop_app(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RemoveQuery {
    #[serde(default)]
    purge: bool,
}

async fn remove(
    State(daemon): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<RemoveQuery>,
) -> ApiResult<StatusCode> {
    record(&daemon, &name)?;
    daemon.remove_app(&name, query.purge).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct LogsQuery {
    #[serde(default)]
    follow: bool,
    #[serde(default)]
    build: bool,
    tail: Option<usize>,
}

async fn logs(
    State(daemon): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<LogsQuery>,
) -> ApiResult<Response> {
    let app = record(&daemon, &name)?;
    if query.build {
        let path = daemon.paths.logs().join(&name).join("build.log");
        let text = tokio::fs::read_to_string(&path).await.map_err(|_| {
            ApiError(
                StatusCode::NOT_FOUND,
                format!("{name} has no build log yet"),
            )
        })?;
        return Ok(text.into_response());
    }
    let Some(container) = app.container else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("{name} isn't running yet. Try `hedgehog logs {name} --build`"),
        ));
    };
    let stream = daemon
        .docker
        .logs(&container, query.follow, query.tail.unwrap_or(200));
    Ok(Body::from_stream(stream).into_response())
}

async fn get_env(
    State(daemon): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<std::collections::BTreeMap<String, String>>> {
    record(&daemon, &name)?;
    Ok(Json(daemon.store.env(&name)?))
}

fn valid_env_key(key: &str) -> bool {
    !key.is_empty() && !key.contains(['=', '\0']) && !key.chars().any(char::is_whitespace)
}

/// Saves env changes and restarts the app so they take effect.
async fn update_env(
    State(daemon): State<AppState>,
    Path(name): Path<String>,
    Json(update): Json<EnvUpdate>,
) -> ApiResult<Response> {
    let app = record(&daemon, &name)?;
    if let Some(key) = update
        .set
        .keys()
        .chain(update.unset.iter())
        .find(|key| !valid_env_key(key))
    {
        return Err(bad_request(format!("`{key}` isn't a valid variable name")));
    }
    daemon.store.update_env(&name, &update.set, &update.unset)?;
    let restarting = app.container.is_some() && !app.stopped;
    if restarting {
        daemon.spawn_restart(name);
    }
    Ok(Json(serde_json::json!({ "restarting": restarting })).into_response())
}

async fn set_public(
    State(daemon): State<AppState>,
    Path(name): Path<String>,
    Json(update): Json<PublicUpdate>,
) -> ApiResult<StatusCode> {
    record(&daemon, &name)?;
    daemon.store.set_public(&name, Some(update.public))?;
    if let Some(mut upstream) = daemon.gateway.upstream(&name) {
        upstream.public = update.public;
        daemon.gateway.set_app(&name, upstream).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn pair(
    State(daemon): State<AppState>,
    Json(request): Json<PairRequest>,
) -> ApiResult<Json<PairResponse>> {
    let code = random_token(16);
    let grant = PairGrant {
        label: request
            .label
            .as_deref()
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(str::to_string),
        admin: request.admin,
    };
    daemon
        .store
        .add_pair_code(&hash_token(&code), &grant, now() + PAIR_CODE_TTL as i64)?;
    let url = format!("{}{PAIR_PATH}?code={code}", daemon.gateway.home_url());
    Ok(Json(PairResponse {
        qr_svg: qr_svg(&url),
        url,
        expires_in_secs: PAIR_CODE_TTL,
    }))
}

fn qr_svg(text: &str) -> Option<String> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    Some(
        code.render::<qrcode::render::svg::Color<'_>>()
            .min_dimensions(220, 220)
            .quiet_zone(true)
            .build(),
    )
}

/// Issues a new API token; the old one stops working right away.
async fn rotate_token(State(daemon): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let token = daemon.token.rotate()?;
    info!("API token rotated");
    Ok(Json(serde_json::json!({ "token": token })))
}

async fn devices(
    State(daemon): State<AppState>,
    current: Option<Extension<CurrentDevice>>,
) -> ApiResult<Json<Vec<hedgehog_core::api::DeviceInfo>>> {
    let mut devices = daemon.store.devices()?;
    if let Some(Extension(CurrentDevice(id))) = current {
        for device in &mut devices {
            device.current = device.id == id;
        }
    }
    Ok(Json(devices))
}

async fn revoke_device(
    State(daemon): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    if !daemon.store.remove_device(&id)? {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no device with id {id}"),
        ));
    }
    daemon.gateway.reload_devices()?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_env_keys() {
        assert!(valid_env_key("DATABASE_URL"));
        assert!(!valid_env_key(""));
        assert!(!valid_env_key("A=B"));
        assert!(!valid_env_key("A B"));
    }
}
