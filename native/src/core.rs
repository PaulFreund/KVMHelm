use crate::{
    GatewayError, Result,
    auth::{Auth, Identity, now_ms},
    contracts::{
        Action, Capabilities, Computer, ControlOperation, Device, Mode, ObservationMode,
        OpenComputer, Profile, Signal, ToolRequest, Transport, Validate,
    },
    drivers::{
        ActionContext, DriverEvent, KvmDriver,
        jetkvm::JetKvmDriver,
        pikvm::{CometDriver, PiKvmDriver},
        simulator::Simulator,
    },
    media::{Frame, FrameInfo, Media},
    network::device_origin,
    queue::DeviceQueue,
    secrets::Secrets,
    shared::{Flight, monotonic_ms, unknown},
    store::{Kind, Store},
};
use futures_util::{FutureExt, future::join_all};
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Mutex as AsyncMutex, broadcast},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ToolResult {
    pub body: Map<String, Value>,
    pub images: Vec<Frame>,
}
impl ToolResult {
    pub fn ok(value: Value) -> Self {
        let mut body = value.as_object().cloned().unwrap_or_default();
        body.insert("ok".into(), true.into());
        Self {
            body,
            images: Vec::new(),
        }
    }
    pub fn failure(error: GatewayError) -> Self {
        let mut details = error.details;
        details.insert("code".into(), error.code.into());
        details.insert("message".into(), error.message.into());
        Self {
            body: json!({"ok":false,"error":details})
                .as_object()
                .unwrap()
                .clone(),
            images: Vec::new(),
        }
    }
    pub fn success(&self) -> bool {
        self.body.get("ok") == Some(&Value::Bool(true))
    }
    pub fn metadata(&self) -> Value {
        let mut body = self.body.clone();
        if !self.images.is_empty() {
            body.insert(
                "frames".into(),
                json!(self.images.iter().map(|f| &f.info).collect::<Vec<_>>()),
            );
        } else {
            body.entry("frames").or_insert_with(|| json!([]));
        }
        Value::Object(body)
    }
}

#[derive(Clone)]
pub struct Limits {
    pub max_sessions: usize,
    pub replay_bytes: usize,
    pub max_open_clients: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_sessions: 128,
            replay_bytes: 64 * 1024 * 1024,
            max_open_clients: 1024,
        }
    }
}

struct JournalEntry {
    hash: [u8; 32],
    result: Arc<Flight<ToolResult>>,
}
#[derive(Default)]
struct Journal {
    entries: Mutex<HashMap<String, JournalEntry>>,
}
impl Journal {
    fn reserve(&self, id: &str, input: &Value) -> Result<(Arc<Flight<ToolResult>>, bool)> {
        // serde_json's default map order is sorted; all schema property names
        // are ASCII. This matches the original stable normalized-request hash.
        let bytes = serde_json::to_vec(input).map_err(|_| GatewayError::invalid())?;
        let hash: [u8; 32] = Sha256::digest(bytes).into();
        let mut entries = self.entries.lock().unwrap();
        if let Some(old) = entries.get(id) {
            return if old.hash == hash {
                Ok((old.result.clone(), false))
            } else {
                Err(GatewayError::new("REQUEST_CONFLICT"))
            };
        }
        if entries.len() >= 256 {
            return Err(GatewayError::message(
                "RESOURCE_LIMIT",
                "Request journal full; open a new session",
            ));
        }
        let result = Arc::new(Flight::new());
        entries.insert(
            id.into(),
            JournalEntry {
                hash,
                result: result.clone(),
            },
        );
        Ok((result, true))
    }
    fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
    fn contains(&self, id: &str) -> bool {
        self.entries.lock().unwrap().contains_key(id)
    }
}
struct Completion(Arc<Flight<ToolResult>>);
impl Drop for Completion {
    fn drop(&mut self) {
        self.0.finish(Ok(ToolResult::failure(unknown())));
    }
}
#[derive(Default)]
struct Replay {
    entries: VecDeque<(Weak<Flight<ToolResult>>, usize)>,
    bytes: usize,
}
impl Replay {
    fn retain(&mut self, entry: &Arc<Flight<ToolResult>>, size: usize, limit: usize) {
        self.entries.retain(|(old, size)| {
            if old.strong_count() == 0 {
                self.bytes = self.bytes.saturating_sub(*size);
                false
            } else {
                true
            }
        });
        if size == 0 {
            return;
        }
        self.bytes += size;
        self.entries.push_back((Arc::downgrade(entry), size));
        // As in the original, retain the newest result (at most four bounded
        // frames) even when an administrator configures a smaller replay budget.
        while self.bytes > limit && self.entries.len() > 1 {
            let (old, size) = self.entries.pop_front().unwrap();
            self.bytes = self.bytes.saturating_sub(size);
            if let Some(old) = old.upgrade()
                && let Some(Ok(result)) = old.value.lock().unwrap().as_mut()
            {
                result.body.insert(
                    "frames".into(),
                    json!(result.images.iter().map(|f| &f.info).collect::<Vec<_>>()),
                );
                result.images.clear();
                result
                    .body
                    .insert("replay_image_evicted".into(), true.into());
            }
        }
    }
}

#[derive(Clone, Serialize)]
pub struct Lease {
    pub session_id: String,
    pub client_id: String,
    pub expires_at: u64,
    pub generation: u64,
}
struct SessionState {
    mode: Mode,
    expires_at: u64,
    connection_generation: u64,
    references: VecDeque<FrameInfo>,
}
pub struct Session {
    pub session_id: String,
    pub device_id: String,
    pub identity: Identity,
    state: Mutex<SessionState>,
    journal: Journal,
}
impl Session {
    pub fn view(&self) -> Value {
        let s = self.state.lock().unwrap();
        json!({"session_id":self.session_id,"device_id":self.device_id,"owner_id":"owner","client_id":self.identity.client_id,"mode":s.mode,"expires_at":s.expires_at,"connection_generation":s.connection_generation,"journal_remaining":256-self.journal.len()})
    }
}
struct RuntimeState {
    status: String,
    error: Option<String>,
    caps: Option<Capabilities>,
    lease: Option<Lease>,
    suspended: bool,
    generation: u64,
    abort: Option<CancellationToken>,
    releasing: Option<Arc<Flight<()>>>,
    retry_at: f64,
    failures: u32,
    last_input: Option<f64>,
}
pub struct Runtime {
    pub device: Device,
    pub driver: Arc<dyn KvmDriver>,
    pub media: Arc<Media>,
    state: Mutex<RuntimeState>,
    queue: DeviceQueue,
    connecting: AsyncMutex<()>,
    closing: AtomicBool,
    lifetime: CancellationToken,
}
impl Runtime {
    pub fn lease(&self) -> Option<Lease> {
        self.state.lock().unwrap().lease.clone()
    }
    pub fn suspended(&self) -> bool {
        self.state.lock().unwrap().suspended
    }
    pub fn pending(&self) -> usize {
        self.queue.pending()
    }
    pub fn last_input_completed_at(&self) -> Option<f64> {
        self.state.lock().unwrap().last_input
    }
    pub fn describe(&self) -> Value {
        let mut s = self.state.lock().unwrap();
        if matches!(s.status.as_str(), "ready" | "no_signal")
            && let Some(frame) = self.media.latest()
        {
            s.status = if frame.info.signal == Signal::Absent {
                "no_signal"
            } else {
                "ready"
            }
            .into();
        }
        let mut value = serde_json::to_value(&self.device).unwrap();
        let out = value.as_object_mut().unwrap();
        out.insert("status".into(), s.status.clone().into());
        if let Some(error) = &s.error {
            out.insert("error".into(), error.clone().into());
        }
        out.insert("capabilities".into(), json!(s.caps));
        out.insert("lease".into(),s.lease.as_ref().map(|l|json!({"session_id":l.session_id,"client_id":l.client_id,"expires_at":l.expires_at})).unwrap_or(Value::Null));
        out.insert("control_suspended".into(), s.suspended.into());
        out.insert("consumers".into(), self.media.consumers());
        out.insert("media".into(), self.media.describe());
        value
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.lifetime.cancel();
    }
}

