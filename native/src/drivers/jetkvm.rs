//! JetKVM's own login/signaling and USB JSON-RPC protocol. Only the HID action
//! planner and the LAN/TLS transport are shared with KVMD adapters.
use super::{
    ActionContext, Delivery, DriverEvent, KvmDriver, VideoStream,
    jet_peer::Peer,
    network_hid::{Hid, HidTransport},
};
use crate::{
    GatewayError, Result,
    contracts::{Action, Capabilities, Device, Signal},
    keyboard::{text_keys, usb_key, validate_hid},
    media::DriverFrame,
    transport::LanTransport,
};
use async_trait::async_trait;
use futures_util::stream;
use reqwest::{
    Method,
    header::{CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, SET_COOKIE},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::{Mutex as AsyncMutex, broadcast};
use tokio_util::sync::CancellationToken;

struct Inner {
    caps: Mutex<Capabilities>,
    events: broadcast::Sender<DriverEvent>,
    epoch: AtomicU64,
}
#[derive(Default)]
struct Reports {
    keys: Vec<u8>,
    x: i64,
    y: i64,
    buttons: u8,
}
pub struct JetKvmDriver {
    device: Device,
    password: String,
    transport: Arc<LanTransport>,
    cookie: Mutex<Option<HeaderValue>>,
    inner: Arc<Inner>,
    peer: Mutex<Option<Arc<Peer>>>,
    lifecycle: AsyncMutex<()>,
    reports: Mutex<Reports>,
    hid: Hid,
}
impl JetKvmDriver {
    pub fn new(device: Device, secret: Value, ca: Option<String>) -> Result<Self> {
        let transport = LanTransport::new(
            device
                .address
                .as_deref()
                .ok_or_else(|| GatewayError::new("INVALID_ADDRESS"))?,
            HeaderMap::new(),
            &device.tls,
            ca.as_deref(),
        )?;
        Self::with_transport(device, secret, transport)
    }
    pub fn with_transport(
        device: Device,
        secret: Value,
        transport: Arc<LanTransport>,
    ) -> Result<Self> {
        let caps:Capabilities = serde_json::from_value(json!({
            "video":{"snapshot":true,"stream":true,"formats":["image/jpeg"],"signal":"unknown"},
            "mouse":{"absolute":false,"relative":false,"buttons":["left","middle","right"],"horizontal_scroll":true},
            "keyboard":{"hid":false,"layouts":["us","de"],"text_method":"hid-chords/v1","characters":"US / DE mapping v1"},
            "audio":{"from_target":false,"to_target":false,"formats":[],"reason":"Native JetKVM Opus/PCM integration pending; microphone transmission is unavailable"},
            "timebase":{"source_timestamp":false,"uncertainty_ms":null},
            "extensions":{"vendor":"JetKVM","protocol":"webrtc-jsonrpc","decoder":"ffmpeg-h264"}
        })).map_err(|_|GatewayError::invalid())?;
        let (events, _) = broadcast::channel(16);
        Ok(Self {
            device,
            password: secret["password"].as_str().unwrap_or("").into(),
            transport,
            cookie: Mutex::new(None),
            inner: Arc::new(Inner {
                caps: Mutex::new(caps),
                events,
                epoch: AtomicU64::new(0),
            }),
            peer: Mutex::new(None),
            lifecycle: AsyncMutex::new(()),
            reports: Mutex::new(Reports::default()),
            hid: Hid::default(),
        })
    }
    fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(cookie) = self.cookie.lock().unwrap().clone() {
            headers.insert(COOKIE, cookie);
        }
        headers
    }
    fn peer(&self) -> Result<Arc<Peer>> {
        self.peer
            .lock()
            .unwrap()
            .as_ref()
            .filter(|p| !p.cancel.is_cancelled())
            .cloned()
            .ok_or_else(|| GatewayError::new("DEVICE_OFFLINE"))
    }
    async fn rpc(
        &self,
        method: &'static str,
        params: Value,
        context: Option<&ActionContext>,
    ) -> Result<Value> {
        self.peer()?.call(method, params, context).await
    }
    async fn disconnect_inner(&self) {
        self.inner.epoch.fetch_add(1, Ordering::AcqRel);
        let peer = self.peer.lock().unwrap().take();
        if let Some(peer) = peer {
            peer.close().await;
        }
        self.transport.close();
    }
    async fn connect_inner(&self, caller: &CancellationToken) -> Result<()> {
        let status = self
            .transport
            .get("/device/status", caller)
            .await?
            .json()
            .await?;
        if status["isSetup"] == false {
            return Err(GatewayError::new("DEVICE_NOT_CONFIGURED"));
        }
        let authenticated = match self
            .transport
            .request(Method::GET, "/device", self.headers(), None, caller)
            .await
        {
            Ok(body) => {
                body.bytes(1024 * 1024).await?;
                true
            }
            Err(error) if error.code == "AUTH_FAILED" => false,
            Err(error) => return Err(error),
        };
        if !authenticated {
            let body = self
                .transport
                .request(
                    Method::POST,
                    "/auth/login-local",
                    self.headers(),
                    Some(json!({"password":self.password}).to_string().into_bytes()),
                    caller,
                )
                .await?;
            let cookie = body
                .headers()
                .get_all(SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .find(|v| v.starts_with("authToken="))
                .and_then(|v| v.split(';').next())
                .filter(|v| v.len() <= 4096)
                .ok_or_else(|| GatewayError::new("AUTH_FAILED"))?;
            let mut cookie =
                HeaderValue::from_str(cookie).map_err(|_| GatewayError::new("AUTH_FAILED"))?;
            cookie.set_sensitive(true);
            body.bytes(1024 * 1024).await?;
            *self.cookie.lock().unwrap() = Some(cookie);
        }
        let ws = self
            .transport
            .websocket_headers("/webrtc/signaling/client", self.headers(), caller)
            .await?;
        let epoch = self.inner.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        self.inner.caps.lock().unwrap().video.signal = Signal::Unknown;
        let inner = self.inner.clone();
        let disconnected_inner = inner.clone();
        let peer = Peer::start(
            ws,
            Arc::new(move |method, params| apply_event(&inner, epoch, method, params)),
            Arc::new(move || {
                if disconnected_inner.epoch.load(Ordering::Acquire) == epoch {
                    let _ = disconnected_inner
                        .events
                        .send(DriverEvent::DisconnectedAt(epoch));
                }
            }),
        )
        .await?;
        let guard = peer.cancel.clone().drop_guard();
        *self.peer.lock().unwrap() = Some(peer.clone());
        peer.wait_ready(caller).await?;
        let video = self.rpc("getVideoState", json!({}), None).await?;
        apply_event(&self.inner, epoch, "videoState", &video);
        let usb = self.rpc("getUSBState", json!({}), None).await?;
        apply_event(&self.inner, epoch, "usbState", &usb);
        self.release_all_inputs().await?;
        guard.disarm();
        Ok(())
    }
}
fn apply_event(inner: &Inner, epoch: u64, method: &str, params: &Value) {
    let mut caps = inner.caps.lock().unwrap();
    if inner.epoch.load(Ordering::Acquire) != epoch {
        return;
    }
    let before = caps.clone();
    match method {
        "videoState" => {
            caps.video.signal = match params["ready"].as_bool() {
                Some(true) => Signal::Present,
                Some(false) => Signal::Absent,
                None => Signal::Unknown,
            }
        }
        "usbState" => {
            caps.keyboard.hid = params == "configured";
            caps.mouse.absolute = caps.keyboard.hid;
            caps.mouse.relative = caps.keyboard.hid;
        }
        _ => (),
    }
    if *caps != before {
        let _ = inner.events.send(DriverEvent::CapabilitiesChangedAt(epoch));
    }
}
#[async_trait]
impl HidTransport for JetKvmDriver {
    async fn event(&self, kind: &str, value: Value, context: Option<&ActionContext>) -> Result<()> {
        if let Some(context) = context {
            context.check()?;
        }
        // Record state before any possibly uncertain send so safety cleanup can
        // release it. Keep insertion order, matching USB report order in TS.
        let (method, params) = {
            let mut reports = self.reports.lock().unwrap();
            match kind {
                "key" => {
                    let code = usb_key(value["key"].as_str().ok_or_else(GatewayError::invalid)?)?;
                    if value["state"] == true {
                        if !reports.keys.contains(&code) {
                            reports.keys.push(code);
                        }
                    } else {
                        reports.keys.retain(|key| *key != code);
                    }
                    let modifier = reports
                        .keys
                        .iter()
                        .filter(|k| **k >= 224)
                        .fold(0u8, |mask, key| mask | (1 << (key - 224)));
                    let keys = reports
                        .keys
                        .iter()
                        .filter(|k| **k < 224)
                        .copied()
                        .collect::<Vec<_>>();
                    if keys.len() > 6 {
                        return Err(GatewayError::message(
                            "UNSUPPORTED_ACTION",
                            "JetKVM supports six simultaneous non-modifier keys",
                        ));
                    }
                    ("keyboardReport", json!({"modifier":modifier,"keys":keys}))
                }
                "mouse_move" => {
                    let coordinate = |v: &Value| -> Result<i64> {
                        Ok(
                            ((v.as_i64().ok_or_else(GatewayError::invalid)? + 32768) as f64
                                * 32767.0
                                / 65535.0
                                + 0.5)
                                .floor() as i64,
                        )
                    };
                    reports.x = coordinate(&value["to"]["x"])?;
                    reports.y = coordinate(&value["to"]["y"])?;
                    (
                        "absMouseReport",
                        json!({"x":reports.x,"y":reports.y,"buttons":reports.buttons}),
                    )
                }
                "mouse_button" => {
                    let bit = match value["button"].as_str() {
                        Some("left") => 1,
                        Some("right") => 2,
                        Some("middle") => 4,
                        _ => return Err(GatewayError::invalid()),
                    };
                    if value["state"] == true {
                        reports.buttons |= bit;
                    } else {
                        reports.buttons &= !bit;
                    }
                    (
                        "absMouseReport",
                        json!({"x":reports.x,"y":reports.y,"buttons":reports.buttons}),
                    )
                }
                "mouse_wheel" => (
                    "wheelReport",
                    json!({"wheelX":value["delta"]["x"],"wheelY":value["delta"]["y"]}),
                ),
                _ => return Err(GatewayError::invalid()),
            }
        };
        self.rpc(method, params, context).await?;
        Ok(())
    }
}
#[async_trait]
impl KvmDriver for JetKvmDriver {
    fn connection_epoch(&self) -> u64 {
        self.inner.epoch.load(Ordering::Acquire)
    }
    fn events(&self) -> broadcast::Receiver<DriverEvent> {
        self.inner.events.subscribe()
    }
    async fn connect(&self, cancellation: CancellationToken) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        self.disconnect_inner().await;
        let result = tokio::select! {biased;
            _=cancellation.cancelled()=>Err(GatewayError::new("DEVICE_OFFLINE")),
            result=self.connect_inner(&cancellation)=>result,
        };
        if result.is_err() {
            self.disconnect_inner().await;
        }
        result
    }
    async fn disconnect(&self) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        self.disconnect_inner().await;
        Ok(())
    }
    async fn capabilities(&self) -> Result<Capabilities> {
        Ok(self.inner.caps.lock().unwrap().clone())
    }
    async fn snapshot(&self, cancellation: CancellationToken) -> Result<DriverFrame> {
        if self.inner.caps.lock().unwrap().video.signal == Signal::Absent {
            return Err(GatewayError::new("NO_SIGNAL"));
        }
        let peer = self.peer()?;
        let mut frame = peer.frame(&cancellation).await?;
        frame.signal = self.inner.caps.lock().unwrap().video.signal;
        Ok(frame)
    }
    async fn subscribe_video(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Option<VideoStream>> {
        let peer = self.peer()?;
        let inner = self.inner.clone();
        Ok(Some(Box::pin(stream::try_unfold(
            (peer, inner, cancellation),
            |(peer, inner, cancel)| async move {
                if cancel.is_cancelled() {
                    return Ok(None);
                }
                if inner.caps.lock().unwrap().video.signal == Signal::Absent {
                    return Err(GatewayError::new("NO_SIGNAL"));
                }
                let mut frame = peer.frame(&cancel).await?;
                frame.signal = inner.caps.lock().unwrap().video.signal;
                Ok(Some((frame, (peer, inner, cancel))))
            },
        ))))
    }
    fn validate(&self, actions: &[Action], width: u32, height: u32) -> Result<()> {
        let caps = self.inner.caps.lock().unwrap();
        validate_hid(
            actions,
            width,
            height,
            caps.mouse.absolute,
            caps.keyboard.hid,
            self.device.keyboard_layout,
        )?;
        for action in actions {
            match action {
                Action::Keypress { keys } => {
                    let codes = keys
                        .iter()
                        .map(|key| usb_key(key))
                        .collect::<Result<BTreeSet<_>>>()?;
                    if codes.iter().filter(|code| **code < 224).count() > 6 {
                        return Err(GatewayError::new("UNSUPPORTED_ACTION"));
                    }
                }
                Action::Type { text } => {
                    for chord in text_keys(text, self.device.keyboard_layout)? {
                        for key in chord {
                            usb_key(&key)?;
                        }
                    }
                }
                _ => (),
            }
        }
        Ok(())
    }
    async fn execute(&self, action: &Action, context: &ActionContext) -> Result<Delivery> {
        self.validate(std::slice::from_ref(action), context.width, context.height)?;
        self.hid
            .execute(self, action, context, self.device.keyboard_layout)
            .await?;
        Ok(Delivery::Sent)
    }
    async fn release_all_inputs(&self) -> Result<()> {
        let peer = match self.peer() {
            Ok(peer) => peer,
            Err(_) => {
                let reports = self.reports.lock().unwrap();
                return if self.hid.has_pressed() || !reports.keys.is_empty() || reports.buttons != 0
                {
                    Err(GatewayError::new("EXECUTION_UNKNOWN"))
                } else {
                    Ok(())
                };
            }
        };
        let keyboard = peer
            .call("keyboardReport", json!({"modifier":0,"keys":[]}), None)
            .await;
        let params = {
            let r = self.reports.lock().unwrap();
            json!({"x":r.x,"y":r.y,"buttons":0})
        };
        let mouse = peer.call("absMouseReport", params, None).await;
        keyboard.and(mouse)?;
        {
            let mut r = self.reports.lock().unwrap();
            r.keys.clear();
            r.buttons = 0;
        }
        self.hid.clear();
        Ok(())
    }
}
impl Drop for JetKvmDriver {
    fn drop(&mut self) {
        if let Some(peer) = self.peer.get_mut().unwrap().take() {
            peer.cancel.cancel();
        }
    }
}
