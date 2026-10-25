//! Stdio tools proxy. Its disposable HTTP session belongs to the existing
//! daemon. Explicit retry policy keeps uncertain input from being repeated.
use crate::{GatewayError, Result, mcp_http::RESPONSE_LIMIT};
use futures_util::StreamExt;
use reqwest::Method;
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt, model::*, service::RequestContext,
    transport::Transport,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore},
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

pub(crate) fn read_pat(path: &Path) -> Result<Zeroizing<String>> {
    let text = if let Ok(value) = std::env::var("KVMHELM_PAT") {
        value
    } else {
        let mut text = String::new();
        std::fs::File::open(path)
            .and_then(|file| file.take(4097).read_to_string(&mut text))
            .map_err(|_| GatewayError::message("AUTH_FAILED", "Cannot read protected PAT file"))?;
        text
    };
    let text = Zeroizing::new(text);
    let pat = text.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}');
    if text.len() > 4096 || pat.is_empty() || pat.len() > 256 {
        return Err(GatewayError::message(
            "AUTH_FAILED",
            "Invalid protected PAT file",
        ));
    }
    Ok(Zeroizing::new(pat.into()))
}
enum Failure {
    Rpc(ErrorData),
    Transport(GatewayError),
}
impl Failure {
    fn recoverable(&self) -> bool {
        match self {
            Self::Rpc(error) => matches!(error.code.0, -32000 | -32001),
            Self::Transport(error) => error.code == "MCP_CONNECTION_LOST",
        }
    }
    fn rpc(self) -> ErrorData {
        match self {
            Self::Rpc(error) => error,
            Self::Transport(error) => ErrorData::internal_error(error.message, None),
        }
    }
}
fn lost() -> Failure {
    Failure::Transport(GatewayError::new("MCP_CONNECTION_LOST"))
}
struct Connection {
    id: Option<String>,
    token: Zeroizing<String>,
    version: String,
    info: ServerConfig,
}
struct Upstream {
    http: reqwest::Client,
    url: url::Url,
    pat_file: PathBuf,
    current: AsyncMutex<Option<Arc<Connection>>>,
    sequence: AtomicU64,
    closed: AtomicBool,
}
impl Upstream {
    async fn exchange(
        &self,
        connection: Option<&Connection>,
        token: &str,
        method: &str,
        params: Value,
        notification: bool,
    ) -> std::result::Result<Value, Failure> {
        let id = self.sequence.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({"jsonrpc":"2.0","method":method,"params":params});
        if !notification {
            message["id"] = id.into();
        }
        let body = message.to_string();
        if body.len() > 256 * 1024 {
            return Err(Failure::Transport(GatewayError::invalid()));
        }
        let mut request = self
            .http
            .post(self.url.clone())
            .bearer_auth(token)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(body);
        if let Some(connection) = connection {
            request = request.header("mcp-protocol-version", &connection.version);
            if let Some(id) = &connection.id {
                request = request.header("mcp-session-id", id);
            }
        }
        let mut response = request.send().await.map_err(|_| lost())?;
        let status = response.status();
        if !status.is_success() {
            return Err(Failure::Transport(GatewayError::new(
                match status.as_u16() {
                    401 => "AUTH_FAILED",
                    403 => "FORBIDDEN",
                    429 => "RATE_LIMIT",
                    400 | 406 | 415 => "INVALID_ARGUMENT",
                    status if status == 404 || status >= 500 => "MCP_CONNECTION_LOST",
                    _ => "HTTP_ERROR",
                },
            )));
        }
        if notification {
            return Ok(Value::Null);
        }
        let session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|id| id.to_str().ok())
            .map(String::from);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let envelope: Value = if content_type.starts_with("application/json") {
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| lost())? {
                if chunk.len() > RESPONSE_LIMIT.saturating_sub(bytes.len()) {
                    return Err(lost());
                }
                bytes.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&bytes).map_err(|_| lost())?
        } else if content_type.starts_with("text/event-stream") {
            // Bound raw bytes before the SSE parser can buffer an unterminated
            // event. Finite POSTs have a 40s HTTP deadline; no GET stream/retry
            // is needed because this proxy advertises only tools.
            let mut received = 0usize;
            let source = response.bytes_stream().map(move |chunk| {
                let chunk = chunk.map_err(|_| std::io::Error::other("Daemon stream failed"))?;
                if chunk.len() > RESPONSE_LIMIT.saturating_sub(received) {
                    return Err(std::io::Error::other("Daemon response exceeded budget"));
                }
                received += chunk.len();
                Ok(chunk)
            });
            let mut events = sse_stream::SseStream::from_bytes_stream(source);
            let mut terminal = None;
            while let Some(event) = events.next().await {
                let event = event.map_err(|_| lost())?;
                if let Some(data) = event.data {
                    let value: Value = serde_json::from_str(&data).map_err(|_| lost())?;
                    if value["id"] == id
                        && (value.get("result").is_some() || value.get("error").is_some())
                    {
                        terminal = Some(value);
                        break;
                    }
                    // There are no server-initiated requests in the tools-only
                    // gateway contract. Never invent acknowledgements for one.
                    if value.get("method").is_some() && value.get("id").is_some() {
                        return Err(lost());
                    }
                }
            }
            terminal.ok_or_else(lost)?
        } else {
            return Err(lost());
        };
        if envelope["jsonrpc"] != "2.0" || envelope["id"] != id {
            return Err(lost());
        }
        if let Some(error) = envelope.get("error") {
            return Err(Failure::Rpc(
                serde_json::from_value(error.clone()).map_err(|_| lost())?,
            ));
        }
        let result = envelope.get("result").cloned().ok_or_else(lost)?;
        Ok(json!({"result":result,"session":session}))
    }
    async fn connect(&self) -> std::result::Result<Arc<Connection>, Failure> {
        let mut current = self.current.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(lost());
        }
        if let Some(connection) = &*current {
            return Ok(connection.clone());
        }
        let token = read_pat(&self.pat_file).map_err(Failure::Transport)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let initialize = async {
            let opened = self
                .exchange(
                    None,
                    &token,
                    "initialize",
                    json!({
                        "protocolVersion":"2025-11-25","capabilities":{},
                        "clientInfo":{"name":"KVMHelm stdio bridge","version":"0.1.0"}
                    }),
                    false,
                )
                .await?;
            let info: ServerConfig =
                serde_json::from_value(opened["result"].clone()).map_err(|_| lost())?;
            if !crate::mcp::legacy_versions()
                .iter()
                .any(|version| version == &info.protocol_version)
            {
                return Err(lost());
            }
            let connection = Arc::new(Connection {
                id: opened["session"].as_str().map(String::from),
                version: info.protocol_version.as_str().into(),
                info,
                token,
            });
            Ok::<_, Failure>(connection)
        };
        let connection = tokio::time::timeout_at(deadline, initialize)
            .await
            .map_err(|_| lost())??;
        let initialized = tokio::time::timeout_at(
            deadline,
            self.exchange(
                Some(&connection),
                &connection.token,
                "notifications/initialized",
                json!({}),
                true,
            ),
        )
        .await;
        if !matches!(initialized, Ok(Ok(_))) {
            self.delete(&connection).await;
            return Err(match initialized {
                Ok(Err(error)) => error,
                _ => lost(),
            });
        }
        if self.closed.load(Ordering::Acquire) {
            self.delete(&connection).await;
            return Err(lost());
        }
        *current = Some(connection.clone());
        Ok(connection)
    }
    async fn delete(&self, connection: &Connection) {
        if let Some(id) = &connection.id {
            let request = self
                .http
                .request(Method::DELETE, self.url.clone())
                .bearer_auth(connection.token.as_str())
                .header("mcp-session-id", id)
                .header("mcp-protocol-version", &connection.version)
                .send();
            let _ = tokio::time::timeout(Duration::from_secs(2), request).await;
        }
    }
    async fn invalidate(&self, connection: &Arc<Connection>) {
        {
            let mut current = self.current.lock().await;
            if current
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, connection))
            {
                current.take();
            }
        }
        self.delete(connection).await;
    }
    async fn request(
        &self,
        method: &str,
        params: Value,
        retry: bool,
    ) -> std::result::Result<Value, Failure> {
        for attempt in 0..2 {
            let connection = match self.connect().await {
                Ok(connection) => connection,
                Err(error) if error.recoverable() && retry && attempt == 0 => continue,
                Err(error) => return Err(error),
            };
            match self
                .exchange(
                    Some(&connection),
                    &connection.token,
                    method,
                    params.clone(),
                    false,
                )
                .await
            {
                Ok(value) => return Ok(value["result"].clone()),
                Err(error) => {
                    if !error.recoverable() {
                        if matches!(&error, Failure::Transport(error) if matches!(error.code.as_str(),"AUTH_FAILED"|"FORBIDDEN"))
                        {
                            self.invalidate(&connection).await;
                        }
                        return Err(error);
                    }
                    self.invalidate(&connection).await;
                    if !retry || attempt > 0 {
                        return Err(error);
                    }
                }
            }
        }
        Err(lost())
    }
    async fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let connection = self.current.lock().await.take();
        if let Some(connection) = connection {
            self.delete(&connection).await;
        }
    }
}
#[derive(Clone)]
struct Bridge {
    upstream: Arc<Upstream>,
    info: ServerConfig,
}
impl ServerHandler for Bridge {
    fn get_info(&self) -> ServerConfig {
        self.info.clone()
    }
    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        crate::mcp::legacy_versions()
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        let value = self
            .upstream
            .request("tools/list", json!({}), true)
            .await
            .map_err(Failure::rpc)?;
        serde_json::from_value(value)
            .map_err(|_| ErrorData::internal_error("Invalid daemon tool list", None))
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let retry = request.name == "list_computers";
        let value = self
            .upstream
            .request(
                "tools/call",
                serde_json::to_value(request).expect("MCP call"),
                retry,
            )
            .await;
        let value = match value {
            Ok(value) => value,
            Err(error) if error.recoverable() => {
                let body = json!({"ok":false,"frames":[],"error":{
                    "code":"MCP_CONNECTION_LOST",
                    "message":"Daemon connection lost. This operation was not automatically repeated; input may already have executed. Use list_computers, then open a new session and observe before deciding further input. Old sessions and references may be invalid."
                }});
                json!({"isError":true,"structuredContent":body,"content":[{"type":"text","text":body.to_string()}]})
            }
            Err(error) => return Err(error.rpc()),
        };
        let result: CallToolResult = serde_json::from_value(value)
            .map_err(|_| ErrorData::internal_error("Invalid daemon tool result", None))?;
        Ok(result.into())
    }
}

