//! PiKVM KVMD HID and video. Comet uses this protocol only through its explicit
//! adapter below. Janus audio owns a separate, independently retrying lifetime.
use super::{
    ActionContext, Delivery, DriverEvent, KvmDriver, VideoStream,
    frames::{self, Mjpeg},
    janus::JanusAudio,
    network_hid::{Hid, HidTransport},
};
use crate::{
    GatewayError, Result,
    contracts::{Action, Capabilities, Device, Signal},
    keyboard::validate_hid,
    media::DriverFrame,
    transport::{LanTransport, Socket},
};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt, stream};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore, broadcast, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

const SEND_TIMEOUT: Duration = Duration::from_secs(5);
struct Outbound {
    text: String,
    context: Option<ActionContext>,
    done: oneshot::Sender<Result<()>>,
    _bytes: OwnedSemaphorePermit,
}
struct Link {
    sender: mpsc::Sender<Outbound>,
    bytes: Arc<Semaphore>,
    cancel: CancellationToken,
    worker: JoinHandle<()>,
}
struct Inner {
    capabilities: Mutex<Capabilities>,
    events: broadcast::Sender<DriverEvent>,
    epoch: AtomicU64,
}
pub struct PiKvmDriver {
    device: Device,
    transport: Arc<LanTransport>,
    inner: Arc<Inner>,
    link: Mutex<Option<Link>>,
    lifecycle: AsyncMutex<()>,
    hid: Hid,
    audio: Mutex<Option<Arc<JanusAudio>>>,
}
fn headers(secret: &Value) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        (
            "x-kvmd-user",
            secret
                .get("username")
                .and_then(Value::as_str)
                .unwrap_or("admin"),
        ),
        (
            "x-kvmd-passwd",
            secret.get("password").and_then(Value::as_str).unwrap_or(""),
        ),
    ] {
        let mut value = HeaderValue::from_str(value).map_err(|_| GatewayError::invalid())?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    Ok(headers)
}
impl PiKvmDriver {
    pub fn new(device: Device, secret: Value, ca: Option<String>) -> Result<Self> {
        let transport = LanTransport::new(
            device
                .address
                .as_deref()
                .ok_or_else(|| GatewayError::new("INVALID_ADDRESS"))?,
            headers(&secret)?,
            &device.tls,
            ca.as_deref(),
        )?;
        Self::with_transport(device, transport)
    }
    pub fn with_transport(device: Device, transport: Arc<LanTransport>) -> Result<Self> {
        let capabilities: Capabilities = serde_json::from_value(json!({
            "video":{"snapshot":true,"stream":true,"formats":["image/jpeg"],"signal":"unknown"},
            "mouse":{"absolute":false,"relative":false,"buttons":["left","middle","right"],"horizontal_scroll":true},
            "keyboard":{"hid":false,"layouts":["us","de"],"text_method":"hid-chords/v1","characters":"US / DE mapping v1"},
            "audio":{"from_target":false,"to_target":false,"formats":[],"reason":"Audio unavailable"},
            "timebase":{"source_timestamp":false,"uncertainty_ms":null},"extensions":{}
        })).map_err(|_|GatewayError::invalid())?;
        let (events, _) = broadcast::channel(16);
        Ok(Self {
            device,
            transport,
            inner: Arc::new(Inner {
                capabilities: Mutex::new(capabilities),
                events,
                epoch: AtomicU64::new(0),
            }),
            link: Mutex::new(None),
            lifecycle: AsyncMutex::new(()),
            hid: Hid::default(),
            audio: Mutex::new(None),
        })
    }
    async fn disconnect_link(&self) {
        self.inner.epoch.fetch_add(1, Ordering::AcqRel);
        let audio = self.audio.lock().unwrap().take();
        if let Some(audio) = audio {
            audio.close().await;
        }
        let link = self.link.lock().unwrap().take();
        if let Some(link) = link {
            link.cancel.cancel();
            let mut worker = link.worker;
            if tokio::time::timeout(SEND_TIMEOUT, &mut worker)
                .await
                .is_err()
            {
                worker.abort();
                let _ = worker.await;
            }
        }
        self.transport.close();
    }
    async fn json(&self, path: &str, cancel: &CancellationToken) -> Result<Value> {
        let value = self.transport.get(path, cancel).await?.json().await?;
        value
            .get("result")
            .cloned()
            .ok_or_else(|| GatewayError::message("DEVICE_OFFLINE", "Invalid KVMD response"))
    }
    async fn connect_inner(&self, cancel: &CancellationToken) -> Result<()> {
        self.json("/api/auth/check", cancel).await?;
        let hid = self.json("/api/hid", cancel).await?;
        {
            let mut caps = self.inner.capabilities.lock().unwrap();
            caps.mouse.absolute = hid["mouse"]["absolute"] == true;
            caps.mouse.relative = !caps.mouse.absolute;
            caps.keyboard.hid = hid["keyboard"]["online"] == true;
            caps.video.signal = Signal::Unknown;
            caps.audio.from_target = false;
            caps.audio.to_target = false;
            caps.audio.formats.clear();
            caps.audio.reason = Some("Audio unavailable".into());
        }
        let ws = self.transport.websocket("/api/ws", cancel).await?;
        let epoch = self.inner.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let inner = self.inner.clone();
        let cancellation = CancellationToken::new();
        // A cancelled connection future must not leave its persistent worker
        // alive; a successful connection explicitly transfers ownership to Link.
        let guard = cancellation.clone().drop_guard();
        let lifetime = cancellation.clone();
        let (sender, receiver) = mpsc::channel(64);
        let worker = tokio::spawn(async move {
            run_socket(ws, receiver, inner, epoch, lifetime).await;
        });
        *self.link.lock().unwrap() = Some(Link {
            sender,
            bytes: Arc::new(Semaphore::new(65536)),
            cancel: cancellation,
            worker,
        });
        tokio::select! {biased;
            _=cancel.cancelled()=>return Err(GatewayError::new("DEVICE_OFFLINE")),
            result=self.release_all_inputs()=>result?,
        }
        guard.disarm();
        let inner = self.inner.clone();
        let audio = JanusAudio::start(
            self.transport.clone(),
            self.device.device_id.clone().unwrap_or_default(),
            Arc::new(move |caps| {
                let mut current = inner.capabilities.lock().unwrap();
                if inner.epoch.load(Ordering::Acquire) == epoch {
                    current.audio = caps;
                    let _ = inner.events.send(DriverEvent::CapabilitiesChangedAt(epoch));
                }
            }),
        );
        *self.audio.lock().unwrap() = Some(audio);
        Ok(())
    }
}

