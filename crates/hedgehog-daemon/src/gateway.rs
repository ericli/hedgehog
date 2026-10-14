//! The gateway sits between OpenTunnel and the app containers. Each app gets
//! its own local listener, so the tunnel route alone decides which app a
//! request reaches (the `Host` header is never trusted for routing). It checks
//! that the visitor is one of your paired devices, then proxies to the app.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::pages;
use crate::secrets::{hash_token, random_token};
use crate::store::Store;

pub const COOKIE: &str = "hedgehog_device";
pub const PAIR_PATH: &str = "/_hedgehog/pair";
const COOKIE_MAX_AGE: u64 = 400 * 24 * 60 * 60;

type Body = BoxBody<Bytes, hyper::Error>;

/// Where an app's traffic should go right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Upstream {
    /// Host port of the running container, `None` while it isn't serving.
    pub port: Option<u16>,
    pub public: bool,
    /// Shown to visitors when there is no port.
    pub reason: String,
}

#[derive(Debug, Clone)]
enum Target {
    Home,
    App(String),
}

struct AppListener {
    port: u16,
    task: JoinHandle<()>,
}

struct Inner {
    store: Arc<Store>,
    upstreams: RwLock<HashMap<String, Upstream>>,
    listeners: RwLock<HashMap<String, AppListener>>,
    devices: RwLock<HashSet<String>>,
    /// Tunnel hostname (`<id>.opentunnel.xyz`) once the tunnel is up.
    hostname: RwLock<Option<String>>,
    home_port: u16,
    client: Client<HttpConnector, Incoming>,
}

#[derive(Clone)]
pub struct Gateway {
    inner: Arc<Inner>,
}

impl Gateway {
    /// Loads paired devices and starts the home listener.
    pub async fn start(store: Arc<Store>) -> Result<Self> {
        let home = TcpListener::bind("127.0.0.1:0").await?;
        let home_port = home.local_addr()?.port();
        let devices = store.device_token_hashes()?.into_iter().collect();
        let mut connector = HttpConnector::new();
        connector.set_nodelay(true);
        let inner = Arc::new(Inner {
            store,
            upstreams: RwLock::default(),
            listeners: RwLock::default(),
            devices: RwLock::new(devices),
            hostname: RwLock::default(),
            home_port,
            client: Client::builder(TokioExecutor::new()).build(connector),
        });
        tokio::spawn(serve(home, inner.clone(), Target::Home));
        Ok(Self { inner })
    }

    pub fn home_port(&self) -> u16 {
        self.inner.home_port
    }

    pub fn set_hostname(&self, hostname: Option<String>) {
        *self.inner.hostname.write().unwrap() = hostname;
    }

    pub fn hostname(&self) -> Option<String> {
        self.inner.hostname.read().unwrap().clone()
    }

    /// Points an app at an upstream, opening its listener the first time.
    pub async fn set_app(&self, app: &str, upstream: Upstream) -> Result<u16> {
        self.inner
            .upstreams
            .write()
            .unwrap()
            .insert(app.to_string(), upstream);
        if let Some(listener) = self.inner.listeners.read().unwrap().get(app) {
            return Ok(listener.port);
        }
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let task = tokio::spawn(serve(
            listener,
            self.inner.clone(),
            Target::App(app.to_string()),
        ));
        let mut listeners = self.inner.listeners.write().unwrap();
        // Another caller may have raced us; keep theirs.
        if let Some(existing) = listeners.get(app) {
            task.abort();
            return Ok(existing.port);
        }
        listeners.insert(app.to_string(), AppListener { port, task });
        Ok(port)
    }

    pub fn upstream(&self, app: &str) -> Option<Upstream> {
        self.inner.upstreams.read().unwrap().get(app).cloned()
    }

    pub fn remove_app(&self, app: &str) {
        self.inner.upstreams.write().unwrap().remove(app);
        if let Some(listener) = self.inner.listeners.write().unwrap().remove(app) {
            listener.task.abort();
        }
    }

