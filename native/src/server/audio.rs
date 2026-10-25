use super::*;
use crate::drivers::KvmDriver;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::derive_accept_key,
        protocol::{Role, WebSocketConfig},
    },
};

#[derive(Clone)]
struct Ticket {
    identity: Identity,
    session: String,
    microphone: bool,
    expires: Instant,
}
#[derive(Default)]
pub(super) struct Tickets(Mutex<HashMap<String, Ticket>>);
impl Tickets {
    pub fn issue(
        &self,
        core: &Arc<Core>,
        identity: &Identity,
        session: &str,
        microphone: bool,
    ) -> Result<String> {
        let runtime = core.authorize_audio(identity, session, microphone, None)?;
        let caps = runtime.describe();
        if caps["capabilities"]["audio"]["from_target"] != true
            || microphone && caps["capabilities"]["audio"]["to_target"] != true
        {
            return Err(GatewayError::message(
                "UNSUPPORTED_ACTION",
                "Requested audio direction unavailable",
            ));
        }
        let mut tickets = self.0.lock().unwrap();
        tickets.retain(|_, v| v.expires > Instant::now());
        if tickets.len() >= 256 {
            return Err(GatewayError::new("RESOURCE_LIMIT"));
        }
        let mut bytes = [0; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let id = URL_SAFE_NO_PAD.encode(bytes);
        tickets.insert(
            id.clone(),
            Ticket {
                identity: identity.clone(),
                session: session.into(),
                microphone,
                expires: Instant::now() + Duration::from_secs(10),
            },
        );
        Ok(id)
    }
    fn take(&self, key: &str) -> Result<Ticket> {
        self.0
            .lock()
            .unwrap()
            .remove(key)
            .filter(|v| v.expires > Instant::now())
            .ok_or_else(|| GatewayError::new("AUTH_FAILED"))
    }
}
pub(super) fn upgrade(
    state: Arc<State>,
    parts: &mut hyper::http::request::Parts,
    peer: Connection,
) -> Result<Response<Body>> {
    if parts.uri.query().is_some() || header(&parts.headers, "origin")?.is_none() {
        return Err(GatewayError::new("FORBIDDEN"));
    }
    let key = header(&parts.headers, "sec-websocket-key")?.ok_or_else(GatewayError::invalid)?;
    if base64::engine::general_purpose::STANDARD
        .decode(key)
        .ok()
        .is_none_or(|v| v.len() != 16)
        || header(&parts.headers, "sec-websocket-version")? != Some("13")
        || header(&parts.headers, "upgrade")?.is_none_or(|v| !v.eq_ignore_ascii_case("websocket"))
        || header(&parts.headers, "connection")?.is_none_or(|v| {
            !v.split(',')
                .any(|v| v.trim().eq_ignore_ascii_case("upgrade"))
        })
    {
        return Err(GatewayError::invalid());
    }
    let accept = derive_accept_key(key.as_bytes());
    let cookie = header(&parts.headers, "cookie")?.unwrap_or("").to_owned();
    let upgrade = parts
        .extensions
        .remove::<hyper::upgrade::OnUpgrade>()
        .ok_or_else(GatewayError::invalid)?;
    let slot = state
        .streams
        .clone()
        .try_acquire_owned()
        .map_err(|_| GatewayError::new("RESOURCE_LIMIT"))?;
    let app = state.clone();
    state.tasks.spawn(async move {
        run(app, peer, slot, upgrade, cookie).await;
    });
    Ok(Response::builder()
        .status(101)
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        .header("sec-websocket-accept", accept)
        .body(full(Bytes::new()))
        .unwrap())
}
async fn run(
    app: Arc<State>,
    peer: Connection,
    slot: OwnedSemaphorePermit,
    upgrade: hyper::upgrade::OnUpgrade,
    cookie: String,
) {
    let _slot = slot;
    let _socket = peer._socket;
    let upgraded = tokio::select! {biased;_=app.cancel.cancelled()=>return,result=tokio::time::timeout(Duration::from_secs(5),upgrade)=>match result{Ok(Ok(io))=>io,_=>return}};
    let config = WebSocketConfig::default()
        .read_buffer_size(4096)
        .write_buffer_size(0)
        .max_write_buffer_size(96000)
        .max_message_size(Some(16384))
        .max_frame_size(Some(16384));
    let mut ws =
        WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(config)).await;
    let ticket = tokio::select! {biased;_=app.cancel.cancelled()=>return,result=tokio::time::timeout(Duration::from_secs(5),async{
        loop{match ws.next().await{Some(Ok(Message::Text(text)))=>{
            let value:Value=serde_json::from_slice(text.as_bytes()).map_err(|_|GatewayError::new("AUTH_FAILED"))?;
            return app.audio.take(value["ticket"].as_str().filter(|s|s.len()<=128).ok_or_else(||GatewayError::new("AUTH_FAILED"))?);
        },Some(Ok(Message::Ping(_)|Message::Pong(_)))=>(),_=>return Err(GatewayError::new("AUTH_FAILED"))}}
    })=>match result{Ok(Ok(ticket))=>ticket,_=>{reject(&mut ws,"Authenticate").await;return}}};
    if ticket.identity.ui_id.as_ref().is_some_and(|id| {
        !cookie
            .split(';')
            .any(|c| c.trim() == format!("kvmhelm={id}"))
    }) {
        reject(&mut ws, "Authenticate").await;
        return;
    }
    let cancel = app.cancel.child_token();
    let _guard = cancel.clone().drop_guard();
    let runtime =
        match app
            .core
            .authorize_audio(&ticket.identity, &ticket.session, ticket.microphone, None)
        {
            Ok(runtime) => runtime,
            Err(_) => {
                reject(&mut ws, "Authorization expired").await;
                return;
            }
        };
    // The connection already holds its global stream slot. Account this
    // authenticated consumer in the same per-token limit as video/SSE.
    let admitted = {
        let mut counts = app.token_streams.lock().unwrap();
        let count = counts.entry(ticket.identity.token_id.clone()).or_default();
        if *count >= 64 {
            false
        } else {
            *count += 1;
            true
        }
    };
    if !admitted {
        reject(&mut ws, "Stream limit").await;
        return;
    }
    let consumer = format!("audio:{}", uuid::Uuid::new_v4());
    runtime.media.retain(&consumer, Profile::Warm);
    let lease = AudioLease {
        state: Arc::downgrade(&app),
        token: ticket.identity.token_id.clone(),
        media: runtime.media.clone(),
        consumer,
    };
    let context = if ticket.microphone {
        match app
            .core
            .audio_context(&ticket.identity, &ticket.session, cancel.clone())
        {
            Ok(context) => Some(context),
            Err(_) => {
                reject(&mut ws, "Control expired").await;
                return;
            }
        }
    } else {
        None
    };
    let owner = uuid::Uuid::new_v4().to_string();
    let mut mic = if ticket.microphone {
        let device = runtime.device.device_id.clone().unwrap_or_default();
        let claimed = {
            let mut senders = app.core.audio_microphones.lock().unwrap();
            if senders.contains_key(&device) {
                false
            } else {
                senders.insert(device.clone(), owner.clone());
                true
            }
        };
        if !claimed {
            reject(&mut ws, "Microphone already active").await;
            return;
        }
        Some(MicrophoneLease {
            errors: Arc::downgrade(&app.core),
            registry: app.core.audio_microphones.clone(),
            driver: runtime.driver.clone(),
            device,
            owner,
            context: cancel.clone(),
            armed: true,
        })
    } else {
        None
    };
    let (mut write, mut read) = ws.split();
    let reader_app = app.clone();
    let reader_ticket = ticket.clone();
    let reader_cancel = cancel.clone();
    let driver = runtime.driver.clone();
    let reader_context = context.clone();
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_ready = ready.clone();
    let reader = tokio::spawn(async move {
        let _ended = reader_cancel.clone().drop_guard();
        let mut window = Instant::now();
        let mut bytes = 0;
        loop {
            let incoming =
                tokio::select! {biased;_=reader_cancel.cancelled()=>break,value=read.next()=>value};
            match incoming {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => (),
                Some(Ok(Message::Binary(data)))
                    if reader_ticket.microphone
                        && reader_ready.load(std::sync::atomic::Ordering::Acquire) =>
                {
                    if window.elapsed() > Duration::from_secs(1) {
                        window = Instant::now();
                        bytes = 0;
                    }
                    bytes += data.len();
                    if bytes > 128000 {
                        break;
                    }
                    let context = reader_context.as_ref().unwrap();
                    if reader_app
                        .core
                        .authorize_audio(
                            &reader_ticket.identity,
                            &reader_ticket.session,
                            true,
                            None,
                        )
                        .is_err()
                        || context.check().is_err()
                    {
                        break;
                    }
                    let accepted = tokio::select! {biased;_=reader_cancel.cancelled()=>break,result=tokio::time::timeout(Duration::from_secs(2),driver.send_audio(Arc::from(data.as_ref()),context))=>result};
                    if !matches!(accepted, Ok(Ok(()))) {
                        break;
                    }
                }
                _ => break,
            }
        }
    });
    let monitor_app = app.clone();
    let monitor_ticket = ticket.clone();
    let monitor_cancel = cancel.clone();
    let monitor_context = context.clone();
    let monitor = tokio::spawn(async move {
        loop {
            tokio::select! {biased;_=monitor_cancel.cancelled()=>break,_=tokio::time::sleep(Duration::from_millis(250))=>()};
            if monitor_app
                .core
                .authorize_audio(
                    &monitor_ticket.identity,
                    &monitor_ticket.session,
                    monitor_ticket.microphone,
                    None,
                )
                .is_err()
                || monitor_context.as_ref().is_some_and(|c| c.check().is_err())
            {
                monitor_cancel.cancel();
                break;
            }
        }
    });
    let result = async {
        if ticket.microphone {
            tokio::time::timeout(
                Duration::from_secs(6),
                runtime.driver.set_microphone(true, context.clone()),
            )
            .await
            .map_err(|_| GatewayError::new("AUDIO_PROBE_TIMEOUT"))??;
        }
        if cancel.is_cancelled() {
            return Err(GatewayError::new("AUTH_FAILED"));
        }
        let mut stream = tokio::time::timeout(
            Duration::from_secs(6),
            runtime.driver.subscribe_audio(cancel.clone()),
        )
        .await
        .map_err(|_| GatewayError::new("AUDIO_PROBE_TIMEOUT"))??;
        ready.store(true, std::sync::atomic::Ordering::Release);
        send(
            &mut write,
            Message::Text("{\"ready\":true}".into()),
            &cancel,
        )
        .await?;
        loop {
            let chunk = tokio::select! {biased;
                _ = cancel.cancelled() => return Ok(()), chunk = stream.next() => chunk
            };
            let chunk = chunk.ok_or_else(|| GatewayError::new("AUDIO_DISCONNECTED"))??;
            app.core
                .authorize_audio(&ticket.identity, &ticket.session, ticket.microphone, None)?;
            if chunk.format != "pcm_s16le"
                || chunk.sample_rate != 48000
                || chunk.channels != 2
                || chunk.data.is_empty()
                || chunk.data.len() > 23040
                || !chunk.data.len().is_multiple_of(4)
            {
                return Err(GatewayError::new("AUDIO_UNAVAILABLE"));
            }
            if chunk.discontinuity {
                send(&mut write, Message::Text("{\"gap\":true}".into()), &cancel).await?;
            }
            send(
                &mut write,
                Message::Binary(Bytes::from_owner(chunk.data)),
                &cancel,
            )
            .await?;
        }
    }
    .await;
    cancel.cancel();
    let _ = reader.await;
    let _ = monitor.await;
    if let Some(mic) = mic.as_mut() {
        mic.finish().await;
    }
    let reason = if result.is_err() {
        close(1011, "Audio source unavailable")
    } else {
        close(1008, "Authorization expired")
    };
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        write.send(Message::Close(Some(reason))),
    )
    .await;
    drop(lease);
}
async fn reject<S>(ws: &mut WebSocketStream<S>, reason: &'static str)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let _ = tokio::time::timeout(Duration::from_secs(1), ws.close(Some(close(1008, reason)))).await;
}
fn close(code: u16, reason: &'static str) -> tokio_tungstenite::tungstenite::protocol::CloseFrame {
    tokio_tungstenite::tungstenite::protocol::CloseFrame {
        code: code.into(),
        reason: reason.into(),
    }
}
async fn send<S>(sink: &mut S, message: Message, cancel: &CancellationToken) -> Result<()>
where
    S: futures_util::Sink<Message> + Unpin,
{
    tokio::select! {biased;_=cancel.cancelled()=>Err(GatewayError::new("AUTH_FAILED")),result=tokio::time::timeout(Duration::from_secs(2),sink.send(message))=>result.map_err(|_|GatewayError::new("RESOURCE_LIMIT"))?.map_err(|_|GatewayError::new("AUDIO_DISCONNECTED"))}
}
struct AudioLease {
    state: std::sync::Weak<State>,
    token: String,
    media: Arc<crate::media::Media>,
    consumer: String,
}
impl Drop for AudioLease {
    fn drop(&mut self) {
        self.media.release(&self.consumer);
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
struct MicrophoneLease {
    errors: std::sync::Weak<Core>,
    registry: Arc<Mutex<HashMap<String, String>>>,
    driver: Arc<dyn KvmDriver>,
    device: String,
    owner: String,
    context: CancellationToken,
    armed: bool,
}
impl MicrophoneLease {
    async fn finish(&mut self) {
        self.context.cancel();
        let stopped = tokio::time::timeout(
            Duration::from_secs(6),
            self.driver.set_microphone(false, None),
        )
        .await;
        if !matches!(stopped, Ok(Ok(())))
            && let Some(core) = self.errors.upgrade()
        {
            core.record_error("AUDIO_STOP_FAILED");
        }
        self.remove();
        self.armed = false;
    }
    fn remove(&self) {
        let mut registry = self.registry.lock().unwrap();
        if registry.get(&self.device) == Some(&self.owner) {
            registry.remove(&self.device);
        }
    }
}
impl Drop for MicrophoneLease {
    fn drop(&mut self) {
        self.context.cancel();
        if self.armed {
            let registry = self.registry.clone();
            let driver = self.driver.clone();
            let device = self.device.clone();
            let owner = self.owner.clone();
            let errors = self.errors.clone();
            tokio::spawn(async move {
                let stopped = tokio::time::timeout(
                    Duration::from_secs(6),
                    driver.set_microphone(false, None),
                )
                .await;
                if !matches!(stopped, Ok(Ok(())))
                    && let Some(core) = errors.upgrade()
                {
                    core.record_error("AUDIO_STOP_FAILED");
                }
                let mut registry = registry.lock().unwrap();
                if registry.get(&device) == Some(&owner) {
                    registry.remove(&device);
                }
            });
        }
    }
}