fn apply_message(inner: &Inner, epoch: u64, message: &str, last_pong: &mut tokio::time::Instant) {
    let Ok(value) = serde_json::from_str::<Value>(message) else {
        return;
    };
    let mut caps = inner.capabilities.lock().unwrap();
    if inner.epoch.load(Ordering::Acquire) != epoch {
        return;
    }
    let before = caps.clone();
    match value["event_type"].as_str() {
        Some("pong") => *last_pong = tokio::time::Instant::now(),
        Some("hid") => {
            caps.keyboard.hid = value["event"]["keyboard"]["online"] == true;
            caps.mouse.absolute = value["event"]["mouse"]["absolute"] == true;
            caps.mouse.relative = !caps.mouse.absolute;
        }
        Some("streamer") => {
            if let Some(online) = value["event"]["streamer"]["source"]["online"].as_bool() {
                caps.video.signal = if online {
                    Signal::Present
                } else {
                    Signal::Absent
                };
            }
        }
        _ => (),
    }
    if *caps != before {
        let _ = inner.events.send(DriverEvent::CapabilitiesChangedAt(epoch));
    }
}
async fn send(ws: &mut Socket, message: Message, cancel: &CancellationToken) -> Result<()> {
    tokio::select! {biased;
        _=cancel.cancelled()=>Err(GatewayError::new("DEVICE_OFFLINE")),
        result=tokio::time::timeout(SEND_TIMEOUT,ws.send(message))=>result
            .map_err(|_|GatewayError::new("EXECUTION_UNKNOWN"))?
            .map_err(|_|GatewayError::new("EXECUTION_UNKNOWN")),
    }
}
async fn run_socket(
    mut ws: Socket,
    mut receiver: mpsc::Receiver<Outbound>,
    inner: Arc<Inner>,
    epoch: u64,
    cancel: CancellationToken,
) {
    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat.tick().await;
    let mut last_pong = tokio::time::Instant::now();
    loop {
        // Fair selection keeps inbound capability traffic from starving HID
        // writes or heartbeat handling. Each write still checks cancellation.
        tokio::select! {
            _=cancel.cancelled()=>break,
            _=heartbeat.tick()=>{
                if last_pong.elapsed() > Duration::from_secs(30)
                    || send(&mut ws,Message::Text(json!({"event_type":"ping","event":{}}).to_string().into()),&cancel).await.is_err() { break; }
            },
            incoming=ws.next()=>match incoming {
                Some(Ok(Message::Text(text)))=>{
                    if inner.epoch.load(Ordering::Acquire) != epoch { break; }
                    apply_message(&inner,epoch,&text,&mut last_pong);
                },
                Some(Ok(Message::Ping(_)))=>{
                    if !matches!(tokio::time::timeout(SEND_TIMEOUT,ws.flush()).await,Ok(Ok(()))) { break; }
                },
                Some(Ok(Message::Close(_)))|Some(Err(_))|None=>break,
                _=>(),
            },
            outgoing=receiver.recv()=>{
                let Some(outgoing) = outgoing else { break; };
                let checked = if inner.epoch.load(Ordering::Acquire) != epoch {Err(GatewayError::new("DEVICE_OFFLINE"))}
                    else {outgoing.context.as_ref().map_or(Ok(()),ActionContext::check)};
                if let Err(error) = checked { let _ = outgoing.done.send(Err(error)); continue; }
                let result = send(&mut ws,Message::Text(outgoing.text.into()),&cancel).await;
                let failed = result.is_err();
                let _ = outgoing.done.send(result);
                if failed { break; }
            },
        }
    }
    if !cancel.is_cancelled() && inner.epoch.load(Ordering::Acquire) == epoch {
        let _ = inner.events.send(DriverEvent::DisconnectedAt(epoch));
    }
    cancel.cancel();
    // Dropping the socket and receiver closes pending commands without replay.
}

