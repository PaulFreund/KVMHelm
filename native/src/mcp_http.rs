//! Stateful MCP over the existing HTTP listener. The SDK owns protocol handling;
//! this boundary owns identities, admission, response budgets and Core cleanup.
use crate::{GatewayError, Result, auth::Identity, core::Core, mcp::Handler};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{Method, Request, Response, body::Body, http::request::Parts};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::{SessionManager, local::LocalSessionManager},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub(crate) type SdkBody = BoxBody<Bytes, Infallible>;
pub(crate) const RESPONSE_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Endpoint {
    core: Arc<Core>,
    table: Arc<Mutex<Table>>,
    config: StreamableHttpServerConfig,
    limits: (usize, usize),
    idle: Duration,
}
#[derive(Default)]
struct Table {
    sessions: HashMap<String, Arc<Entry>>,
    reservations: HashMap<String, usize>,
    closing: bool,
}
struct Reservation {
    table: Weak<Mutex<Table>>,
    token: String,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(table) = self.table.upgrade() {
            let mut table = table.lock().unwrap();
            if let Some(count) = table.reservations.get_mut(&self.token) {
                *count -= 1;
                if *count == 0 {
                    table.reservations.remove(&self.token);
                }
            }
        }
    }
}
struct Entry {
    identity: Identity,
    last: Mutex<Instant>,
    manager: Arc<LocalSessionManager>,
    service: StreamableHttpService<Handler, LocalSessionManager>,
    cancel: CancellationToken,
    pending: Mutex<HashSet<rmcp::model::RequestId>>,
    _reservation: Reservation,
}
struct PendingRequest {
    entry: Arc<Entry>,
    id: rmcp::model::RequestId,
}
impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.entry.pending.lock().unwrap().remove(&self.id);
    }
}
impl Entry {
    async fn close(&self, core: &Arc<Core>) {
        self.cancel.cancel();
        let ids = self
            .manager
            .sessions
            .read()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let protocol = async {
            for id in ids {
                let _ =
                    tokio::time::timeout(Duration::from_secs(5), self.manager.close_session(&id))
                        .await;
            }
        };
        tokio::join!(protocol, core.disconnect_client(&self.identity));
    }
}
pub(crate) struct Reply {
    pub response: Response<SdkBody>,
    pub lifetime: Option<CancellationToken>,
}
fn rpc_error(id: Value, code: i32, message: &str) -> Response<SdkBody> {
    Response::builder()
        .header("content-type", "application/json")
        .body(
            Full::new(Bytes::from(
                json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
                    .to_string(),
            ))
            .boxed(),
        )
        .unwrap()
}
pub(crate) async fn bounded_body<B: Body<Data = Bytes> + Unpin>(
    mut body: B,
    limit: usize,
) -> Result<Bytes> {
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| GatewayError::new("MCP_CONNECTION_LOST"))?;
        if let Some(data) = frame.data_ref() {
            if data.len() > limit.saturating_sub(bytes.len()) {
                return Err(GatewayError::new("RESOURCE_LIMIT"));
            }
            bytes.extend_from_slice(data);
        }
    }
    Ok(bytes.into())
}
impl Endpoint {
    pub fn new(
        core: Arc<Core>,
        hosts: Vec<String>,
        origins: Vec<String>,
        limits: (usize, usize),
        idle: Duration,
    ) -> Self {
        let config = StreamableHttpServerConfig::default()
            .with_allowed_hosts(hosts)
            .with_allowed_origins(origins)
            .with_max_request_body_bytes(256 * 1024)
            .with_sse_retry(None)
            .with_sse_keep_alive(Some(Duration::from_secs(1)));
        Self {
            core,
            table: Arc::default(),
            config,
            limits,
            idle,
        }
    }
    fn reserve(&self, token: &str) -> Result<Reservation> {
        let mut table = self.table.lock().unwrap();
        if table.closing {
            return Err(GatewayError::new("DEVICE_OFFLINE"));
        }
        let total = table.reservations.values().sum::<usize>();
        if total >= self.limits.0
            || table.reservations.get(token).copied().unwrap_or(0) >= self.limits.1
        {
            return Err(GatewayError::new("RESOURCE_LIMIT"));
        }
        *table.reservations.entry(token.into()).or_default() += 1;
        Ok(Reservation {
            table: Arc::downgrade(&self.table),
            token: token.into(),
        })
    }
    fn new_entry(&self, mut identity: Identity) -> Result<Arc<Entry>> {
        let reservation = self.reserve(&identity.token_id)?;
        identity.client_id = format!("mcp:{}", uuid::Uuid::new_v4());
        let mut manager = LocalSessionManager::default();
        manager.session_config.keep_alive = Some(self.idle);
        manager.session_config.init_timeout = Some(Duration::from_secs(5));
        manager.session_config.sse_retry = None;
        manager.session_config.completed_cache_ttl = Duration::ZERO;
        let manager = Arc::new(manager);
        let cancel = CancellationToken::new();
        let handler = Handler {
            core: self.core.clone(),
            identity: identity.clone(),
        };
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            manager.clone(),
            self.config.clone().with_cancellation_token(cancel.clone()),
        );
        Ok(Arc::new(Entry {
            identity,
            last: Mutex::new(Instant::now()),
            manager,
            service,
            cancel,
            pending: Mutex::default(),
            _reservation: reservation,
        }))
    }
    async fn remove(&self, id: &str) {
        let entry = self.table.lock().unwrap().sessions.remove(id);
        if let Some(entry) = entry {
            entry.close(&self.core).await;
        }
    }
    pub async fn sweep(&self) {
        let expired = {
            let table = self.table.lock().unwrap();
            table
                .sessions
                .iter()
                .filter(|(_, entry)| {
                    entry.cancel.is_cancelled()
                        || entry.last.lock().unwrap().elapsed() >= self.idle
                        || self.core.auth.check(&entry.identity, None, None).is_err()
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        };
        for id in expired {
            self.remove(&id).await;
        }
    }
    pub async fn close(&self) {
        let entries = {
            let mut table = self.table.lock().unwrap();
            table.closing = true;
            table
                .sessions
                .drain()
                .map(|(_, entry)| entry)
                .collect::<Vec<_>>()
        };
        futures_util::future::join_all(entries.iter().map(|entry| entry.close(&self.core))).await;
    }
    pub async fn handle(&self, mut parts: Parts, body: Value, identity: Identity) -> Result<Reply> {
        let id = crate::server::header(&parts.headers, "mcp-session-id")?.map(String::from);
        let method = parts.method.clone();
        let entry = if let Some(id) = &id {
            let entry = self
                .table
                .lock()
                .unwrap()
                .sessions
                .get(id)
                .cloned()
                .ok_or_else(|| GatewayError::new("NOT_FOUND"))?;
            if identity.token_id != entry.identity.token_id {
                return Err(GatewayError::new("FORBIDDEN"));
            }
            self.core.auth.check(&entry.identity, None, None)?;
            if entry.cancel.is_cancelled() {
                return Err(GatewayError::new("NOT_FOUND"));
            }
            *entry.last.lock().unwrap() = Instant::now();
            entry
        } else {
            if method != Method::POST || body["method"] != "initialize" {
                return Err(GatewayError::message(
                    "INVALID_ARGUMENT",
                    "Initialize MCP first",
                ));
            }
            self.new_entry(identity)?
        };
        if let Some(version) = crate::server::header(&parts.headers, "mcp-protocol-version")?
            && !crate::mcp::legacy_versions()
                .iter()
                .any(|v| v.as_str() == version)
        {
            if id.is_none() {
                entry.close(&self.core).await;
            }
            return Err(GatewayError::message(
                "INVALID_ARGUMENT",
                "Unsupported MCP protocol version",
            ));
        }
        // The original SDK also negotiates 2024-10-07. Its message forms match
        // this legacy path; rmcp's HTTP header allowlist begins at 2024-11-05.
        if parts
            .headers
            .get("mcp-protocol-version")
            .is_some_and(|v| v == "2024-10-07")
        {
            parts.headers.remove("mcp-protocol-version");
        }
        let duplicate_init = id.is_some() && body["method"] == "initialize";
        if let Err(error) = crate::mcp::validate_wire_request(&body) {
            return Ok(Reply {
                response: rpc_error(body["id"].clone(), error.code.0, &error.message),
                lifetime: None,
            });
        }
        let unknown_tool = body["method"] == "tools/call"
            && !body["params"]["name"]
                .as_str()
                .is_some_and(crate::mcp::known_tool);
        if duplicate_init || unknown_tool {
            return Ok(Reply {
                response: rpc_error(
                    body["id"].clone(),
                    if duplicate_init { -32600 } else { -32602 },
                    if duplicate_init {
                        "Already initialized"
                    } else {
                        "Unknown tool"
                    },
                ),
                lifetime: None,
            });
        }
        let _pending = if method == Method::POST {
            if let Some(id) = body
                .get("id")
                .and_then(|id| serde_json::from_value::<rmcp::model::RequestId>(id.clone()).ok())
            {
                if !entry.pending.lock().unwrap().insert(id.clone()) {
                    return Ok(Reply {
                        response: rpc_error(
                            body["id"].clone(),
                            -32600,
                            "Request ID is already in flight",
                        ),
                        lifetime: None,
                    });
                }
                Some(PendingRequest {
                    entry: entry.clone(),
                    id,
                })
            } else {
                None
            }
        } else {
            None
        };
        // Reject the newer stateless lifecycle, which cannot preserve the
        // session identity/cleanup contract of the existing KVMHelm clients.
        if body["params"]["_meta"]["protocolVersion"]
            .as_str()
            .is_some_and(|v| {
                !crate::mcp::legacy_versions()
                    .iter()
                    .any(|version| version.as_str() == v)
            })
        {
            return Err(GatewayError::invalid());
        }
        let list_tools = body["method"] == "tools/list";
        let request = Request::from_parts(parts, Full::new(Bytes::from(body.to_string())));
        let response = entry.service.handle(request).await;
        if method == Method::GET {
            return Ok(Reply {
                response,
                lifetime: Some(entry.cancel.clone()),
            });
        }
        // Consume finite POST responses inside the accepted HTTP owner. Slow or
        // disconnected sockets cannot free its work slot while the tool runs.
        let (mut response_parts, response_body) = response.into_parts();
        let collected = tokio::time::timeout(
            Duration::from_secs(40),
            bounded_body(response_body, RESPONSE_LIMIT),
        )
        .await;
        let mut bytes = match collected {
            Ok(Ok(bytes)) => bytes,
            _ => {
                if let Some(id) = &id {
                    self.remove(id).await;
                } else {
                    entry.close(&self.core).await;
                }
                return Err(GatewayError::new("MCP_CONNECTION_LOST"));
            }
        };
        if list_tools && response_parts.status.is_success() {
            // The SDK writes one compact JSON message per SSE data line. Keep
            // its validated response and restore the original legacy metadata.
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| GatewayError::new("MCP_CONNECTION_LOST"))?;
            let mut message = if response_parts
                .headers
                .get("content-type")
                .is_some_and(|v| v.as_bytes().starts_with(b"application/json"))
            {
                serde_json::from_str::<Value>(text).ok()
            } else {
                text.lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .find_map(|data| {
                        serde_json::from_str::<Value>(data)
                            .ok()
                            .filter(|v| v.get("result").is_some() || v.get("error").is_some())
                    })
            };
            if let Some(message) = &mut message {
                crate::mcp::restore_legacy_tool_metadata(message);
                bytes = Bytes::from(message.to_string());
                response_parts.headers.insert(
                    "content-type",
                    hyper::header::HeaderValue::from_static("application/json"),
                );
                response_parts.headers.remove("content-length");
            }
        }
        if method == Method::DELETE && response_parts.status.is_success() {
            response_parts.status = hyper::StatusCode::OK;
        }
        if let Some(id) = &id {
            if method == Method::DELETE {
                self.remove(id).await;
            }
        } else if response_parts.status.is_success() {
            let session_id = response_parts
                .headers
                .get("mcp-session-id")
                .and_then(|id| id.to_str().ok())
                .map(String::from);
            if let Some(session_id) = session_id {
                let inserted = {
                    let mut table = self.table.lock().unwrap();
                    if table.closing {
                        false
                    } else {
                        table.sessions.insert(session_id, entry.clone());
                        true
                    }
                };
                if !inserted {
                    entry.close(&self.core).await;
                    return Err(GatewayError::new("DEVICE_OFFLINE"));
                }
            } else {
                entry.close(&self.core).await;
            }
        } else {
            entry.close(&self.core).await;
        }
        Ok(Reply {
            response: Response::from_parts(response_parts, Full::new(bytes).boxed()),
            lifetime: None,
        })
    }
}