    pub fn apps(&self) -> Vec<String> {
        self.inner
            .listeners
            .read()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    /// App name to local gateway port, for building tunnel routes.
    pub fn app_ports(&self) -> BTreeMap<String, u16> {
        self.inner
            .listeners
            .read()
            .unwrap()
            .iter()
            .map(|(app, listener)| (app.clone(), listener.port))
            .collect()
    }

    pub fn reload_devices(&self) -> Result<()> {
        let devices = self
            .inner
            .store
            .device_token_hashes()?
            .into_iter()
            .collect();
        *self.inner.devices.write().unwrap() = devices;
        Ok(())
    }

    pub fn app_url(&self, app: &str) -> Option<String> {
        self.inner.app_url(app)
    }

    pub fn home_url(&self) -> String {
        self.inner.home_url()
    }
}

impl Inner {
    fn app_url(&self, app: &str) -> Option<String> {
        match self.hostname.read().unwrap().as_deref() {
            Some(hostname) => Some(format!("https://{app}.{hostname}")),
            None => {
                let port = self.listeners.read().unwrap().get(app)?.port;
                Some(format!("http://127.0.0.1:{port}"))
            }
        }
    }

    fn home_url(&self) -> String {
        match self.hostname.read().unwrap().as_deref() {
            Some(hostname) => format!("https://{hostname}"),
            None => format!("http://127.0.0.1:{}", self.home_port),
        }
    }

    fn is_paired(&self, headers: &HeaderMap) -> bool {
        let devices = self.devices.read().unwrap();
        cookie_values(headers, COOKIE).any(|token| devices.contains(&hash_token(token)))
    }

    /// The cookie covers every app (`Domain=<id>.opentunnel.xyz`) when
    /// tunneled; locally, cookies ignore ports, so `127.0.0.1` covers them all.
    fn device_cookie(&self, token: &str) -> String {
        match self.hostname.read().unwrap().as_deref() {
            Some(hostname) => format!(
                "{COOKIE}={token}; Domain={hostname}; Path=/; Max-Age={COOKIE_MAX_AGE}; HttpOnly; Secure; SameSite=Lax"
            ),
            None => format!(
                "{COOKIE}={token}; Path=/; Max-Age={COOKIE_MAX_AGE}; HttpOnly; SameSite=Lax"
            ),
        }
    }

    fn is_tunneled(&self, headers: &HeaderMap) -> bool {
        let Some(hostname) = self.hostname.read().unwrap().clone() else {
            return false;
        };
        headers
            .get(header::HOST)
            .and_then(|host| host.to_str().ok())
            .map(|host| host.split(':').next().unwrap_or(host).to_ascii_lowercase())
            .is_some_and(|host| host == hostname || host.ends_with(&format!(".{hostname}")))
    }
}

async fn serve(listener: TcpListener, inner: Arc<Inner>, target: Target) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                warn!(%error, "gateway accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let (inner, target) = (inner.clone(), target.clone());
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let (inner, target) = (inner.clone(), target.clone());
                async move { Ok::<_, Infallible>(handle(&inner, &target, request).await) }
            });
            if let Err(error) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await
            {
                debug!(%error, "gateway connection ended");
            }
        });
    }
}

async fn handle(inner: &Inner, target: &Target, request: Request<Incoming>) -> Response<Body> {
    match target {
        Target::Home => handle_home(inner, request),
        Target::App(app) => handle_app(inner, app, request).await,
    }
}

fn handle_home(inner: &Inner, request: Request<Incoming>) -> Response<Body> {
    if request.uri().path() == PAIR_PATH {
        return pair(inner, &request);
    }
    if request.uri().path() != "/" {
        return html(StatusCode::NOT_FOUND, pages::wall());
    }
    if !inner.is_paired(request.headers()) {
        return html(StatusCode::UNAUTHORIZED, pages::wall());
    }
    let mut apps: Vec<(String, String, &'static str)> = inner
        .upstreams
        .read()
        .unwrap()
        .iter()
        .filter_map(|(app, upstream)| {
            let status = match (upstream.port.is_some(), upstream.public) {
                (false, _) => "not running",
                (true, true) => "public",
                (true, false) => "private",
            };
            Some((app.clone(), inner.app_url(app)?, status))
        })
        .collect();
    apps.sort();
    html(StatusCode::OK, pages::home(&apps))
}

/// Trades a one-time pairing code for a long-lived device cookie.
fn pair(inner: &Inner, request: &Request<Incoming>) -> Response<Body> {
    let code = request
        .uri()
        .query()
        .and_then(|query| query.split('&').find_map(|pair| pair.strip_prefix("code=")));
    let label = match code.map(|code| inner.store.take_pair_code(&hash_token(code))) {
        Some(Ok(Some(label))) => label,
        Some(Err(error)) => {
            warn!(%error, "pairing lookup failed");
            return html(StatusCode::INTERNAL_SERVER_ERROR, pages::pair_failed());
        }
        _ => return html(StatusCode::FORBIDDEN, pages::pair_failed()),
    };
    let token = random_token(32);
    let id = random_token(4);
    let token_hash = hash_token(&token);
    if let Err(error) = inner.store.add_device(&id, label.as_deref(), &token_hash) {
        warn!(%error, "could not save paired device");
        return html(StatusCode::INTERNAL_SERVER_ERROR, pages::pair_failed());
    }
    inner.devices.write().unwrap().insert(token_hash);
    let mut response = Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, "/")
        .header(header::CACHE_CONTROL, "no-store");
    if let Ok(cookie) = HeaderValue::from_str(&inner.device_cookie(&token)) {
        response = response.header(header::SET_COOKIE, cookie);
    }
    response.body(empty()).unwrap()
}