#[async_trait]
impl HidTransport for PiKvmDriver {
    async fn event(&self, kind: &str, value: Value, context: Option<&ActionContext>) -> Result<()> {
        if let Some(context) = context {
            context.check()?;
        }
        let text = json!({"event_type":kind,"event":value}).to_string();
        let (sender, bytes, cancel) = self
            .link
            .lock()
            .unwrap()
            .as_ref()
            .filter(|link| !link.cancel.is_cancelled())
            .map(|link| (link.sender.clone(), link.bytes.clone(), link.cancel.clone()))
            .ok_or_else(|| GatewayError::new("DEVICE_OFFLINE"))?;
        let byte_count =
            u32::try_from(text.len()).map_err(|_| GatewayError::new("RESOURCE_LIMIT"))?;
        let permit = bytes
            .try_acquire_many_owned(byte_count)
            .map_err(|_| GatewayError::message("RESOURCE_LIMIT", "HID transport congested"))?;
        let (done, wait) = oneshot::channel();
        sender
            .try_send(Outbound {
                text,
                context: context.cloned(),
                done,
                _bytes: permit,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => GatewayError::new("RESOURCE_LIMIT"),
                _ => GatewayError::new("DEVICE_OFFLINE"),
            })?;
        tokio::select! {biased;
            _=cancel.cancelled()=>Err(GatewayError::new("EXECUTION_UNKNOWN")),
            result=wait=>result.unwrap_or_else(|_|Err(GatewayError::new("EXECUTION_UNKNOWN"))),
        }
    }
}
#[async_trait]
impl KvmDriver for PiKvmDriver {
    async fn subscribe_audio(&self, cancel: CancellationToken) -> Result<super::AudioStream> {
        let audio = self
            .audio
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| GatewayError::new("UNSUPPORTED_ACTION"))?;
        audio.subscribe(cancel).await
    }
    async fn set_microphone(&self, enabled: bool, context: Option<ActionContext>) -> Result<()> {
        let audio = self.audio.lock().unwrap().clone();
        match audio {
            Some(audio) => audio.microphone(enabled, context).await,
            None if !enabled => Ok(()),
            _ => Err(GatewayError::new("UNSUPPORTED_ACTION")),
        }
    }
    async fn send_audio(&self, data: Arc<[u8]>, context: &ActionContext) -> Result<()> {
        let audio = self
            .audio
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| GatewayError::new("UNSUPPORTED_ACTION"))?;
        audio.send(data, context)
    }
    fn connection_epoch(&self) -> u64 {
        self.inner.epoch.load(Ordering::Acquire)
    }
    fn events(&self) -> broadcast::Receiver<DriverEvent> {
        self.inner.events.subscribe()
    }
    async fn connect(&self, cancellation: CancellationToken) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        self.disconnect_link().await;
        let result = self.connect_inner(&cancellation).await;
        if result.is_err() {
            self.disconnect_link().await;
        }
        result
    }
    async fn disconnect(&self) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        self.disconnect_link().await;
        Ok(())
    }
    async fn capabilities(&self) -> Result<Capabilities> {
        Ok(self.inner.capabilities.lock().unwrap().clone())
    }
    async fn snapshot(&self, cancellation: CancellationToken) -> Result<DriverFrame> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        let body = loop {
            match self
                .transport
                .get("/api/streamer/snapshot", &cancellation)
                .await
            {
                Ok(body) => break body,
                Err(error) if error.details.get("http_status") == Some(&json!(503)) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(GatewayError::message(
                            "FRAME_TIMEOUT",
                            "KVMD snapshot source is not ready (HTTP 503)",
                        ));
                    }
                    tokio::select! {biased;_=cancellation.cancelled()=>return Err(GatewayError::new("FRAME_TIMEOUT")),_=tokio::time::sleep(Duration::from_millis(100))=>()};
                }
                Err(error) => return Err(error),
            }
        };
        let data = body.bytes(frames::FRAME_BYTES).await?;
        let signal = self.inner.capabilities.lock().unwrap().video.signal;
        frames::inspect(data.into(), signal).await
    }
    async fn subscribe_video(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Option<VideoStream>> {
        let body = self
            .transport
            .get("/streamer/stream", &cancellation)
            .await?;
        let inner = self.inner.clone();
        Ok(Some(Box::pin(stream::try_unfold(
            (body, Mjpeg::default(), inner),
            |(mut body, mut parser, inner)| async move {
                let frame = tokio::time::timeout(SEND_TIMEOUT, async {
                    loop {
                        if let Some(data) = parser.next_frame()? {
                            let signal = inner.capabilities.lock().unwrap().video.signal;
                            break frames::inspect(data, signal).await;
                        }
                        match body.chunk().await? {
                            Some(bytes) => parser.push(&bytes)?,
                            None => return Err(GatewayError::new("DEVICE_OFFLINE")),
                        }
                    }
                })
                .await
                .unwrap_or_else(|_| Err(GatewayError::new("FRAME_TIMEOUT")))?;
                Ok(Some((frame, (body, parser, inner))))
            },
        ))))
    }
    fn validate(&self, actions: &[Action], width: u32, height: u32) -> Result<()> {
        let caps = self.inner.capabilities.lock().unwrap();
        validate_hid(
            actions,
            width,
            height,
            caps.mouse.absolute,
            caps.keyboard.hid,
            self.device.keyboard_layout,
        )
    }
    async fn execute(&self, action: &Action, context: &ActionContext) -> Result<Delivery> {
        self.validate(std::slice::from_ref(action), context.width, context.height)?;
        self.hid
            .execute(self, action, context, self.device.keyboard_layout)
            .await
    }
    async fn release_all_inputs(&self) -> Result<()> {
        if let Some(audio) = self.audio.lock().unwrap().as_ref() {
            audio.stop_microphone();
        }
        let connected = self
            .link
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|link| !link.cancel.is_cancelled());
        if !connected {
            return if self.hid.has_pressed() {
                Err(GatewayError::message(
                    "EXECUTION_UNKNOWN",
                    "Input release could not be confirmed",
                ))
            } else {
                Ok(())
            };
        }
        self.hid.release(self).await
    }
}
impl Drop for PiKvmDriver {
    fn drop(&mut self) {
        self.inner.epoch.fetch_add(1, Ordering::AcqRel);
        if let Some(link) = self.link.lock().unwrap().take() {
            link.cancel.cancel();
        }
        self.transport.close();
    }
}