pub type DriverFactory =
    Arc<dyn Fn(Device, Value, Option<String>) -> Result<Arc<dyn KvmDriver>> + Send + Sync>;
#[derive(Clone, Serialize)]
pub struct GatewayEvent {
    pub event_id: u64,
    pub instance_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    pub at: u64,
    pub data: Value,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub historical: bool,
}
#[derive(Default)]
struct State {
    devices: IndexMap<String, Arc<Runtime>>,
    sessions: HashMap<String, Arc<Session>>,
    opening: HashMap<String, usize>,
    opens: HashMap<(String, String), Arc<Journal>>,
    errors: BTreeMap<String, u64>,
    dispatch_ms: VecDeque<f64>,
    batches: u64,
    lag: VecDeque<f64>,
}
struct EventLog {
    sequence: u64,
    events: VecDeque<GatewayEvent>,
    sender: broadcast::Sender<GatewayEvent>,
}
pub struct Core {
    pub store: Store,
    pub secrets: Secrets,
    pub auth: Arc<Auth>,
    pub instance_id: String,
    pub limits: Limits,
    pub lease_ms: u64,
    pub(crate) audio_microphones: Arc<Mutex<HashMap<String, String>>>,
    plugins: OnceLock<Arc<crate::plugins::Host>>,
    state: Mutex<State>,
    factories: Mutex<HashMap<String, DriverFactory>>,
    configuration: DeviceQueue,
    events: Mutex<EventLog>,
    replay: Mutex<Replay>,
    lifetime: CancellationToken,
    closing: AtomicBool,
    workers: Mutex<Vec<JoinHandle<()>>>,
}
struct Reservation {
    core: Weak<Core>,
    token: String,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(core) = self.core.upgrade() {
            let mut s = core.state.lock().unwrap();
            if let Some(count) = s.opening.get_mut(&self.token) {
                *count -= 1;
                if *count == 0 {
                    s.opening.remove(&self.token);
                }
            }
        }
    }
}
struct InputFinished(Arc<Runtime>);
impl Drop for InputFinished {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().last_input = Some(monotonic_ms());
    }
}