async fn handle_app(inner: &Inner, app: &str, request: Request<Incoming>) -> Response<Body> {
    let Some(upstream) = inner.upstreams.read().unwrap().get(app).cloned() else {
        return html(
            StatusCode::NOT_FOUND,
            pages::unavailable(app, "This app no longer exists."),
        );
    };
    if !upstream.public && !inner.is_paired(request.headers()) {
        return html(StatusCode::UNAUTHORIZED, pages::wall());
    }
    let Some(port) = upstream.port else {
        return html(
            StatusCode::SERVICE_UNAVAILABLE,
            pages::unavailable(app, &upstream.reason),
        );
    };
    let tunneled = inner.is_tunneled(request.headers());
    match proxy(inner, port, tunneled, request).await {
        Ok(response) => response,
        Err(error) => {
            debug!(%error, app, "upstream request failed");
            html(
                StatusCode::BAD_GATEWAY,
                pages::unavailable(app, "The app didn't answer. It may be restarting."),
            )
        }
    }
}

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

async fn proxy(
    inner: &Inner,
    port: u16,
    tunneled: bool,
    mut request: Request<Incoming>,
) -> Result<Response<Body>> {
    let upgrade = request.headers().get(header::UPGRADE).cloned();
    let client_upgrade = upgrade.is_some().then(|| hyper::upgrade::on(&mut request));

    let path = request
        .uri()
        .path_and_query()
        .map(|path| path.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    *request.uri_mut() = format!("http://127.0.0.1:{port}{path}").parse()?;
    prepare_headers(request.headers_mut(), tunneled);
    if let Some(upgrade) = upgrade {
        request
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        request.headers_mut().insert(header::UPGRADE, upgrade);
    }

    let Some(client_upgrade) = client_upgrade else {
        let mut response = inner.client.request(request).await?;
        strip_hop_by_hop(response.headers_mut());
        return Ok(response.map(|body| body.boxed()));
    };

    // Upgrades (WebSockets) get their own connection so both sides can be
    // spliced together once the app agrees to switch protocols.
    let stream = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let mut response = sender.send_request(request).await?;
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let upstream_upgrade = hyper::upgrade::on(&mut response);
        tokio::spawn(async move {
            if let (Ok(client), Ok(upstream)) = (client_upgrade.await, upstream_upgrade.await) {
                let _ = tokio::io::copy_bidirectional(
                    &mut TokioIo::new(client),
                    &mut TokioIo::new(upstream),
                )
                .await;
            }
        });
    }
    Ok(response.map(|body| body.boxed()))
}

/// Drops hop-by-hop and spoofable forwarding headers, removes hedgehog's own
/// cookie so apps never see it, and says how the visitor really connected.
fn prepare_headers(headers: &mut HeaderMap, tunneled: bool) {
    strip_hop_by_hop(headers);
    for name in [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-proto",
        "x-forwarded-host",
        "x-real-ip",
    ] {
        headers.remove(name);
    }
    let cookies: Vec<String> = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .map(str::trim)
        .filter(|pair| !pair.is_empty() && pair.split('=').next() != Some(COOKIE))
        .map(str::to_string)
        .collect();
    headers.remove(header::COOKIE);
    if !cookies.is_empty()
        && let Ok(value) = HeaderValue::from_str(&cookies.join("; "))
    {
        headers.insert(header::COOKIE, value);
    }
    let proto = if tunneled { "https" } else { "http" };
    headers.insert("x-forwarded-proto", HeaderValue::from_static(proto));
    if let Some(host) = headers.get(header::HOST).cloned() {
        headers.insert("x-forwarded-host", host);
    }
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<String> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .collect();
    for name in HOP_BY_HOP
        .iter()
        .copied()
        .chain(listed.iter().map(String::as_str))
    {
        headers.remove(name);
    }
}

fn cookie_values<'a>(headers: &'a HeaderMap, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(move |pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then_some(value)
        })
}

fn empty() -> Body {
    Full::new(Bytes::new())
        .map_err(|never| match never {})
        .boxed()
}