/// Hold admission through the actual stdout write, not just the handler return.
/// Otherwise blocked stdout can accumulate arbitrarily many completed images.
struct BoundedStdio<R: tokio::io::AsyncRead, W: tokio::io::AsyncWrite> {
    read: BufReader<R>,
    line: Vec<u8>,
    discarding: bool,
    write: Arc<AsyncMutex<Option<W>>>,
    slots: Arc<Semaphore>,
    pending: Arc<Mutex<HashMap<RequestId, OwnedSemaphorePermit>>>,
}
impl<R, W> BoundedStdio<R, W>
where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    fn new(read: R, write: W) -> Self {
        Self {
            read: BufReader::new(read),
            line: Vec::new(),
            discarding: false,
            write: Arc::new(AsyncMutex::new(Some(write))),
            slots: Arc::new(Semaphore::new(16)),
            pending: Arc::default(),
        }
    }
    async fn read_line(&mut self) -> Option<std::result::Result<Value, ErrorData>> {
        loop {
            let available = self.read.fill_buf().await.ok()?;
            if available.is_empty() {
                return None;
            }
            let end = available.iter().position(|byte| *byte == b'\n');
            let count = end.map_or(available.len(), |index| index + 1);
            if self.discarding {
                self.read.consume(count);
                if end.is_some() {
                    self.discarding = false;
                }
                continue;
            }
            if count > (256 * 1024usize).saturating_sub(self.line.len()) {
                self.line.clear();
                self.discarding = end.is_none();
                self.read.consume(count);
                return Some(Err(ErrorData::invalid_request(
                    "Stdio message exceeded 256 KiB",
                    None,
                )));
            }
            self.line.extend_from_slice(&available[..count]);
            self.read.consume(count);
            if end.is_none() {
                continue;
            }
            if self.line.iter().all(u8::is_ascii_whitespace) {
                self.line.clear();
                continue;
            }
            let value = serde_json::from_slice(&self.line)
                .map_err(|_| ErrorData::parse_error("Invalid JSON", None));
            self.line.clear();
            return Some(value);
        }
    }
}
async fn write_stdio<W: tokio::io::AsyncWrite + Unpin>(
    write: Arc<AsyncMutex<Option<W>>>,
    message: ServerJsonRpcMessage,
) -> std::io::Result<()> {
    let mut value = serde_json::to_value(message)
        .map_err(|_| std::io::Error::other("Cannot encode MCP response"))?;
    crate::mcp::restore_legacy_tool_metadata(&mut value);
    let mut bytes = serde_json::to_vec(&value)
        .map_err(|_| std::io::Error::other("Cannot encode MCP response"))?;
    if bytes.len() > RESPONSE_LIMIT {
        return Err(std::io::Error::other("MCP response exceeded budget"));
    }
    bytes.push(b'\n');
    let mut writer = write.lock().await;
    let writer = writer
        .as_mut()
        .ok_or_else(|| std::io::Error::other("Stdio is closed"))?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}