impl Core {
    pub fn new(
        store: Store,
        secrets: Secrets,
        auth: Arc<Auth>,
        limits: Limits,
        lease_ms: u64,
    ) -> Result<Arc<Self>> {
        let (sender, _) = broadcast::channel(256);
        let core = Arc::new(Self {
            store,
            secrets,
            auth,
            instance_id: uuid::Uuid::new_v4().to_string(),
            limits,
            lease_ms,
            audio_microphones: Arc::new(Mutex::new(HashMap::new())),
            plugins: OnceLock::new(),
            state: Mutex::new(State::default()),
            factories: Mutex::new(HashMap::new()),
            configuration: DeviceQueue::new(),
            events: Mutex::new(EventLog {
                sequence: 0,
                events: VecDeque::new(),
                sender,
            }),
            replay: Mutex::new(Replay::default()),
            lifetime: CancellationToken::new(),
            closing: AtomicBool::new(false),
            workers: Mutex::new(Vec::new()),
        });
        core.register_factory(
            "simulator",
            Arc::new(|device, _, _| Ok(Arc::new(Simulator::new(device.keyboard_layout)))),
        );
        core.register_factory(
            "pikvm-v4",
            Arc::new(|device, secret, ca| Ok(Arc::new(PiKvmDriver::new(device, secret, ca)?))),
        );
        core.register_factory(
            "glinet-comet",
            Arc::new(|device, secret, ca| Ok(Arc::new(CometDriver::new(device, secret, ca)?))),
        );
        core.register_factory(
            "jetkvm",
            Arc::new(|device, secret, ca| Ok(Arc::new(JetKvmDriver::new(device, secret, ca)?))),
        );
        let weak = Arc::downgrade(&core);
        core.auth.set_on_revoke(Arc::new(move |id| {
            let weak = weak.clone();
            Box::pin(async move {
                if let Some(core) = weak.upgrade() {
                    core.close_token(&id).await;
                }
            })
        }))?;
        core.start_maintenance();
        Ok(core)
    }
    pub fn register_factory(&self, id: &str, factory: DriverFactory) {
        self.factories.lock().unwrap().insert(id.into(), factory);
    }
    pub(crate) fn event(&self, kind: &str, device: Option<&str>, data: Value) {
        let mut log = self.events.lock().unwrap();
        log.sequence += 1;
        let event = GatewayEvent {
            event_id: log.sequence,
            instance_id: self.instance_id.clone(),
            kind: kind.into(),
            device_id: device.map(str::to_owned),
            at: now_ms(),
            data,
            historical: false,
        };
        log.events.push_back(event.clone());
        if log.events.len() > 256 {
            log.events.pop_front();
        }
        let _ = log.sender.send(event);
    }
    pub fn subscribe_events(
        &self,
        instance: Option<&str>,
        cursor: u64,
    ) -> (Vec<GatewayEvent>, broadcast::Receiver<GatewayEvent>) {
        let log = self.events.lock().unwrap();
        let cursor = if instance == Some(&self.instance_id) {
            cursor
        } else {
            0
        };
        (
            log.events
                .iter()
                .filter(|e| e.event_id > cursor)
                .map(|e| {
                    let mut e = e.clone();
                    e.historical = true;
                    e
                })
                .collect(),
            log.sender.subscribe(),
        )
    }
    pub fn health(&self) -> Value {
        let s = self.state.lock().unwrap();
        let mut lag = s.lag.iter().copied().collect::<Vec<_>>();
        lag.sort_by(f64::total_cmp);
        let p95 = if lag.is_empty() {
            0.0
        } else {
            lag[((lag.len() as f64 * 0.95).ceil() as usize).saturating_sub(1)]
        };
        json!({"store":self.store.state(),"sessions":s.sessions.len(),"event_loop_p95_ms":p95,"errors":s.errors,"queues":s.devices.iter().map(|(id,r)|json!({"device_id":id,"pending":r.pending(),"reconnect_failures":r.state.lock().unwrap().failures})).collect::<Vec<_>>()})
    }
    pub fn metrics(&self) -> Value {
        let s = self.state.lock().unwrap();
        json!({"dispatch_ms":s.dispatch_ms,"batches":s.batches})
    }
    pub fn record_error(&self, code: &str) {
        let mut s = self.state.lock().unwrap();
        if s.errors.len() < 64 || s.errors.contains_key(code) {
            *s.errors.entry(code.into()).or_default() += 1;
        }
    }
    fn failure(&self, error: GatewayError) -> ToolResult {
        self.record_error(&error.code);
        ToolResult::failure(error)
    }
    pub fn get(&self, id: &str) -> Result<Arc<Runtime>> {
        self.state
            .lock()
            .unwrap()
            .devices
            .get(id)
            .cloned()
            .ok_or_else(|| GatewayError::message("DEVICE_OFFLINE", "Unknown device"))
    }
    pub fn list(&self, identity: &Identity) -> Result<Vec<Value>> {
        let token = self.auth.check(identity, Some("devices:read"), None)?;
        let devices = self
            .state
            .lock()
            .unwrap()
            .devices
            .values()
            .cloned()
            .collect::<Vec<_>>();
        Ok(devices
            .iter()
            .filter(|r| {
                token
                    .devices
                    .iter()
                    .any(|d| d == "*" || Some(d) == r.device.device_id.as_ref())
            })
            .map(|r| r.describe())
            .collect())
    }
    pub fn sessions(&self, identity: &Identity) -> Result<Vec<Value>> {
        self.auth.check(identity, None, None)?;
        Ok(self
            .state
            .lock()
            .unwrap()
            .sessions
            .values()
            .filter(|s| {
                s.identity.token_id == identity.token_id
                    && s.identity.client_id == identity.client_id
            })
            .map(|s| s.view())
            .collect())
    }
    pub async fn load(self: &Arc<Self>) -> Result<()> {
        let core = self.clone();
        self.configuration
            .run_owned(async move {
                if core.closing.load(Ordering::Acquire) {
                    return Err(GatewayError::new("DEVICE_OFFLINE"));
                }
                for device in core.store.list::<Value>(Kind::Devices).await? {
                    core.mount(Device::parse(device)?).await?;
                }
                Ok(())
            })
            .await?;
        self.plugins().load().await
    }
    pub fn plugins(self: &Arc<Self>) -> Arc<crate::plugins::Host> {
        self.plugins
            .get_or_init(|| {
                crate::plugins::Host::new(
                    Arc::downgrade(self),
                    self.store.directory().join("plugins"),
                )
            })
            .clone()
    }
    async fn mount(self: &Arc<Self>, device: Device) -> Result<Arc<Runtime>> {
        let id = device.device_id.clone().ok_or_else(GatewayError::invalid)?;
        let factory = self
            .factories
            .lock()
            .unwrap()
            .get(&device.driver_id)
            .cloned()
            .ok_or_else(|| GatewayError::new("UNSUPPORTED_DRIVER"))?;
        let credentials = self.secrets.get(device.secret_ref.as_deref()).await?;
        let ca = if let Some(reference) = &device.tls.ca_ref {
            self.secrets
                .get(Some(reference))
                .await?
                .get("pem")
                .and_then(Value::as_str)
                .map(str::to_owned)
        } else {
            None
        };
        let driver = factory(device.clone(), credentials, ca)?;
        let lifetime = self.lifetime.child_token();
        let media = Media::new(id.clone(), driver.clone(), lifetime.clone());
        let runtime = Arc::new(Runtime {
            state: Mutex::new(RuntimeState {
                status: if device.enabled {
                    "disconnected"
                } else {
                    "disabled"
                }
                .into(),
                error: None,
                caps: None,
                lease: None,
                suspended: false,
                generation: 0,
                abort: None,
                releasing: None,
                retry_at: 0.0,
                failures: 0,
                last_input: None,
            }),
            device,
            driver,
            media,
            queue: DeviceQueue::new(),
            connecting: AsyncMutex::new(()),
            closing: AtomicBool::new(false),
            lifetime,
        });
        self.state
            .lock()
            .unwrap()
            .devices
            .insert(id, runtime.clone());
        self.watch_runtime(&runtime);
        Ok(runtime)
    }
    fn watch_runtime(self: &Arc<Self>, runtime: &Arc<Runtime>) {
        let mut driver = runtime.driver.events();
        let mut media = runtime.media.faults();
        let weak = Arc::downgrade(self);
        let r = Arc::downgrade(runtime);
        let cancellation = runtime.lifetime.clone();
        tokio::spawn(async move {
            loop {
                let fault = tokio::select! {biased;_=cancellation.cancelled()=>break,
                    event=driver.recv()=>match event {
                        Ok(DriverEvent::CapabilitiesChanged)=>None,
                        Ok(DriverEvent::CapabilitiesChangedAt(epoch)) if r.upgrade().is_some_and(|runtime|runtime.driver.connection_epoch()==epoch)=>None,
                        Ok(DriverEvent::DisconnectedAt(epoch)) if r.upgrade().is_some_and(|runtime|runtime.driver.connection_epoch()==epoch)=>Some(GatewayError::new("DEVICE_OFFLINE")),
                        Ok(DriverEvent::DisconnectedAt(_)|DriverEvent::CapabilitiesChangedAt(_))=>continue,
                        Ok(DriverEvent::Disconnected)|Err(broadcast::error::RecvError::Lagged(_))=>Some(GatewayError::new("DEVICE_OFFLINE")),Err(_)=>break},
                    fault=media.recv()=>match fault {Ok(error)=>Some(error),Err(broadcast::error::RecvError::Lagged(_))=>Some(GatewayError::new("DEVICE_OFFLINE")),Err(_)=>break},
                };
                let (Some(core), Some(runtime)) = (weak.upgrade(), r.upgrade()) else {
                    break;
                };
                if let Some(error) = fault {
                    if error.code != "FRAME_TIMEOUT" {
                        core.device_fault(&runtime, error).await;
                    }
                } else {
                    let update = tokio::select! {biased;_=cancellation.cancelled()=>break,result=tokio::time::timeout(Duration::from_secs(5),AssertUnwindSafe(runtime.driver.capabilities()).catch_unwind())=>result};
                    if let Ok(Ok(Ok(caps))) = update {
                        runtime.state.lock().unwrap().caps = Some(caps);
                    }
                }
            }
        });
    }
    pub async fn connect(self: &Arc<Self>, runtime: &Arc<Runtime>) -> Result<()> {
        let _connecting = runtime.connecting.lock().await;
        if runtime.closing.load(Ordering::Acquire)
            || self.closing.load(Ordering::Acquire)
            || !runtime.device.enabled
        {
            return Err(GatewayError::new("DEVICE_OFFLINE"));
        }
        {
            let mut s = runtime.state.lock().unwrap();
            if matches!(s.status.as_str(), "ready" | "no_signal") {
                return Ok(());
            }
            if monotonic_ms() < s.retry_at {
                return Err(GatewayError::message(
                    s.error.as_deref().unwrap_or("DEVICE_OFFLINE"),
                    "Reconnect backoff active",
                ));
            }
            s.status = "connecting".into();
        }
        let work = async {
            runtime.media.pause().await;
            if runtime.media.identity().1 > 0 {
                runtime.driver.disconnect().await?;
            }
            let cancellation = runtime.lifetime.child_token();
            let _cancel = cancellation.clone().drop_guard();
            runtime.driver.connect(cancellation).await?;
            if runtime.closing.load(Ordering::Acquire) {
                return Err(GatewayError::new("DEVICE_OFFLINE"));
            }
            let caps = runtime.driver.capabilities().await?;
            runtime.media.reconnect();
            {
                let mut s = runtime.state.lock().unwrap();
                s.caps = Some(caps);
                s.status = "ready".into();
                s.error = None;
                s.failures = 0;
            }
            runtime.media.resume();
            self.event(
                "device.connected",
                runtime.device.device_id.as_deref(),
                json!({}),
            );
            if runtime.device.media_profile == Profile::Auto
                && runtime.device.driver_id != "simulator"
            {
                let core = self.clone();
                let runtime = runtime.clone();
                tokio::spawn(async move {
                    if let Ok(value) = runtime.media.benchmark().await {
                        core.event("media.strategy", runtime.device.device_id.as_deref(), value);
                    }
                });
            }
            Ok(())
        };
        let result = tokio::time::timeout(
            Duration::from_secs(7),
            AssertUnwindSafe(work).catch_unwind(),
        )
        .await
        .map_err(|_| GatewayError::new("DEVICE_OFFLINE"))
        .and_then(|r| r.unwrap_or_else(|_| Err(unknown())));
        if let Err(error) = &result {
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                AssertUnwindSafe(runtime.driver.disconnect()).catch_unwind(),
            )
            .await;
            let mut s = runtime.state.lock().unwrap();
            s.status = "error".into();
            s.error = Some(error.code.clone());
            let delay = if error.code == "AUTH_FAILED" {
                30000
            } else {
                let delay = 500u64.saturating_mul(1u64 << s.failures.min(6)).min(30000);
                s.failures = s.failures.saturating_add(1);
                delay
            };
            s.retry_at = monotonic_ms() + delay as f64;
        }
        result
    }
    async fn device_fault(self: &Arc<Self>, runtime: &Arc<Runtime>, error: GatewayError) {
        {
            let mut s = runtime.state.lock().unwrap();
            s.status = "reconnecting".into();
            s.error = Some(error.code);
        }
        self.revoke(runtime).await;
    }
    pub async fn save_device(
        self: &Arc<Self>,
        identity: &Identity,
        input: Value,
        secret: Option<Value>,
        ca: Option<Value>,
    ) -> Result<Value> {
        self.auth.check(identity, Some("devices:manage"), None)?;
        let core = self.clone();
        let identity = identity.clone();
        self.configuration
            .run_owned(async move {
                if core.closing.load(Ordering::Acquire) {
                    return Err(GatewayError::new("DEVICE_OFFLINE"));
                }
                core.save_device_owned(&identity, input, secret, ca).await
            })
            .await
    }
    async fn save_device_owned(
        self: &Arc<Self>,
        identity: &Identity,
        input: Value,
        secret: Option<Value>,
        ca: Option<Value>,
    ) -> Result<Value> {
        self.auth.check(identity, Some("devices:manage"), None)?;
        let mut device = Device::parse(input)?;
        let id = device
            .device_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        device.device_id = Some(id.clone());
        self.auth
            .check(identity, Some("devices:manage"), Some(&id))?;
        if device.transport == Transport::Network {
            device_origin(device.address.as_deref().unwrap_or(""))?;
        }
        if !self
            .factories
            .lock()
            .unwrap()
            .contains_key(&device.driver_id)
        {
            return Err(GatewayError::new("UNSUPPORTED_DRIVER"));
        }
        let old = self.state.lock().unwrap().devices.get(&id).cloned();
        if old.as_ref().map_or(0, |r| r.device.revision) != device.revision {
            return Err(GatewayError::new("REVISION_CONFLICT"));
        }
        if device.secret_ref.is_some()
            && device.secret_ref != old.as_ref().and_then(|r| r.device.secret_ref.clone())
        {
            return Err(GatewayError::new("FORBIDDEN"));
        }
        if device.tls.ca_ref.is_some()
            && device.tls.ca_ref != old.as_ref().and_then(|r| r.device.tls.ca_ref.clone())
        {
            return Err(GatewayError::new("FORBIDDEN"));
        }
        let mut created = Vec::new();
        let mut writing_config = false;
        let write = async {
            if let Some(ca) = ca {
                if ca.is_null() {
                    device.tls.ca_ref = None;
                } else {
                    let text = ca.as_str().ok_or_else(GatewayError::invalid)?;
                    crate::contracts::bounded_text(text, 0, 65536)?;
                    if !text.is_empty() {
                        let reference = format!("ca-{}", uuid::Uuid::new_v4());
                        device.tls.ca_ref = Some(reference.clone());
                        created.push(reference.clone());
                        self.secrets.set(&reference, json!({"pem":text})).await?;
                    }
                }
            }
            if let Some(secret) = secret {
                if secret.is_null() {
                    device.secret_ref = None;
                } else if secret.is_object() {
                    let reference = format!("{id}-{}", uuid::Uuid::new_v4());
                    device.secret_ref = Some(reference.clone());
                    created.push(reference.clone());
                    self.secrets.set(&reference, secret).await?;
                } else {
                    return Err(GatewayError::invalid());
                }
            }
            writing_config = true;
            self.store
                .put(
                    Kind::Devices,
                    &id,
                    serde_json::to_value(&device).map_err(|_| GatewayError::invalid())?,
                    device.revision,
                )
                .await
        }
        .await;
        let value = match write {
            Ok(value) => value,
            Err(error) => {
                if !writing_config || error.code == "REVISION_CONFLICT" {
                    for reference in created {
                        let _ = self.secrets.delete(&reference).await;
                    }
                }
                return Err(error);
            }
        };
        let device = Device::parse(value)?;
        if let Some(old) = &old {
            self.unmount(old).await;
        }
        let runtime = self.mount(device).await?;
        if let Some(old) = old {
            if let Some(reference) = &old.device.secret_ref
                && Some(reference) != runtime.device.secret_ref.as_ref()
            {
                self.secrets.delete(reference).await?;
            }
            if let Some(reference) = &old.device.tls.ca_ref
                && Some(reference) != runtime.device.tls.ca_ref.as_ref()
            {
                self.secrets.delete(reference).await?;
            }
        }
        self.event("device.updated", Some(&id), json!({}));
        Ok(runtime.describe())
    }
    pub async fn delete_device(
        self: &Arc<Self>,
        identity: &Identity,
        id: &str,
        revision: i64,
        confirmed: bool,
    ) -> Result<()> {
        self.auth
            .check(identity, Some("devices:manage"), Some(id))?;
        let core = self.clone();
        let identity = identity.clone();
        let id = id.to_owned();
        self.configuration
            .run_owned(async move {
                if core.closing.load(Ordering::Acquire) {
                    return Err(GatewayError::new("DEVICE_OFFLINE"));
                }
                core.delete_device_owned(&identity, &id, revision, confirmed)
                    .await
            })
            .await
    }
    async fn delete_device_owned(
        self: &Arc<Self>,
        identity: &Identity,
        id: &str,
        revision: i64,
        confirmed: bool,
    ) -> Result<()> {
        self.auth
            .check(identity, Some("devices:manage"), Some(id))?;
        let runtime = self.get(id)?;
        if !confirmed && (runtime.media.consumer_count() > 0 || runtime.lease().is_some()) {
            return Err(GatewayError::new("CONFIRM_REQUIRED"));
        }
        self.store.delete(Kind::Devices, id, revision).await?;
        self.unmount(&runtime).await;
        for reference in [&runtime.device.secret_ref, &runtime.device.tls.ca_ref]
            .into_iter()
            .flatten()
        {
            self.secrets.delete(reference).await?;
        }
        self.event("device.deleted", Some(id), json!({}));
        Ok(())
    }
    pub fn session(&self, identity: &Identity, id: &str) -> Result<Arc<Session>> {
        self.auth.check(identity, None, None)?;
        let session = self
            .state
            .lock()
            .unwrap()
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| {
                GatewayError::message(
                    "SESSION_LOST",
                    "Session expired or daemon restarted; open a new observation session",
                )
            })?;
        if session.identity.token_id != identity.token_id
            || session.identity.client_id != identity.client_id
        {
            return Err(GatewayError::message(
                "FORBIDDEN",
                "Session does not belong to this client",
            ));
        }
        self.auth
            .check(&session.identity, None, Some(&session.device_id))?;
        {
            let mut s = session.state.lock().unwrap();
            if s.expires_at <= now_ms() {
                return Err(GatewayError::new("LEASE_EXPIRED"));
            }
            s.expires_at = now_ms() + 3_600_000;
        }
        Ok(session)
    }
    fn assert_control(
        &self,
        identity: &Identity,
        session: &Session,
        runtime: &Runtime,
        generation: Option<u64>,
    ) -> Result<()> {
        self.auth
            .check(identity, Some("input:write"), Some(&session.device_id))?;
        self.auth.check(&session.identity, None, None)?;
        if !self
            .state
            .lock()
            .unwrap()
            .sessions
            .contains_key(&session.session_id)
            || session.state.lock().unwrap().expires_at <= now_ms()
        {
            return Err(GatewayError::new("LEASE_EXPIRED"));
        }
        let s = runtime.state.lock().unwrap();
        if s.suspended {
            return Err(GatewayError::new("CONTROL_SUSPENDED"));
        }
        let lease = s
            .lease
            .as_ref()
            .ok_or_else(|| GatewayError::new("LEASE_REVOKED"))?;
        if lease.session_id != session.session_id || generation.is_some_and(|g| g != s.generation) {
            return Err(GatewayError::new("LEASE_REVOKED"));
        }
        if lease.expires_at <= now_ms() {
            return Err(GatewayError::new("LEASE_EXPIRED"));
        }
        Ok(())
    }
    pub fn authorize_audio(
        &self,
        identity: &Identity,
        id: &str,
        microphone: bool,
        generation: Option<u64>,
    ) -> Result<Arc<Runtime>> {
        let session = self.session(identity, id)?;
        let runtime = self.get(&session.device_id)?;
        if session.state.lock().unwrap().connection_generation != runtime.media.identity().1 {
            return Err(GatewayError::new("SESSION_LOST"));
        }
        self.auth
            .check(identity, Some("audio:read"), Some(&session.device_id))?;
        if microphone {
            self.auth
                .check(identity, Some("audio:write"), Some(&session.device_id))?;
            self.assert_control(identity, &session, &runtime, generation)?;
        }
        Ok(runtime)
    }
    pub fn audio_context(
        self: &Arc<Self>,
        identity: &Identity,
        id: &str,
        cancellation: CancellationToken,
    ) -> Result<ActionContext> {
        let runtime = self.authorize_audio(identity, id, true, None)?;
        let generation = runtime.state.lock().unwrap().generation;
        let core = Arc::downgrade(self);
        let identity = identity.clone();
        let id = id.to_owned();
        Ok(ActionContext {
            cancellation,
            width: 0,
            height: 0,
            assert_control: Arc::new(move || {
                core.upgrade()
                    .ok_or_else(|| GatewayError::new("LEASE_REVOKED"))?
                    .authorize_audio(&identity, &id, true, Some(generation))
                    .map(|_| ())
            }),
        })
    }
    async fn acquire(
        self: &Arc<Self>,
        identity: &Identity,
        session: &Arc<Session>,
        runtime: &Arc<Runtime>,
    ) -> Result<()> {
        self.auth
            .check(identity, Some("input:write"), Some(&session.device_id))?;
        loop {
            let release = runtime.state.lock().unwrap().releasing.clone();
            if let Some(release) = release {
                release.wait().await?;
                continue;
            }
            if runtime.lease().is_some_and(|l| l.expires_at <= now_ms()) {
                self.revoke(runtime).await;
                continue;
            }
            let mut s = runtime.state.lock().unwrap();
            if s.releasing.is_some() {
                continue;
            }
            if s.suspended {
                return Err(GatewayError::new("CONTROL_SUSPENDED"));
            }
            if let Some(lease) = &s.lease
                && lease.session_id != session.session_id
            {
                let mut e = GatewayError::message("CONTROL_BUSY", "Another client holds control");
                e.details = json!({"client_id":lease.client_id,"expires_at":lease.expires_at})
                    .as_object()
                    .unwrap()
                    .clone();
                return Err(e);
            }
            if s.lease.is_none() {
                s.generation += 1;
                runtime.media.bump_input_revision();
                s.lease = Some(Lease {
                    session_id: session.session_id.clone(),
                    client_id: identity.client_id.clone(),
                    expires_at: now_ms() + self.lease_ms,
                    generation: s.generation,
                });
            } else if let Some(lease) = &mut s.lease {
                lease.expires_at = now_ms() + self.lease_ms;
            }
            session.state.lock().unwrap().mode = Mode::Control;
            break;
        }
        self.event("control.changed", Some(&session.device_id), json!({}));
        Ok(())
    }
    async fn release_driver(runtime: &Runtime) -> bool {
        matches!(
            tokio::time::timeout(
                Duration::from_secs(5),
                AssertUnwindSafe(runtime.driver.release_all_inputs()).catch_unwind()
            )
            .await,
            Ok(Ok(Ok(())))
        )
    }
    async fn revoke(self: &Arc<Self>, runtime: &Arc<Runtime>) {
        let flight = {
            let mut s = runtime.state.lock().unwrap();
            if let Some(releasing) = &s.releasing {
                releasing.clone()
            } else {
                let flight = Arc::new(Flight::new());
                s.releasing = Some(flight.clone());
                s.generation += 1;
                runtime.media.bump_input_revision();
                s.lease = None;
                if let Some(abort) = s.abort.take() {
                    abort.cancel();
                }
                let core = self.clone();
                let runtime = runtime.clone();
                let complete = flight.clone();
                tokio::spawn(async move {
                    let released = Self::release_driver(&runtime).await;
                    {
                        let mut s = runtime.state.lock().unwrap();
                        if !released {
                            s.suspended = true;
                            s.error = Some("EXECUTION_UNKNOWN".into());
                        }
                        s.releasing = None;
                    }
                    core.event(
                        "control.changed",
                        runtime.device.device_id.as_deref(),
                        json!({}),
                    );
                    complete.finish(Ok(()));
                });
                flight
            }
        };
        let _ = flight.wait().await;
    }
    pub async fn close_session(self: &Arc<Self>, id: &str) {
        let session = self.state.lock().unwrap().sessions.remove(id);
        if let Some(session) = session
            && let Ok(runtime) = self.get(&session.device_id)
        {
            runtime.media.release(id);
            if runtime.lease().is_some_and(|l| l.session_id == id) {
                self.revoke(&runtime).await;
            }
        }
    }
    async fn close_token(self: &Arc<Self>, id: &str) {
        let sessions = self
            .state
            .lock()
            .unwrap()
            .sessions
            .values()
            .filter(|s| s.identity.token_id == id)
            .map(|s| s.session_id.clone())
            .collect::<Vec<_>>();
        for session in sessions {
            self.close_session(&session).await;
        }
        self.state
            .lock()
            .unwrap()
            .opens
            .retain(|(token, _), _| token != id);
        self.event("token.revoked", None, json!({"token_id":id}));
    }
    pub async fn disconnect_client(self: &Arc<Self>, identity: &Identity) {
        let sessions = self
            .state
            .lock()
            .unwrap()
            .sessions
            .values()
            .filter(|s| {
                s.identity.token_id == identity.token_id
                    && s.identity.client_id == identity.client_id
            })
            .map(|s| s.session_id.clone())
            .collect::<Vec<_>>();
        for session in sessions {
            self.close_session(&session).await;
        }
        self.state
            .lock()
            .unwrap()
            .opens
            .remove(&(identity.token_id.clone(), identity.client_id.clone()));
    }
    pub async fn stop_control(
        self: &Arc<Self>,
        identity: &Identity,
        id: &str,
        resume: bool,
    ) -> Result<Value> {
        self.auth.check(identity, Some("control:stop"), Some(id))?;
        let runtime = self.get(id)?;
        if resume {
            runtime.queue.barrier().await?;
            if !Self::release_driver(&runtime).await {
                return Err(GatewayError::new("EXECUTION_UNKNOWN"));
            }
            runtime.state.lock().unwrap().suspended = false;
            self.event("control.resumed", Some(id), json!({}));
        } else {
            runtime.state.lock().unwrap().suspended = true;
            self.revoke(&runtime).await;
            self.event("control.suspended", Some(id), json!({}));
        }
        Ok(json!({"ok":true,"control_suspended":runtime.suspended()}))
    }
    pub async fn release_inputs(
        self: &Arc<Self>,
        identity: &Identity,
        session_id: &str,
    ) -> Result<()> {
        let session = self.session(identity, session_id)?;
        let runtime = self.get(&session.device_id)?;
        self.assert_control(identity, &session, &runtime, None)?;
        if let Some(abort) = runtime.state.lock().unwrap().abort.as_ref() {
            abort.cancel();
        }
        runtime.queue.barrier().await?;
        if !Self::release_driver(&runtime).await {
            runtime.state.lock().unwrap().suspended = true;
            return Err(GatewayError::new("EXECUTION_UNKNOWN"));
        }
        Ok(())
    }
    pub async fn observe(
        self: &Arc<Self>,
        identity: &Identity,
        session: &Arc<Session>,
        max_age: Duration,
        timeout: Duration,
        mut after: bool,
    ) -> Result<Frame> {
        self.auth
            .check(identity, Some("video:read"), Some(&session.device_id))?;
        let runtime = self.get(&session.device_id)?;
        self.connect(&runtime).await?;
        let last = runtime.last_input_completed_at();
        let mut age = max_age.as_secs_f64() * 1000.0;
        if let Some(last) = last {
            let deadline = last + runtime.device.minimum_action_latency_ms as f64;
            while monotonic_ms() < deadline {
                tokio::select! {biased;_=runtime.lifetime.cancelled()=>return Err(GatewayError::new("DEVICE_OFFLINE")),
                _=tokio::time::sleep(Duration::from_secs_f64(((deadline-monotonic_ms())/1000.0).max(0.0)))=>()}
            }
            age = age.min((monotonic_ms() - deadline).max(0.0));
            after |= runtime
                .media
                .latest()
                .is_none_or(|f| f.info.received_at_monotonic_ms < deadline);
        }
        let frame = runtime
            .media
            .screenshot(Duration::from_secs_f64(age / 1000.0), timeout, after)
            .await?;
        self.auth
            .check(identity, Some("video:read"), Some(&session.device_id))?;
        if !self
            .state
            .lock()
            .unwrap()
            .sessions
            .contains_key(&session.session_id)
        {
            return Err(GatewayError::new("SESSION_LOST"));
        }
        {
            let mut s = session.state.lock().unwrap();
            s.connection_generation = frame.info.connection_generation;
            s.references.push_back(frame.info.clone());
            if s.references.len() > 64 {
                s.references.pop_front();
            }
        }
        Ok(frame)
    }
    fn complete(&self, flight: &Arc<Flight<ToolResult>>, result: ToolResult) {
        let size = result.images.iter().map(|frame| frame.data.len()).sum();
        // Publish while holding the replay lock: another completion must not
        // evict an entry whose image payload has not been published yet.
        let mut replay = self.replay.lock().unwrap();
        replay.retain(flight, size, self.limits.replay_bytes);
        flight.finish(Ok(result));
    }
    async fn scheduled<F>(
        self: &Arc<Self>,
        runtime: &Arc<Runtime>,
        flight: Arc<Flight<ToolResult>>,
        work: F,
    ) -> ToolResult
    where
        F: Future<Output = Result<ToolResult>> + Send + 'static,
    {
        let complete = flight.clone();
        let core = self.clone();
        let r = runtime.clone();
        let accepted = runtime.queue.submit(async move {
            let _guard = Completion(complete.clone());
            let result = if r.closing.load(Ordering::Acquire) {
                Err(GatewayError::new("DEVICE_OFFLINE"))
            } else {
                match AssertUnwindSafe(work).catch_unwind().await {
                    Ok(value) => value,
                    Err(_) => {
                        r.state.lock().unwrap().suspended = true;
                        core.revoke(&r).await;
                        Err(unknown())
                    }
                }
            };
            let result = result.unwrap_or_else(|error| core.failure(error));
            core.complete(&complete, result);
        });
        if let Err(error) = accepted {
            self.complete(&flight, self.failure(error));
        }
        flight
            .wait()
            .await
            .unwrap_or_else(|error| self.failure(error))
    }
    pub async fn tool(
        self: &Arc<Self>,
        identity: &Identity,
        name: &str,
        input: Value,
    ) -> ToolResult {
        match self.tool_checked(identity, name, input).await {
            Ok(result) => result,
            Err(error) => self.failure(error),
        }
    }
    async fn tool_checked(
        self: &Arc<Self>,
        identity: &Identity,
        name: &str,
        input: Value,
    ) -> Result<ToolResult> {
        self.auth.check(identity, None, None)?;
        if self.closing.load(Ordering::Acquire) {
            return Err(GatewayError::new("DEVICE_OFFLINE"));
        }
        let request = ToolRequest::parse_tool(name, input)?;
        let normalized = request.normalized_args();
        if let ToolRequest::ListComputers(args) = request {
            let computers = self
                .list(identity)?
                .into_iter()
                .filter(|d| {
                    args.tags.as_ref().is_none_or(|tags| {
                        tags.iter().all(|tag| {
                            d["tags"]
                                .as_array()
                                .is_some_and(|v| v.contains(&json!(tag)))
                        })
                    })
                })
                .collect::<Vec<_>>();
            return Ok(ToolResult::ok(json!({"computers":computers})));
        }
        if let ToolRequest::OpenComputer(args) = request {
            if args.mode == Mode::Control && args.request_id.is_none() {
                return Err(GatewayError::message(
                    "INVALID_ARGUMENT",
                    "Control requires request_id",
                ));
            }
            self.auth
                .check(identity, Some("video:read"), Some(&args.computer_id))?;
            let result = if let Some(id) = &args.request_id {
                let journal = {
                    let mut s = self.state.lock().unwrap();
                    let key = (identity.token_id.clone(), identity.client_id.clone());
                    if !s.opens.contains_key(&key) && s.opens.len() >= self.limits.max_open_clients
                    {
                        return Err(GatewayError::new("RESOURCE_LIMIT"));
                    }
                    s.opens.entry(key).or_default().clone()
                };
                let (flight, new) = journal.reserve(id, &normalized)?;
                if new {
                    let complete = flight.clone();
                    let core = self.clone();
                    let identity = identity.clone();
                    tokio::spawn(async move {
                        let _guard = Completion(complete.clone());
                        let value = AssertUnwindSafe(core.open(&identity, args))
                            .catch_unwind()
                            .await
                            .unwrap_or_else(|_| Err(unknown()))
                            .unwrap_or_else(|e| core.failure(e));
                        core.complete(&complete, value);
                    });
                }
                flight.wait().await?
            } else {
                let flight = Arc::new(Flight::new());
                let complete = flight.clone();
                let core = self.clone();
                let identity = identity.clone();
                tokio::spawn(async move {
                    let _guard = Completion(complete.clone());
                    let value = AssertUnwindSafe(core.open(&identity, args))
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|_| Err(unknown()))
                        .unwrap_or_else(|e| core.failure(e));
                    core.complete(&complete, value);
                });
                flight.wait().await?
            };
            return Ok(result);
        }
        let id = match &request {
            ToolRequest::Computer(a) => &a.session_id,
            ToolRequest::ComputerScreenshot(a) => &a.session_id,
            ToolRequest::ComputerControl(a) => &a.session_id,
            ToolRequest::CloseComputer(a) => &a.session_id,
            _ => unreachable!(),
        };
        let session = self.session(identity, id)?;
        let runtime = self.get(&session.device_id)?;
        if matches!(request, ToolRequest::CloseComputer(_)) {
            self.close_session(&session.session_id).await;
            return Ok(ToolResult::ok(json!({})));
        }
        if let ToolRequest::ComputerScreenshot(args) = request {
            let image = self
                .observe(
                    identity,
                    &session,
                    Duration::from_millis(args.max_age_ms as u64),
                    Duration::from_millis(args.timeout_ms as u64),
                    false,
                )
                .await?;
            let mut result = ToolResult::ok(json!({}));
            result.images.push(image);
            return Ok(result);
        }
        let request_id = match &request {
            ToolRequest::Computer(a) => &a.request_id,
            ToolRequest::ComputerControl(a) => &a.request_id,
            _ => unreachable!(),
        };
        if let ToolRequest::ComputerControl(args) = &request
            && args.operation == ControlOperation::Release
            && session.journal.len() >= 256
            && !session.journal.contains(request_id)
        {
            self.auth
                .check(identity, Some("input:write"), Some(&session.device_id))?;
            self.close_session(&session.session_id).await;
            return Ok(ToolResult::ok(json!({"lease":null,"session_closed":true})));
        }
        let mut hash = normalized;
        hash.as_object_mut()
            .unwrap()
            .insert("name".into(), name.into());
        let (flight, new) = session.journal.reserve(request_id, &hash)?;
        if !new {
            return flight.wait().await;
        }
        let core = self.clone();
        let identity = identity.clone();
        let r = runtime.clone();
        Ok(self
            .scheduled(&runtime, flight, async move {
                core.session(&identity, &session.session_id)?;
                match request {
                    ToolRequest::ComputerControl(args) => {
                        match args.operation {
                            ControlOperation::Acquire => {
                                core.acquire(&identity, &session, &r).await?
                            }
                            ControlOperation::Renew => {
                                core.assert_control(&identity, &session, &r, None)?;
                                if let Some(lease) = r.state.lock().unwrap().lease.as_mut() {
                                    lease.expires_at = now_ms() + core.lease_ms;
                                }
                            }
                            ControlOperation::Release => {
                                if r.lease()
                                    .is_some_and(|l| l.session_id == session.session_id)
                                {
                                    core.auth.check(
                                        &identity,
                                        Some("input:write"),
                                        Some(&session.device_id),
                                    )?;
                                    core.revoke(&r).await;
                                    session.state.lock().unwrap().mode = Mode::Observe;
                                }
                            }
                        }
                        Ok(ToolResult::ok(json!({"lease":r.lease()})))
                    }
                    ToolRequest::Computer(args) => core.batch(&identity, &session, &r, args).await,
                    _ => unreachable!(),
                }
            })
            .await)
    }
    async fn open(self: &Arc<Self>, identity: &Identity, args: OpenComputer) -> Result<ToolResult> {
        self.auth
            .check(identity, Some("video:read"), Some(&args.computer_id))?;
        let reservation = {
            let mut s = self.state.lock().unwrap();
            let total = s.opening.values().sum::<usize>();
            let owned = s
                .sessions
                .values()
                .filter(|x| x.identity.token_id == identity.token_id)
                .count();
            if s.sessions.len() + total >= self.limits.max_sessions
                || owned + s.opening.get(&identity.token_id).copied().unwrap_or(0)
                    >= self.limits.max_sessions.min(32)
            {
                return Err(GatewayError::new("RESOURCE_LIMIT"));
            }
            *s.opening.entry(identity.token_id.clone()).or_default() += 1;
            Reservation {
                core: Arc::downgrade(self),
                token: identity.token_id.clone(),
            }
        };
        let runtime = self.get(&args.computer_id)?;
        self.connect(&runtime).await?;
        self.auth
            .check(identity, Some("video:read"), Some(&args.computer_id))?;
        if runtime.closing.load(Ordering::Acquire) {
            return Err(GatewayError::new("DEVICE_OFFLINE"));
        }
        let session = Arc::new(Session {
            session_id: uuid::Uuid::new_v4().to_string(),
            device_id: args.computer_id,
            identity: identity.clone(),
            state: Mutex::new(SessionState {
                mode: args.mode,
                expires_at: now_ms() + 3_600_000,
                connection_generation: runtime.media.identity().1,
                references: VecDeque::new(),
            }),
            journal: Journal::default(),
        });
        self.state
            .lock()
            .unwrap()
            .sessions
            .insert(session.session_id.clone(), session.clone());
        drop(reservation);
        let work = async {
            if args.mode == Mode::Control {
                let flight = Arc::new(Flight::new());
                let core = self.clone();
                let identity = identity.clone();
                let s = session.clone();
                let r = runtime.clone();
                let result = self
                    .scheduled(&runtime, flight, async move {
                        core.session(&identity, &s.session_id)?;
                        core.acquire(&identity, &s, &r).await?;
                        Ok(ToolResult::ok(json!({})))
                    })
                    .await;
                if !result.success() {
                    return Ok(result);
                }
            }
            let profile =
                args.profile
                    .unwrap_or(if runtime.device.media_profile == Profile::Auto {
                        Profile::Active
                    } else {
                        runtime.device.media_profile
                    });
            runtime.media.retain(&session.session_id, profile);
            let image = self
                .observe(
                    identity,
                    &session,
                    Duration::ZERO,
                    Duration::from_millis(1500),
                    false,
                )
                .await?;
            let mut result =
                ToolResult::ok(json!({"session":session.view(),"lease":runtime.lease()}));
            result.images.push(image);
            Ok(result)
        }
        .await;
        if !work.as_ref().is_ok_and(ToolResult::success) {
            self.close_session(&session.session_id).await;
        }
        work
    }
    async fn batch(
        self: &Arc<Self>,
        identity: &Identity,
        session: &Arc<Session>,
        runtime: &Arc<Runtime>,
        args: Computer,
    ) -> Result<ToolResult> {
        self.assert_control(identity, session, runtime, None)?;
        self.connect(runtime).await?;
        let reference = session
            .state
            .lock()
            .unwrap()
            .references
            .iter()
            .find(|f| f.frame_id == args.reference.frame_id)
            .cloned()
            .ok_or_else(|| GatewayError::new("VIEW_CHANGED"))?;
        let (view, generation, revision) = runtime.media.identity();
        if reference.view_id != args.reference.view_id
            || reference.view_id != view
            || reference.connection_generation != generation
        {
            return Err(GatewayError::new("VIEW_CHANGED"));
        }
        if args.reference.input_revision != revision
            || reference.input_revision != revision
            || monotonic_ms() - reference.received_at_monotonic_ms > 30_000.0
        {
            return Err(GatewayError::new("STALE_REFERENCE"));
        }
        runtime
            .driver
            .validate(&args.actions, reference.width, reference.height)?;
        let observe = args
            .observation
            .as_ref()
            .is_none_or(|o| o.mode != ObservationMode::None);
        let timeout =
            Duration::from_millis(args.observation.as_ref().map_or(1500, |o| o.timeout_ms) as u64);
        let count = args
            .actions
            .iter()
            .filter(|a| matches!(a, Action::Screenshot {}))
            .count()
            + usize::from(observe);
        if count > 4
            || args
                .actions
                .iter()
                .map(|a| {
                    if let Action::Wait { duration_ms } = a {
                        *duration_ms
                    } else {
                        0
                    }
                })
                .sum::<i64>()
                > 25_000
        {
            return Err(GatewayError::new("RESOURCE_LIMIT"));
        }
        if count > 0 {
            self.auth
                .check(identity, Some("video:read"), Some(&session.device_id))?;
        }
        let cancellation = runtime.lifetime.child_token();
        let generation = {
            let mut s = runtime.state.lock().unwrap();
            s.abort = Some(cancellation.clone());
            s.generation
        };
        let context = {
            let core = self.clone();
            let identity = identity.clone();
            let session = session.clone();
            let runtime = runtime.clone();
            let view = reference.view_id.clone();
            ActionContext {
                cancellation: cancellation.clone(),
                width: reference.width,
                height: reference.height,
                assert_control: Arc::new(move || {
                    core.assert_control(&identity, &session, &runtime, Some(generation))?;
                    if runtime.media.identity().0 != view {
                        return Err(GatewayError::new("VIEW_CHANGED"));
                    }
                    Ok(())
                }),
            }
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut statuses = args
            .actions
            .iter()
            .map(|a| json!({"type":a.name(),"status":"not_started"}))
            .collect::<Vec<_>>();
        let mut images = Vec::new();
        let mut error = None;
        for (index, action) in args.actions.iter().enumerate() {
            let step = async {
                context.check()?;
                match action {
                    Action::Wait { duration_ms } => {
                        context
                            .delay(Duration::from_millis(*duration_ms as u64))
                            .await?;
                        statuses[index]["status"] = "acknowledged".into();
                    }
                    Action::Screenshot {} => {
                        images.push(
                            self.observe(identity, session, Duration::ZERO, timeout, true)
                                .await?,
                        );
                        statuses[index]["status"] = "acknowledged".into();
                    }
                    _ => {
                        statuses[index]["status"] = "unknown".into();
                        runtime.media.bump_input_revision();
                        let start = monotonic_ms();
                        let _completed = InputFinished(runtime.clone());
                        let sent = tokio::select! {biased;_=cancellation.cancelled()=>Err(GatewayError::new("LEASE_REVOKED")),result=tokio::time::timeout_at(deadline,AssertUnwindSafe(runtime.driver.execute(action,&context)).catch_unwind())=>match result{Ok(Ok(result))=>result,Ok(Err(_))|Err(_)=>Err(unknown())}};
                        let mut metrics = self.state.lock().unwrap();
                        metrics.dispatch_ms.push_back(monotonic_ms() - start);
                        if metrics.dispatch_ms.len() > 512 {
                            metrics.dispatch_ms.pop_front();
                        }
                        drop(metrics);
                        statuses[index]["status"] = serde_json::to_value(sent?).unwrap();
                    }
                }
                context.check()?;
                if let Some(lease) = runtime.state.lock().unwrap().lease.as_mut() {
                    lease.expires_at = now_ms() + self.lease_ms;
                }
                Ok(())
            };
            let result = tokio::select! {biased;_=cancellation.cancelled()=>Err(GatewayError::new("LEASE_REVOKED")),result=tokio::time::timeout_at(deadline,AssertUnwindSafe(step).catch_unwind())=>match result{Ok(Ok(result))=>result,Ok(Err(_))|Err(_)=>Err(unknown())}};
            if let Err(failure) = result {
                error = Some(failure);
                break;
            }
        }
        if error.is_some() {
            if !Self::release_driver(runtime).await {
                runtime.state.lock().unwrap().suspended = true;
            }
            self.event(
                "action.failed",
                Some(&session.device_id),
                json!({"statuses":statuses}),
            );
        } else if observe {
            match tokio::time::timeout_at(
                deadline,
                self.observe(identity, session, Duration::ZERO, timeout, true),
            )
            .await
            .unwrap_or_else(|_| Err(GatewayError::new("FRAME_TIMEOUT")))
            {
                Ok(image) => images.push(image),
                Err(failure) => error = Some(failure),
            }
        }
        runtime.state.lock().unwrap().abort = None;
        self.state.lock().unwrap().batches += 1;
        let mut result = if let Some(error) = error {
            self.failure(error)
        } else {
            ToolResult::ok(json!({}))
        };
        result.body.insert("actions".into(), statuses.into());
        result.body.insert(
            "input_revision".into(),
            runtime.media.input_revision().into(),
        );
        result.body.insert(
            "journal_remaining".into(),
            (256 - session.journal.len()).into(),
        );
        result.images = images;
        Ok(result)
    }
    fn start_maintenance(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let cancel = self.lifetime.clone();
        let sweep = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.tick().await;
            loop {
                tokio::select! {biased;_=cancel.cancelled()=>break,_=interval.tick()=>()};
                let Some(core) = weak.upgrade() else {
                    break;
                };
                core.sweep().await;
            }
        });
        let weak = Arc::downgrade(self);
        let cancel = self.lifetime.clone();
        let lag = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(20));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let scheduled =
                    tokio::select! {biased;_=cancel.cancelled()=>break,at=interval.tick()=>at};
                let Some(core) = weak.upgrade() else {
                    break;
                };
                let lag = tokio::time::Instant::now()
                    .saturating_duration_since(scheduled)
                    .as_secs_f64()
                    * 1000.0;
                let mut s = core.state.lock().unwrap();
                s.lag.push_back(lag);
                if s.lag.len() > 512 {
                    s.lag.pop_front();
                }
            }
        });
        self.workers.lock().unwrap().extend([sweep, lag]);
    }
    pub async fn sweep(self: &Arc<Self>) {
        let sessions = self
            .state
            .lock()
            .unwrap()
            .sessions
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for session in sessions {
            if self.auth.check(&session.identity, None, None).is_err()
                || session.state.lock().unwrap().expires_at <= now_ms()
            {
                self.close_session(&session.session_id).await;
            }
        }
        let devices = self
            .state
            .lock()
            .unwrap()
            .devices
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for runtime in devices {
            if runtime.lease().is_some_and(|l| l.expires_at <= now_ms()) {
                self.revoke(&runtime).await;
            }
            let reconnect = {
                let s = runtime.state.lock().unwrap();
                runtime.device.enabled
                    && runtime.media.consumer_count() > 0
                    && matches!(s.status.as_str(), "error" | "reconnecting" | "disconnected")
            };
            if reconnect {
                let core = self.clone();
                tokio::spawn(async move {
                    let _ = core.connect(&runtime).await;
                });
            }
        }
    }
    async fn unmount(self: &Arc<Self>, runtime: &Arc<Runtime>) {
        runtime.closing.store(true, Ordering::Release);
        self.revoke(runtime).await;
        {
            let _connecting = runtime.connecting.lock().await;
        }
        let sessions = self
            .state
            .lock()
            .unwrap()
            .sessions
            .values()
            .filter(|s| Some(&s.device_id) == runtime.device.device_id.as_ref())
            .map(|s| s.session_id.clone())
            .collect::<Vec<_>>();
        for session in sessions {
            self.close_session(&session).await;
        }
        runtime.lifetime.cancel();
        runtime.media.stop().await;
        runtime.queue.close().await;
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            AssertUnwindSafe(runtime.driver.disconnect()).catch_unwind(),
        )
        .await;
        let mut s = self.state.lock().unwrap();
        if let Some(id) = &runtime.device.device_id
            && s.devices.get(id).is_some_and(|r| Arc::ptr_eq(r, runtime))
        {
            s.devices.shift_remove(id);
        }
    }
    pub async fn shutdown(self: &Arc<Self>) {
        self.closing.store(true, Ordering::Release);
        self.lifetime.cancel();
        if let Some(plugins) = self.plugins.get() {
            plugins.shutdown().await;
        }
        self.configuration.close().await;
        let devices = self
            .state
            .lock()
            .unwrap()
            .devices
            .values()
            .cloned()
            .collect::<Vec<_>>();
        join_all(devices.iter().map(|r| self.unmount(r))).await;
        let workers = std::mem::take(&mut *self.workers.lock().unwrap());
        for worker in workers {
            let _ = worker.await;
        }
    }
}
impl Drop for Core {
    fn drop(&mut self) {
        self.lifetime.cancel();
        if let Some(plugins) = self.plugins.get() {
            plugins.stop_now();
        }
    }
}