/// GL.iNet identity and firmware translation remain explicit, as in the source.
pub struct CometDriver(PiKvmDriver);
impl CometDriver {
    pub fn new(device: Device, mut secret: Value, ca: Option<String>) -> Result<Self> {
        if secret
            .get("username")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            if !secret.is_object() {
                secret = json!({});
            }
            secret["username"] = "admin".into();
        }
        let driver = PiKvmDriver::new(device.clone(), secret, ca)?;
        driver.inner.capabilities.lock().unwrap().extensions =
            json!({"vendor":"GL.iNet","model":device.model,"protocol":"glkvm-kvmd"})
                .as_object()
                .unwrap()
                .clone();
        Ok(Self(driver))
    }
}
#[async_trait]
impl KvmDriver for CometDriver {
    async fn subscribe_audio(&self, cancel: CancellationToken) -> Result<super::AudioStream> {
        self.0.subscribe_audio(cancel).await
    }
    async fn set_microphone(&self, enabled: bool, context: Option<ActionContext>) -> Result<()> {
        self.0.set_microphone(enabled, context).await
    }
    async fn send_audio(&self, data: Arc<[u8]>, context: &ActionContext) -> Result<()> {
        self.0.send_audio(data, context).await
    }
    fn connection_epoch(&self) -> u64 {
        self.0.connection_epoch()
    }
    fn events(&self) -> broadcast::Receiver<DriverEvent> {
        self.0.events()
    }
    async fn connect(&self, cancellation: CancellationToken) -> Result<()> {
        self.0.connect(cancellation).await.map_err(|error| {
            if error.code == "DEVICE_OFFLINE"
                && error
                    .details
                    .get("http_status")
                    .is_some_and(|v| v == 404 || v == 405)
            {
                GatewayError::message(
                    "UNSUPPORTED_FIRMWARE",
                    "This Comet firmware does not expose the required local KVMD API.",
                )
            } else {
                error
            }
        })
    }
    async fn disconnect(&self) -> Result<()> {
        self.0.disconnect().await
    }
    async fn capabilities(&self) -> Result<Capabilities> {
        self.0.capabilities().await
    }
    async fn snapshot(&self, cancellation: CancellationToken) -> Result<DriverFrame> {
        self.0.snapshot(cancellation).await
    }
    async fn subscribe_video(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Option<VideoStream>> {
        self.0.subscribe_video(cancellation).await
    }
    fn validate(&self, actions: &[Action], width: u32, height: u32) -> Result<()> {
        self.0.validate(actions, width, height)
    }
    async fn execute(&self, action: &Action, context: &ActionContext) -> Result<Delivery> {
        self.0.execute(action, context).await
    }
    async fn release_all_inputs(&self) -> Result<()> {
        self.0.release_all_inputs().await
    }
}