impl<R, W> Transport<RoleServer> for BoundedStdio<R, W>
where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    type Error = std::io::Error;
    fn send(
        &mut self,
        message: ServerJsonRpcMessage,
    ) -> impl Future<Output = std::io::Result<()>> + Send + 'static {
        let id = match &message {
            ServerJsonRpcMessage::Response(response) => Some(response.id.clone()),
            ServerJsonRpcMessage::Error(error) => error.id.clone(),
            _ => None,
        };
        let pending = self.pending.clone();
        let write = self.write.clone();
        async move {
            let result = write_stdio(write, message).await;
            if let Some(id) = id {
                pending.lock().unwrap().remove(&id);
            }
            result
        }
    }
    async fn receive(&mut self) -> Option<ClientJsonRpcMessage> {
        let slot = self.slots.clone().acquire_owned().await.ok()?;
        let message = loop {
            let value = match self.read_line().await? {
                Ok(value) => value,
                Err(error) => {
                    write_stdio(self.write.clone(), ServerJsonRpcMessage::error(error, None))
                        .await
                        .ok()?;
                    continue;
                }
            };
            let id = value
                .get("id")
                .and_then(|id| serde_json::from_value::<RequestId>(id.clone()).ok());
            let parsed = crate::mcp::validate_wire_request(&value).and_then(|()| {
                serde_json::from_value(value)
                    .map_err(|_| ErrorData::invalid_request("Invalid MCP message", None))
            });
            match parsed {
                Ok(message) => break message,
                Err(error) => {
                    write_stdio(self.write.clone(), ServerJsonRpcMessage::error(error, id))
                        .await
                        .ok()?;
                }
            }
        };
        if let ClientJsonRpcMessage::Request(request) = &message {
            let mut pending = self.pending.lock().unwrap();
            if pending.contains_key(&request.id) {
                return None;
            }
            pending.insert(request.id.clone(), slot);
        }
        Some(message)
    }
    async fn close(&mut self) -> std::io::Result<()> {
        self.slots.close();
        self.pending.lock().unwrap().clear();
        tokio::time::timeout(Duration::from_secs(2), async {
            if let Some(mut write) = self.write.lock().await.take() {
                write.shutdown().await?;
            }
            Ok(())
        })
        .await
        .unwrap_or_else(|_| Err(std::io::Error::other("Stdio close timed out")))
    }
}