fn html(status: StatusCode, body: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(
            Full::new(Bytes::from(body))
                .map_err(|never| match never {})
                .boxed(),
        )
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_hedgehog_cookie_and_forwarding_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::HOST,
            HeaderValue::from_static("budget.abc.opentunnel.xyz"),
        );
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; hedgehog_device=secret; b=2"),
        );
        headers.insert("x-forwarded-for", HeaderValue::from_static("6.6.6.6"));
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, x-custom"),
        );
        headers.insert("x-custom", HeaderValue::from_static("drop me"));
        prepare_headers(&mut headers, true);
        assert_eq!(headers[header::COOKIE], "a=1; b=2");
        assert!(!headers.contains_key("x-forwarded-for"));
        assert!(!headers.contains_key("x-custom"));
        assert!(!headers.contains_key(header::CONNECTION));
        assert_eq!(headers["x-forwarded-proto"], "https");
        assert_eq!(headers["x-forwarded-host"], "budget.abc.opentunnel.xyz");
    }

    #[test]
    fn removes_cookie_header_when_only_hedgehog_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("hedgehog_device=secret"),
        );
        prepare_headers(&mut headers, false);
        assert!(!headers.contains_key(header::COOKIE));
        assert_eq!(headers["x-forwarded-proto"], "http");
    }

    /// Upstream that answers with the cookie and forwarded-proto it received.
    async fn echo_upstream() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let service = service_fn(|request: Request<Incoming>| async move {
                        let header = |name| {
                            request
                                .headers()
                                .get(name)
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or("-")
                                .to_string()
                        };
                        let body = format!(
                            "cookie={} proto={}",
                            header("cookie"),
                            header("x-forwarded-proto")
                        );
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(body))))
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        port
    }

    async fn get(port: u16, path: &str, cookie: Option<&str>) -> (StatusCode, HeaderMap, String) {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(connection);
        let mut request = Request::builder()
            .uri(path)
            .header(header::HOST, "localhost");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        let response = sender
            .send_request(request.body(Full::new(Bytes::new())).unwrap())
            .await
            .unwrap();
        let (parts, body) = response.into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        (
            parts.status,
            parts.headers,
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    #[tokio::test]
    async fn pairs_devices_and_proxies_only_for_them() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let gateway = Gateway::start(store.clone()).await.unwrap();
        let upstream = echo_upstream().await;
        let app_port = gateway
            .set_app(
                "budget",
                Upstream {
                    port: Some(upstream),
                    public: false,
                    reason: String::new(),
                },
            )
            .await
            .unwrap();

        let (status, _, _) = get(app_port, "/", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, _) = get(app_port, "/", Some("hedgehog_device=forged")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        store
            .add_pair_code(
                &hash_token("code123"),
                Some("phone"),
                crate::secrets::now() + 60,
            )
            .unwrap();
        let (status, headers, _) =
            get(gateway.home_port(), "/_hedgehog/pair?code=code123", None).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let set_cookie = headers[header::SET_COOKIE].to_str().unwrap().to_string();
        let device = set_cookie.split(';').next().unwrap().to_string();
        assert!(device.starts_with("hedgehog_device="));
        let (status, _, _) = get(gateway.home_port(), "/_hedgehog/pair?code=code123", None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "codes are single use");

        let cookies = format!("theme=dark; {device}");
        let (status, _, body) = get(app_port, "/", Some(&cookies)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body, "cookie=theme=dark proto=http",
            "app must not see hedgehog's cookie"
        );

        let (status, _, body) = get(gateway.home_port(), "/", Some(&device)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("budget"));

        gateway
            .set_app(
                "budget",
                Upstream {
                    port: Some(upstream),
                    public: true,
                    reason: String::new(),
                },
            )
            .await
            .unwrap();
        let (status, _, _) = get(app_port, "/", None).await;
        assert_eq!(status, StatusCode::OK, "public apps skip the device check");

        gateway
            .set_app(
                "budget",
                Upstream {
                    port: None,
                    public: true,
                    reason: "stopped".into(),
                },
            )
            .await
            .unwrap();
        let (status, _, _) = get(app_port, "/", None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        gateway.remove_app("budget");
        assert!(gateway.app_ports().is_empty());
    }

    #[test]
    fn reads_cookie_values() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("x=1; hedgehog_device=one"),
        );
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("hedgehog_device=two"),
        );
        let values: Vec<_> = cookie_values(&headers, COOKIE).collect();
        assert_eq!(values, vec!["one", "two"]);
    }
}
