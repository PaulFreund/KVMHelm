//! HTTP entry point for the shared Core. Accepted work is owned independently
//! of a socket; body streams own bounded subscriptions and connection monitors.
mod audio;
use crate::{
    GatewayError, Result,
    auth::{Identity, bearer_client},
    contracts::Profile,
    core::Core,
    store::Kind,
};
use bytes::Bytes;
use futures_util::stream;
use http_body_util::{BodyExt, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Method, Request, Response,
    body::{Frame, Incoming},
    header::{HeaderMap, HeaderValue},
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    convert::Infallible,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Body = UnsyncBoxBody<Bytes, BoxError>;
pub type Assets = &'static [(&'static str, &'static [u8])];
const BODY_LIMIT: usize = 256 * 1024;

/// Match the existing express-rate-limit default: individual IPv4 addresses,
/// including IPv4-mapped peers, and a shared /56 window for native IPv6 peers.
pub fn rate_limit_key(address: IpAddr) -> String {
    match address {
        IpAddr::V4(address) => address.to_string(),
        IpAddr::V6(address) => {
            if let Some(address) = address.to_ipv4_mapped() {
                return address.to_string();
            }
            let mut octets = address.octets();
            octets[7..].fill(0);
            format!("{}/56", Ipv6Addr::from(octets))
        }
    }
}

#[derive(Clone)]
pub struct Options {
    pub host: String,
    pub port: u16,
    pub certificate: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub origins: Vec<String>,
    pub dev: bool,
    pub media_per_minute: u32,
    pub control_per_minute: u32,
    pub mcp_sessions: usize,
    pub mcp_sessions_per_token: usize,
    pub mcp_idle_timeout: Duration,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8765,
            certificate: None,
            key: None,
            origins: Vec::new(),
            dev: false,
            media_per_minute: 12000,
            control_per_minute: 2400,
            mcp_sessions: 128,
            mcp_sessions_per_token: 32,
            mcp_idle_timeout: Duration::from_secs(3600),
        }
    }
}
struct Window {
    start: Instant,
    count: u32,
}
#[derive(Default)]
struct Budgets {
    entries: Mutex<HashMap<(String, String), Window>>,
}
impl Budgets {
    fn take(&self, kind: &str, key: &str, limit: u32) -> std::result::Result<(), u64> {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|_, w| now.duration_since(w.start) < Duration::from_secs(60));
        if entries.len() >= 8192 && !entries.contains_key(&(kind.into(), key.into())) {
            return Err(60);
        }
        let window = entries.entry((kind.into(), key.into())).or_insert(Window {
            start: now,
            count: 0,
        });
        if window.count >= limit {
            return Err(60 - now.duration_since(window.start).as_secs());
        }
        window.count += 1;
        Ok(())
    }
}
#[derive(Clone)]
pub struct Connection {
    pub peer: SocketAddr,
    pub cancellation: CancellationToken,
    _socket: Arc<OwnedSemaphorePermit>,
}
struct State {
    core: Arc<Core>,
    mcp: crate::mcp_http::Endpoint,
    options: Options,
    origins: HashSet<String>,
    authorities: HashSet<String>,
    secure: bool,
    assets: Assets,
    budgets: Budgets,
    work: Arc<Semaphore>,
    safety: Arc<Semaphore>,
    streams: Arc<Semaphore>,
    token_streams: Mutex<HashMap<String, usize>>,
    tasks: TaskTracker,
    cancel: CancellationToken,
    audio: audio::Tickets,
}
pub struct Server {
    pub address: SocketAddr,
    pub url: String,
    state: Arc<State>,
    worker: Option<JoinHandle<()>>,
}
impl Server {
    pub async fn bind(core: Arc<Core>, options: Options, assets: Assets) -> Result<Self> {
        let loopback = matches!(options.host.as_str(), "127.0.0.1" | "::1" | "localhost");
        if options.certificate.is_some() != options.key.is_some()
            || (!loopback && options.certificate.is_none())
        {
            return Err(GatewayError::message(
                "INVALID_ARGUMENT",
                "LAN access requires an explicit TLS certificate and key",
            ));
        }
        let tls = if let (Some(cert), Some(key)) = (&options.certificate, &options.key) {
            let cert =
                std::fs::read(cert).map_err(|_| GatewayError::new("TLS_CERTIFICATE_ERROR"))?;
            let key = zeroize::Zeroizing::new(
                std::fs::read(key).map_err(|_| GatewayError::new("TLS_CERTIFICATE_ERROR"))?,
            );
            let certs = rustls_pemfile::certs(&mut cert.as_slice())
                .collect::<std::io::Result<Vec<_>>>()
                .map_err(|_| GatewayError::new("TLS_CERTIFICATE_ERROR"))?;
            let key = rustls_pemfile::private_key(&mut key.as_slice())
                .map_err(|_| GatewayError::new("TLS_CERTIFICATE_ERROR"))?
                .ok_or_else(|| GatewayError::new("TLS_CERTIFICATE_ERROR"))?;
            let config = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|_| GatewayError::new("TLS_CERTIFICATE_ERROR"))?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|_| GatewayError::new("TLS_CERTIFICATE_ERROR"))?;
            Some(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
        } else {
            None
        };
        let listener = TcpListener::bind((options.host.as_str(), options.port))
            .await
            .map_err(|_| GatewayError::new("LISTEN_ERROR"))?;
        let address = listener
            .local_addr()
            .map_err(|_| GatewayError::new("LISTEN_ERROR"))?;
        let scheme = if tls.is_some() { "https" } else { "http" };
        let host = if options.host.contains(':') {
            format!("[{}]", options.host)
        } else {
            options.host.clone()
        };
        let server_url = url::Url::parse(&format!("{scheme}://{host}:{}", address.port()))
            .map_err(|_| GatewayError::invalid())?
            .origin()
            .ascii_serialization();
        let mut origins = HashSet::from([server_url.clone()]);
        origins.extend(options.origins.iter().cloned());
        if loopback {
            origins.insert(
                url::Url::parse(&format!("{scheme}://localhost:{}", address.port()))
                    .map_err(|_| GatewayError::invalid())?
                    .origin()
                    .ascii_serialization(),
            );
            if options.dev {
                origins.insert("http://127.0.0.1:5173".into());
            }
        }
        let mut authorities = HashSet::new();
        for origin in &origins {
            let url = url::Url::parse(origin).map_err(|_| GatewayError::invalid())?;
            if !matches!(url.scheme(), "http" | "https")
                || url.origin().ascii_serialization() != *origin
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(GatewayError::message(
                    "INVALID_ARGUMENT",
                    "Use a canonical HTTP(S) origin",
                ));
            }
            authorities.insert(url[url::Position::BeforeHost..url::Position::AfterPort].to_owned());
        }
        let mcp = crate::mcp_http::Endpoint::new(
            core.clone(),
            authorities.iter().cloned().collect(),
            origins.iter().cloned().collect(),
            (options.mcp_sessions, options.mcp_sessions_per_token),
            options.mcp_idle_timeout,
        );
        let state = Arc::new(State {
            core,
            mcp,
            options,
            origins,
            authorities,
            secure: tls.is_some(),
            assets,
            budgets: Budgets::default(),
            work: Arc::new(Semaphore::new(64)),
            safety: Arc::new(Semaphore::new(16)),
            streams: Arc::new(Semaphore::new(512)),
            token_streams: Mutex::new(HashMap::new()),
            tasks: TaskTracker::new(),
            cancel: CancellationToken::new(),
            audio: audio::Tickets::default(),
        });
        let endpoint = state.mcp.clone();
        let endpoint_stop = state.cancel.clone();
        state.tasks.spawn(async move {
            loop {
                tokio::select! {biased;_=endpoint_stop.cancelled()=>break,_=tokio::time::sleep(Duration::from_secs(1))=>()};
                endpoint.sweep().await;
            }
            endpoint.close().await;
        });
        let app = state.clone();
        let worker = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            let slots = Arc::new(Semaphore::new(512));
            loop {
                tokio::select! {biased;_=app.cancel.cancelled()=>break,
                    // Reap before accepting more sockets: a continuously ready
                    // accept queue must not retain completed connection tasks.
                    _=connections.join_next(),if !connections.is_empty()=>(),
                    accepted=listener.accept()=>if let Ok((tcp,peer))=accepted {
                        let Ok(slot)=slots.clone().try_acquire_owned() else {continue;};
                        let app=app.clone();let tls=tls.clone();
                        connections.spawn(async move {
                            let connection=Connection {peer,cancellation:app.cancel.child_token(),_socket:Arc::new(slot)};
                            if let Some(tls)=tls {
                                tokio::select! {biased;_=app.cancel.cancelled()=>(),stream=tokio::time::timeout(Duration::from_secs(5),tls.accept(tcp))=>if let Ok(Ok(stream))=stream {serve_connection(stream,app,connection).await;}}
                            } else {serve_connection(tcp,app,connection).await;}
                        });
                    },
                }
            }
            while connections.join_next().await.is_some() {}
        });
        Ok(Self {
            address,
            url: server_url,
            state,
            worker: Some(worker),
        })
    }
    pub fn stop(&self) {
        self.state.cancel.cancel();
    }
    pub async fn close(mut self) {
        self.state.cancel.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
        self.state.tasks.close();
        self.state.tasks.wait().await;
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.state.cancel.cancel();
    }
}
async fn serve_connection<T: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    io: T,
    state: Arc<State>,
    connection: Connection,
) {
    let cancel = connection.cancellation.clone();
    let close = cancel.clone().drop_guard();
    let service = service_fn(move |mut request| {
        request.extensions_mut().insert(connection.clone());
        let state = state.clone();
        async move { Ok::<_, Infallible>(entry(state, request).await) }
    });
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(10))
        .max_buf_size(32768);
    let stream = builder
        .serve_connection(TokioIo::new(io), service)
        .with_upgrades();
    tokio::select! {biased;_=cancel.cancelled()=>(),_=stream=>()};
    drop(close);
}
fn full(value: impl Into<Bytes>) -> Body {
    Full::new(value.into())
        .map_err(|never| match never {})
        .boxed_unsync()
}
fn json_response(status: u16, value: Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full(value.to_string()))
        .unwrap()
}
fn error_response(error: GatewayError) -> Response<Body> {
    let status = match error.code.as_str() {
        "AUTH_FAILED" => 401,
        "FORBIDDEN" => 403,
        "NOT_FOUND" => 404,
        "SESSION_LOST" => 410,
        "STORAGE_ERROR" => 503,
        "INTERNAL_ERROR" => 500,
        "MCP_CONNECTION_LOST" => 502,
        "REVISION_CONFLICT" => 409,
        "BODY_LIMIT" => 413,
        "RATE_LIMIT" => 429,
        _ => 400,
    };
    json_response(
        status,
        json!({"ok":false,"error":{"code":error.code,"message":error.message}}),
    )
}
fn rate_response(seconds: u64) -> Response<Body> {
    let mut response = error_response(GatewayError::message(
        "RATE_LIMIT",
        "Request budget exceeded; retry after the indicated delay",
    ));
    response.headers_mut().insert(
        "retry-after",
        HeaderValue::from_str(&seconds.max(1).to_string()).unwrap(),
    );
    response
}
pub(crate) fn header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(GatewayError::new("FORBIDDEN"));
    }
    value
        .to_str()
        .map(Some)
        .map_err(|_| GatewayError::new("FORBIDDEN"))
}
fn authorize(
    state: &State,
    headers: &HeaderMap,
    method: &Method,
) -> Result<(Identity, Option<String>)> {
    let client = header(headers, "x-kvmhelm-client")?.unwrap_or("default");
    if client.is_empty()
        || client.len() > 100
        || !client
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
    {
        return Err(GatewayError::new("FORBIDDEN"));
    }
    if let Some(secret) = header(headers, "authorization")?
        .and_then(|s| s.strip_prefix("Bearer "))
        .filter(|s| !s.is_empty())
    {
        return Ok((
            state
                .core
                .auth
                .authenticate(secret, &bearer_client(secret, client))?,
            None,
        ));
    }
    let cookies = header(headers, "cookie")?.unwrap_or("");
    let mut ids = cookies
        .split(';')
        .filter_map(|c| c.trim().strip_prefix("kvmhelm="));
    let id = ids.next().ok_or_else(|| GatewayError::new("AUTH_FAILED"))?;
    if ids.next().is_some() {
        return Err(GatewayError::new("FORBIDDEN"));
    }
    let (identity, csrf) = state.core.auth.cookie(id, client)?;
    if !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
        && (header(headers, "x-csrf-token")?.is_none_or(|value| {
            !state
                .core
                .auth
                .valid_csrf(&identity, value)
                .unwrap_or(false)
        }) || header(headers, "origin")?.is_none_or(|value| !state.origins.contains(value)))
    {
        return Err(GatewayError::message("FORBIDDEN", "CSRF validation failed"));
    }
    Ok((identity, Some(csrf)))
}
fn safety(path: &str, method: &Method, body: &Value) -> bool {
    if path == "/mcp" && *method == Method::DELETE {
        return true;
    }
    if *method != Method::POST {
        return false;
    }
    let parts = path.split('/').collect::<Vec<_>>();
    if matches!(
        parts.as_slice(),
        ["", "api", "v1", "devices", _, "stop"]
            | ["", "api", "v1", "sessions", _, "release-inputs"]
    ) {
        return true;
    }
    let (name, args) = if path == "/mcp" && body["method"] == "tools/call" {
        (&body["params"]["name"], &body["params"]["arguments"])
    } else if let Some(name) = path.strip_prefix("/api/v1/tools/") {
        return name == "close_computer"
            || name == "computer_control" && body["operation"] == "release";
    } else {
        return false;
    };
    name == "close_computer" || name == "computer_control" && args["operation"] == "release"
}
async fn read_json(mut body: Incoming) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut data = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| GatewayError::invalid())?;
            if let Some(bytes) = frame.data_ref() {
                if bytes.len() > BODY_LIMIT.saturating_sub(data.len()) {
                    return Err(GatewayError::new("BODY_LIMIT"));
                }
                data.extend_from_slice(bytes);
            }
        }
        if data.is_empty() {
            return Ok(json!({}));
        }
        let value: Value = serde_json::from_slice(&data).map_err(|_| GatewayError::invalid())?;
        if !value.is_object() && !value.is_array() {
            return Err(GatewayError::invalid());
        }
        Ok(value)
    })
    .await
    .unwrap_or_else(|_| Err(GatewayError::invalid()))
}
async fn entry(state: Arc<State>, request: Request<Incoming>) -> Response<Body> {
    let result = entry_checked(state.clone(), request).await;
    if let Err(error) = &result {
        state.core.record_error(&error.code);
    }
    let mut response = result.unwrap_or_else(error_response);
    for (name, value) in [
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cache-control", "no-store"),
        (
            "content-security-policy",
            "default-src 'self'; img-src 'self' blob: data:; media-src 'self' blob:; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'",
        ),
    ] {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}
async fn entry_checked(state: Arc<State>, request: Request<Incoming>) -> Result<Response<Body>> {
    let (mut parts, body) = request.into_parts();
    let path = parts.uri.path().to_owned();
    if header(&parts.headers, "host")?.is_none_or(|value| !state.authorities.contains(value))
        || header(&parts.headers, "origin")?.is_some_and(|value| !state.origins.contains(value))
    {
        return Err(GatewayError::new("FORBIDDEN"));
    }
    if path == "/healthz" && parts.method == Method::GET {
        return Ok(json_response(200, json!({"ok":true,"service":"KVMHelm"})));
    }
    if path == "/readyz" && parts.method == Method::GET {
        return Ok(if state.core.store.health().await.is_ok() {
            json_response(200, json!({"ok":true}))
        } else {
            json_response(503, json!({"ok":false,"error":{"code":"STORAGE_ERROR"}}))
        });
    }
    let peer = parts
        .extensions
        .get::<Connection>()
        .cloned()
        .ok_or_else(|| GatewayError::new("INTERNAL_ERROR"))?;
    if path == "/api/v1/audio" && parts.method == Method::GET {
        return audio::upgrade(state, &mut parts, peer);
    }
    let api = path == "/api" || path.starts_with("/api/") || path == "/mcp";
    if !api {
        return static_asset(&state, &path, &parts.method);
    }
    let login = path == "/api/v1/login" && parts.method == Method::POST;
    if login
        && let Err(seconds) = state
            .budgets
            .take("login", &rate_limit_key(peer.peer.ip()), 10)
    {
        return Ok(rate_response(seconds));
    }
    let auth = if login {
        None
    } else {
        match authorize(&state, &parts.headers, &parts.method) {
            Ok(auth) => Some(auth),
            Err(error) => {
                return Ok(
                    match state
                        .budgets
                        .take("auth", &rate_limit_key(peer.peer.ip()), 120)
                    {
                        Ok(()) => error_response(error),
                        Err(seconds) => rate_response(seconds),
                    },
                );
            }
        }
    };
    let body = read_json(body).await?;
    let safe = safety(&path, &parts.method, &body);
    if let Some((identity, _)) = &auth {
        let media = parts.method == Method::GET
            && matches!(
                path.split('/').collect::<Vec<_>>().as_slice(),
                ["", "api", "v1", "sessions", _, "frame" | "video"]
            );
        if !safe
            && let Err(seconds) = state.budgets.take(
                if media { "media" } else { "control" },
                &identity.token_id,
                if media {
                    state.options.media_per_minute
                } else {
                    state.options.control_per_minute
                },
            )
        {
            return Ok(rate_response(seconds));
        }
    }
    let slot = if safe {
        state.safety.clone()
    } else {
        state.work.clone()
    }
    .try_acquire_owned()
    .map_err(|_| GatewayError::new("RESOURCE_LIMIT"))?;
    if state.cancel.is_cancelled() {
        return Err(GatewayError::new("DEVICE_OFFLINE"));
    }
    let app = state.clone();
    let long_stream = parts.method == Method::GET
        && (path == "/mcp" || path == "/api/v1/events" || path.ends_with("/video"));
    let task = state.tasks.spawn(async move {
        let response = dispatch(app, parts, body, auth, peer).await?;
        if long_stream {
            return Ok(response);
        }
        // Keep admission through the last buffered output byte. Hyper can own
        // a data frame after dropping its Body, so the bytes share the permit.
        Ok(response.map(|body| admitted_body(body, slot)))
    });
    task.await
        .map_err(|_| GatewayError::new("INTERNAL_ERROR"))?
}
struct AdmittedBytes {
    data: Bytes,
    _slot: Arc<OwnedSemaphorePermit>,
}
fn admitted_body(body: Body, slot: OwnedSemaphorePermit) -> Body {
    let slot = Arc::new(slot);
    body.map_frame(move |frame| {
        frame.map_data(|data| {
            Bytes::from_owner(AdmittedBytes {
                data,
                _slot: slot.clone(),
            })
        })
    })
    .boxed_unsync()
}
impl AsRef<[u8]> for AdmittedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}
fn static_asset(state: &State, path: &str, method: &Method) -> Result<Response<Body>> {
    if !matches!(*method, Method::GET | Method::HEAD) {
        return Err(GatewayError::new("NOT_FOUND"));
    }
    let asset = state
        .assets
        .iter()
        .find(|(name, _)| *name == path)
        .or_else(|| state.assets.iter().find(|(name, _)| *name == "/index.html"))
        .ok_or_else(|| GatewayError::new("NOT_FOUND"))?;
    let mime = if asset.0.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else if asset.0.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if asset.0.ends_with(".svg") {
        "image/svg+xml"
    } else if asset.0.ends_with(".png") {
        "image/png"
    } else if asset.0.ends_with(".ico") {
        "image/x-icon"
    } else {
        "text/html; charset=utf-8"
    };
    Ok(Response::builder()
        .header("content-type", mime)
        .body(full(if *method == Method::HEAD {
            Bytes::new()
        } else {
            Bytes::from_static(asset.1)
        }))
        .unwrap())
}

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    if !value.is_object() {
        return Err(GatewayError::invalid());
    }
    serde_json::from_value(value).map_err(|_| GatewayError::invalid())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginInput {
    pat: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectInput {
    address: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveInput {
    device: Value,
    #[serde(default, deserialize_with = "crate::contracts::optional")]
    secret: Option<Value>,
    #[serde(default, deserialize_with = "crate::contracts::optional")]
    ca: Option<Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    username: String,
    password: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteInput {
    #[serde(deserialize_with = "crate::contracts::integer")]
    revision: i64,
    #[serde(default)]
    confirmed: bool,
}
#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    #[serde(deserialize_with = "crate::contracts::integer")]
    revision: i64,
    sidebar: bool,
    theme: String,
    selected: String,
    order: Vec<String>,
    visible: Vec<String>,
    #[serde(deserialize_with = "crate::contracts::integer")]
    tile_size: i64,
}
async fn dispatch(
    state: Arc<State>,
    parts: hyper::http::request::Parts,
    body: Value,
    auth: Option<(Identity, Option<String>)>,
    peer: Connection,
) -> Result<Response<Body>> {
    let path = parts.uri.path();
    let method = parts.method.clone();
    if path == "/api/v1/login" && method == Method::POST {
        if header(&parts.headers, "origin")?.is_none_or(|value| !state.origins.contains(value)) {
            return Err(GatewayError::new("FORBIDDEN"));
        }
        let input: LoginInput = parse(body)?;
        if input.pat.chars().count() > 256 {
            return Err(GatewayError::invalid());
        }
        let login = state.core.auth.login(&input.pat)?;
        let cookie = format!(
            "kvmhelm={}; HttpOnly; SameSite=Strict; Max-Age=28800; Path=/{}",
            login.id,
            if state.secure { "; Secure" } else { "" }
        );
        let mut response = json_response(200, json!({"csrf":login.csrf}));
        response
            .headers_mut()
            .insert("set-cookie", HeaderValue::from_str(&cookie).unwrap());
        return Ok(response);
    }
    let (identity, csrf) = auth.ok_or_else(|| GatewayError::new("AUTH_FAILED"))?;
    state.core.auth.check(&identity, None, None)?;
    if path == "/mcp" {
        let reply = state.mcp.handle(parts, body, identity.clone()).await?;
        let (parts, body) = reply.response.into_parts();
        if let Some(lifetime) = reply.lifetime {
            let lease = stream_lease(&state, &identity, None, peer, None, Some(lifetime))?;
            let stream = stream::unfold((body, lease), |(mut body, lease)| async move {
                let frame = body.frame().await?;
                Some((frame.map_err(|never| match never {}), (body, lease)))
            });
            return Ok(Response::from_parts(
                parts,
                StreamBody::new(stream).boxed_unsync(),
            ));
        }
        return Ok(Response::from_parts(
            parts,
            body.map_err(|never| match never {}).boxed_unsync(),
        ));
    }
    let route = path
        .strip_prefix("/api/v1/")
        .unwrap_or("")
        .split('/')
        .collect::<Vec<_>>();
    let value = match (method.clone(), route.as_slice()) {
        (Method::GET, ["me"]) => {
            let token = state.core.auth.check(&identity, None, None)?;
            let mut value = json!({"owner_id":"owner","token":token.public()});
            if let Some(csrf) = csrf {
                value["csrf"] = csrf.into();
            }
            value
        }
        (Method::POST, ["logout"]) => {
            if let Some(id) = &identity.ui_id {
                state.core.auth.logout(id)?;
            }
            state.core.disconnect_client(&identity).await;
            let mut response = json_response(200, json!({"ok":true}));
            response.headers_mut().insert(
                "set-cookie",
                HeaderValue::from_static("kvmhelm=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict"),
            );
            return Ok(response);
        }
        (Method::GET, ["devices"]) => json!(state.core.list(&identity)?),
        (Method::POST, ["certificates", "inspect"]) => {
            state
                .core
                .auth
                .check(&identity, Some("devices:manage"), None)?;
            let input: InspectInput = parse(body)?;
            crate::transport::inspect_certificate(&input.address).await?
        }
        (Method::POST, ["devices"]) => {
            let input: SaveInput = parse(body)?;
            if let Some(secret) = &input.secret
                && !secret.is_null()
            {
                let value: Credentials = parse(secret.clone())?;
                if value.username.chars().count() > 100 || value.password.chars().count() > 1000 {
                    return Err(GatewayError::invalid());
                }
            }
            if let Some(ca) = &input.ca
                && !ca.is_null()
                && ca
                    .as_str()
                    .is_none_or(|value| value.chars().count() > 65536)
            {
                return Err(GatewayError::invalid());
            }
            state
                .core
                .save_device(&identity, input.device, input.secret, input.ca)
                .await?
        }
        (Method::DELETE, ["devices", id]) => {
            let input: DeleteInput = parse(body)?;
            state
                .core
                .delete_device(&identity, id, input.revision, input.confirmed)
                .await?;
            json!({"ok":true})
        }
        (Method::POST, ["devices", id, "test" | "benchmark"]) => {
            state
                .core
                .auth
                .check(&identity, Some("devices:manage"), Some(id))?;
            let runtime = state.core.get(id)?;
            state.core.connect(&runtime).await?;
            if route[2] == "benchmark" {
                runtime.media.benchmark().await?
            } else {
                runtime
                    .media
                    .screenshot(Duration::ZERO, Duration::from_secs(5), false)
                    .await?;
                runtime.describe()
            }
        }
        (Method::POST, ["devices", id, "stop" | "resume"]) => {
            state
                .core
                .stop_control(&identity, id, route[2] == "resume")
                .await?
        }
        (Method::GET, ["tokens"]) => {
            state
                .core
                .auth
                .check(&identity, Some("tokens:manage"), None)?;
            json!(state.core.auth.list()?)
        }
        (Method::POST, ["tokens"]) => state.core.auth.create(body, Some(&identity)).await?.wire(),
        (Method::DELETE, ["tokens", id]) => {
            state
                .core
                .auth
                .check(&identity, Some("tokens:manage"), None)?;
            state.core.auth.revoke(id).await?;
            json!({"ok":true})
        }
        (Method::GET, ["preferences"]) => {
            state
                .core
                .auth
                .check(&identity, Some("devices:read"), None)?;
            state.core.store.get::<Value>(Kind::Preferences,"owner").await?.unwrap_or_else(||json!({"revision":0,"sidebar":true,"theme":"dark","selected":"","order":[],"visible":[],"tile_size":400}))
        }
        (Method::PUT, ["preferences"]) => {
            state
                .core
                .auth
                .check(&identity, Some("devices:read"), None)?;
            let input: Preferences = parse(body)?;
            if !matches!(input.theme.as_str(), "light" | "dark")
                || input.order.len() > 1000
                || input.visible.len() > 1000
                || !(240..=1000).contains(&input.tile_size)
            {
                return Err(GatewayError::invalid());
            }
            state
                .core
                .store
                .put(
                    Kind::Preferences,
                    "owner",
                    serde_json::to_value(&input).map_err(|_| GatewayError::invalid())?,
                    input.revision,
                )
                .await?
        }
        (Method::GET, ["sessions"]) => json!(state.core.sessions(&identity)?),
        (Method::POST, ["sessions", id, "audio"]) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Input {
                microphone: bool,
            }
            let input: Input = parse(body)?;
            json!({"ticket":state.audio.issue(&state.core,&identity,id,input.microphone)?})
        }
        (Method::POST, ["tools", name]) => {
            if ![
                "list_computers",
                "open_computer",
                "computer",
                "computer_screenshot",
                "computer_control",
                "close_computer",
            ]
            .contains(name)
            {
                return Err(GatewayError::new("NOT_FOUND"));
            }
            crate::mcp::pack(&state.core.tool(&identity, name, body).await)
        }
        (Method::POST, ["sessions", id, "release-inputs"]) => {
            state.core.release_inputs(&identity, id).await?;
            json!({"ok":true})
        }
        (Method::GET, ["sessions", id, "frame"]) => {
            let session = state.core.session(&identity, id)?;
            let frame = state
                .core
                .observe(
                    &identity,
                    &session,
                    Duration::from_millis(100),
                    Duration::from_secs(3),
                    false,
                )
                .await?;
            return Ok(Response::builder()
                .header("content-type", frame.info.mime_type.name())
                .header(
                    "x-kvmhelm-frame",
                    serde_json::to_string(&frame.info).unwrap(),
                )
                .body(full(Bytes::from_owner(frame.data)))
                .unwrap());
        }
        (Method::GET, ["sessions", id, "video"]) => {
            return video(state.clone(), identity, id, peer);
        }
        (Method::GET, ["events"]) => return events(state.clone(), identity, &parts.headers, peer),
        (Method::GET, ["plugins" | "ui-extensions"]) => {
            state.core.auth.check(
                &identity,
                Some(if route[0] == "plugins" {
                    "plugins:manage"
                } else {
                    "devices:read"
                }),
                None,
            )?;
            // An empty installed inventory has a real empty view. Configured
            // plugins are never presented as running without their runtime.
            if !state
                .core
                .store
                .list::<Value>(Kind::Plugins)
                .await?
                .is_empty()
            {
                return Err(GatewayError::message(
                    "UNSUPPORTED_RUNTIME",
                    "Configured plugins require the plugin runtime, which is not yet integrated in this native build",
                ));
            }
            json!([])
        }
        (Method::GET, ["diagnostics"]) => {
            state
                .core
                .auth
                .check(&identity, Some("devices:manage"), None)?;
            if !state
                .core
                .store
                .list::<Value>(Kind::Plugins)
                .await?
                .is_empty()
            {
                return Err(GatewayError::message(
                    "UNSUPPORTED_RUNTIME",
                    "Configured plugins require the plugin runtime, which is not yet integrated in this native build",
                ));
            }
            let counters = crate::runtime::memory_and_cpu();
            json!({"runtime":concat!("rust/",env!("CARGO_PKG_VERSION")),"platform":std::env::consts::OS,"memory":counters["memory"],"cpu_time_ms":counters["cpu_time_ms"],"metrics":state.core.metrics(),"health":state.core.health(),"devices":state.core.list(&identity)?,"plugins":[]})
        }
        _ => return Err(GatewayError::new("NOT_FOUND")),
    };
    Ok(json_response(200, value))
}

struct StreamLease {
    state: std::sync::Weak<State>,
    token: String,
    _slot: OwnedSemaphorePermit,
    alive: CancellationToken,
    consumer: Option<(Arc<crate::media::Media>, String)>,
}
impl Drop for StreamLease {
    fn drop(&mut self) {
        self.alive.cancel();
        if let Some((media, id)) = &self.consumer {
            media.release(id);
        }
        if let Some(state) = self.state.upgrade() {
            let mut counts = state.token_streams.lock().unwrap();
            if let Some(count) = counts.get_mut(&self.token) {
                *count -= 1;
                if *count == 0 {
                    counts.remove(&self.token);
                }
            }
        }
    }
}
fn stream_lease(
    state: &Arc<State>,
    identity: &Identity,
    session: Option<String>,
    peer: Connection,
    scope: Option<&'static str>,
    stop: Option<CancellationToken>,
) -> Result<StreamLease> {
    let slot = state
        .streams
        .clone()
        .try_acquire_owned()
        .map_err(|_| GatewayError::new("RESOURCE_LIMIT"))?;
    {
        let mut counts = state.token_streams.lock().unwrap();
        let count = counts.entry(identity.token_id.clone()).or_default();
        if *count >= 64 {
            return Err(GatewayError::new("RESOURCE_LIMIT"));
        }
        *count += 1;
    }
    let alive = state.cancel.child_token();
    let lifetime = alive.clone();
    let core = state.core.clone();
    let who = identity.clone();
    let stop = stop.unwrap_or_default();
    state.tasks.spawn(async move {
        loop {
            tokio::select! {biased;_=lifetime.cancelled()=>break,_=stop.cancelled()=>{peer.cancellation.cancel();break;},_=tokio::time::sleep(Duration::from_secs(1))=>()};
            let checked=if let Some(id)=&session {core.session(&who,id).and_then(|session|core.auth.check(&who,scope,Some(&session.device_id)).map(|_|()))} else {core.auth.check(&who,scope,None).map(|_|())};
            if checked.is_err() {peer.cancellation.cancel();break;}
        }
    });
    Ok(StreamLease {
        state: Arc::downgrade(state),
        token: identity.token_id.clone(),
        _slot: slot,
        alive,
        consumer: None,
    })
}
fn video(
    state: Arc<State>,
    identity: Identity,
    id: &str,
    peer: Connection,
) -> Result<Response<Body>> {
    let session = state.core.session(&identity, id)?;
    state
        .core
        .auth
        .check(&identity, Some("video:read"), Some(&session.device_id))?;
    let runtime = state.core.get(&session.device_id)?;
    let consumer = uuid::Uuid::new_v4().to_string();
    let mut lease = stream_lease(
        &state,
        &identity,
        Some(id.into()),
        peer.clone(),
        Some("video:read"),
        None,
    )?;
    runtime.media.retain(&consumer, Profile::Overview);
    lease.consumer = Some((runtime.media.clone(), consumer));
    let receiver = runtime.media.subscribe();
    let core = state.core.clone();
    let session_id = id.to_owned();
    let stream = stream::unfold(
        (
            receiver,
            VecDeque::<Bytes>::new(),
            lease,
            core,
            identity,
            session_id,
            true,
            peer,
        ),
        |(mut receiver, mut chunks, lease, core, identity, id, mut initial, peer)| async move {
            loop {
                if lease.alive.is_cancelled() || peer.cancellation.is_cancelled() {
                    return None;
                }
                if let Some(chunk) = chunks.pop_front() {
                    return Some((
                        Ok::<_, BoxError>(Frame::data(chunk)),
                        (receiver, chunks, lease, core, identity, id, initial, peer),
                    ));
                }
                if !initial {
                    tokio::select! {biased;_=lease.alive.cancelled()=>return None,_=peer.cancellation.cancelled()=>return None,changed=receiver.changed()=>if changed.is_err() {return None;}}
                }
                initial = false;
                if core.session(&identity, &id).is_err() {
                    return None;
                }
                if let Some(frame) = receiver.borrow_and_update().clone() {
                    let text = format!(
                        "--kvmhelm\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n",
                        frame.info.mime_type.name(),
                        frame.data.len()
                    );
                    chunks.push_back(text.into());
                    chunks.push_back(Bytes::from_owner(frame.data));
                    chunks.push_back(Bytes::from_static(b"\r\n"));
                }
            }
        },
    );
    Ok(Response::builder()
        .header(
            "content-type",
            "multipart/x-mixed-replace; boundary=kvmhelm",
        )
        .body(StreamBody::new(stream).boxed_unsync())
        .unwrap())
}
fn events(
    state: Arc<State>,
    identity: Identity,
    headers: &HeaderMap,
    peer: Connection,
) -> Result<Response<Body>> {
    state
        .core
        .auth
        .check(&identity, Some("devices:read"), None)?;
    let instance = header(headers, "x-kvmhelm-instance")?;
    let cursor = header(headers, "last-event-id")?
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let (history, receiver) = state.core.subscribe_events(instance, cursor);
    let lease = stream_lease(
        &state,
        &identity,
        None,
        peer.clone(),
        Some("devices:read"),
        None,
    )?;
    let core = state.core.clone();
    let initial = VecDeque::from(history);
    let stream = stream::unfold(
        (initial, receiver, lease, core, identity, peer),
        |(mut initial, mut receiver, lease, core, identity, peer)| async move {
            loop {
                if lease.alive.is_cancelled() || peer.cancellation.is_cancelled() {
                    return None;
                }
                let event = if let Some(event) = initial.pop_front() {
                    Some(event)
                } else {
                    tokio::select! {biased;_=lease.alive.cancelled()=>return None,_=peer.cancellation.cancelled()=>return None,event=receiver.recv()=>match event {Ok(event)=>Some(event),Err(_)=>return None},_=tokio::time::sleep(Duration::from_secs(1))=>None}
                };
                let data = if let Some(event) = event {
                    if core
                        .auth
                        .check(&identity, Some("devices:read"), event.device_id.as_deref())
                        .is_err()
                    {
                        continue;
                    }
                    format!(
                        "id: {}\ndata: {}\n\n",
                        event.event_id,
                        serde_json::to_string(&event).unwrap()
                    )
                } else {
                    ": heartbeat\n\n".into()
                };
                return Some((
                    Ok::<_, BoxError>(Frame::data(Bytes::from(data))),
                    (initial, receiver, lease, core, identity, peer),
                ));
            }
        },
    );
    Ok(Response::builder()
        .header("content-type", "text/event-stream")
        .header("x-kvmhelm-instance", state.core.instance_id.clone())
        .header("connection", "keep-alive")
        .body(StreamBody::new(stream).boxed_unsync())
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ipv6_rate_windows_group_a_56_bit_prefix_and_preserve_mapped_ipv4() {
        let key = |ip: &str| rate_limit_key(ip.parse().unwrap());
        assert_eq!(key("2001:db8:1234:56ff::1"), key("2001:db8:1234:56aa::2"));
        assert_ne!(key("2001:db8:1234:56ff::1"), key("2001:db8:1234:57aa::2"));
        assert_eq!(key("::ffff:127.0.0.1"), key("127.0.0.1"));
        assert_ne!(key("127.0.0.1"), key("127.0.0.2"));
    }
    #[tokio::test]
    async fn outbound_bytes_keep_admission_after_the_http_body_has_been_dropped() {
        let slots = Arc::new(Semaphore::new(1));
        let mut body = admitted_body(
            full("buffered response"),
            slots.clone().try_acquire_owned().unwrap(),
        );
        let bytes = body.frame().await.unwrap().unwrap().into_data().unwrap();
        let buffered = bytes.clone();
        drop(body);
        drop(bytes);
        assert_eq!(slots.available_permits(), 0);
        assert_eq!(&buffered[..], b"buffered response");
        drop(buffered);
        assert_eq!(slots.available_permits(), 1);
    }
}