pub(crate) async fn run(http: reqwest::Client, url: url::Url, pat_file: PathBuf) -> Result<()> {
    let upstream = Arc::new(Upstream {
        http,
        url: url
            .join("/mcp")
            .map_err(|_| GatewayError::new("INVALID_ADDRESS"))?,
        pat_file,
        current: AsyncMutex::new(None),
        sequence: AtomicU64::new(1),
        closed: AtomicBool::new(false),
    });
    let connection = upstream
        .connect()
        .await
        .map_err(|_| GatewayError::new("MCP_CONNECTION_LOST"))?;
    let mut info = crate::mcp::server_info();
    // Like the original CLI bridge, expose tools without promising to forward
    // upstream tool-list notifications through the independent stdio session.
    info.capabilities = serde_json::from_value(json!({"tools":{}})).expect("bridge capabilities");
    info.instructions = connection.info.instructions.clone();
    let bridge = Bridge {
        upstream: upstream.clone(),
        info,
    };
    let transport = BoundedStdio::new(tokio::io::stdin(), tokio::io::stdout());
    let service = match bridge.serve(transport).await {
        Ok(service) => service,
        Err(_) => {
            upstream.close().await;
            return Err(GatewayError::new("MCP_CONNECTION_LOST"));
        }
    };
    let stopped = CancellationToken::new();
    let stop_watcher = stopped.clone();
    let cancel = service.cancellation_token();
    let watcher = tokio::spawn(async move {
        tokio::select! { _=stop_watcher.cancelled()=>(),_=crate::cli::shutdown_signal()=>cancel.cancel() }
    });
    let result = service.waiting().await;
    stopped.cancel();
    let _ = watcher.await;
    upstream.close().await;
    result
        .map(|_| ())
        .map_err(|_| GatewayError::new("MCP_CONNECTION_LOST"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test]
    async fn malformed_stdio_objects_and_oversize_lines_do_not_hide_the_next_request() {
        let (mut input, read) = tokio::io::duplex(16384);
        let (write, output) = tokio::io::duplex(1024);
        let mut transport = BoundedStdio::new(read, write);
        let writer = tokio::spawn(async move {
            input.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"list_computers\",\"arguments\":null}}\n").await.unwrap();
            input.write_all(&vec![b'x'; 256 * 1024 + 1]).await.unwrap();
            input
                .write_all(b"\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n")
                .await
                .unwrap();
        });
        let request = transport.receive().await.unwrap();
        assert!(
            matches!(request,ClientJsonRpcMessage::Request(request) if request.id == RequestId::Number(2))
        );
        let mut output = BufReader::new(output);
        for (id, code) in [(json!(1), -32602), (Value::Null, -32600)] {
            let mut line = String::new();
            output.read_line(&mut line).await.unwrap();
            let response: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(response["id"], id);
            assert_eq!(response["error"]["code"], code);
        }
        assert_eq!(transport.slots.available_permits(), 15);
        writer.await.unwrap();
        transport.close().await.unwrap();
    }

    #[tokio::test]
    async fn stdio_admission_waits_until_a_blocked_stdout_reply_is_written() {
        let (mut input, read) = tokio::io::duplex(16384);
        let (write, output) = tokio::io::duplex(8);
        let mut transport = BoundedStdio::new(read, write);
        for id in 0..16 {
            input
                .write_all(
                    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"ping\"}}\n").as_bytes(),
                )
                .await
                .unwrap();
            assert!(transport.receive().await.is_some());
        }
        assert_eq!(transport.slots.available_permits(), 0);
        input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":16,\"method\":\"ping\"}\n")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), transport.receive())
                .await
                .is_err()
        );
        let response: ServerJsonRpcMessage =
            serde_json::from_value(json!({"jsonrpc":"2.0","id":0,"result":{}})).unwrap();
        let sending = tokio::spawn(transport.send(response));
        tokio::task::yield_now().await;
        assert_eq!(transport.slots.available_permits(), 0);
        assert!(!sending.is_finished());
        let mut output = BufReader::new(output);
        let mut line = String::new();
        output.read_line(&mut line).await.unwrap();
        sending.await.unwrap().unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["id"], 0);
        let message = transport.receive().await.unwrap();
        assert!(
            matches!(message,ClientJsonRpcMessage::Request(request) if request.id == RequestId::Number(16))
        );
        transport.close().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_partial_stdio_reads_retain_bytes_and_release_their_admission() {
        let (mut input, read) = tokio::io::duplex(16384);
        let mut transport = BoundedStdio::new(read, tokio::io::sink());
        input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), transport.receive())
                .await
                .is_err()
        );
        assert_eq!(transport.slots.available_permits(), 16);
        input.write_all(b"42,\"method\":\"ping\"}\n").await.unwrap();
        let message = transport.receive().await.unwrap();
        assert!(
            matches!(message,ClientJsonRpcMessage::Request(request) if request.id == RequestId::Number(42))
        );
        assert_eq!(transport.slots.available_permits(), 15);
        transport.close().await.unwrap();
    }
}
