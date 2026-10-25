//! One optional Node fork/IPC transport; all host policy and state is native.
mod bridge;
mod process_tree;
pub mod schema;
use crate::{
    GatewayError, Result,
    auth::Identity,
    contracts::{Profile, Validate, bounded_text},
    core::Core,
    private_fs,
    queue::DeviceQueue,
    store::Kind,
};
use bridge::Bridge;
use futures_util::{StreamExt, future::join_all};
use indexmap::IndexMap;
use schema::{Manifest, Panel, Record, Reference};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore, mpsc, oneshot};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

struct Run {
    id: String,
    bridge: Arc<Bridge>,
    exited: CancellationToken,
}
struct State {
    status: String,
    runtime_error: Option<GatewayError>,
    panels: Vec<Panel>,
    subscriptions: HashMap<String, Arc<CancellationToken>>,
    health: Instant,
    event_at: Instant,
    events: u32,
}
struct Entry {
    record: Mutex<Record>,
    run: Mutex<Option<Arc<Run>>>,
    state: Mutex<State>,
}
impl Entry {
    fn new(record: Record) -> Arc<Self> {
        Arc::new(Self {
            record: Mutex::new(record),
            run: Mutex::new(None),
            state: Mutex::new(State {
                status: "disabled".into(),
                runtime_error: None,
                panels: vec![],
                subscriptions: HashMap::new(),
                health: Instant::now(),
                event_at: Instant::now(),
                events: 0,
            }),
        })
    }
    fn view(&self) -> Value {
        let record = self.record.lock().unwrap();
        let state = self.state.lock().unwrap();
        let mut value = json!({"id":record.id,"version":record.manifest.version,"manifest":record.manifest,"devices":record.devices,"enabled":record.enabled,"status":state.status,"panels":state.panels,"revision":record.revision});
        if let Some(error) = &state.runtime_error {
            value["runtime_error"] = serde_json::to_value(error).unwrap();
        }
        value
    }
}
struct Call {
    run: String,
    done: oneshot::Sender<Result<Value>>,
    deadline: Instant,
}
pub struct Host {
    core: Weak<Core>,
    data: PathBuf,
    entries: Mutex<IndexMap<String, Arc<Entry>>>,
    queue: DeviceQueue,
    bridge: AsyncMutex<Option<Arc<Bridge>>>,
    inbox: mpsc::Sender<Value>,
    calls: Mutex<HashMap<String, Call>>,
    cancel: CancellationToken,
    tasks: TaskTracker,
    subscriptions: Arc<Semaphore>,
    wake: Arc<Notify>,
}
fn error(code: &str) -> GatewayError {
    GatewayError::new(code)
}
impl Host {
    pub(crate) fn new(core: Weak<Core>, data: PathBuf) -> Arc<Self> {
        let (inbox, mut receive) = mpsc::channel(256);
        let host = Arc::new(Self {
            core,
            data,
            entries: Mutex::new(IndexMap::new()),
            queue: DeviceQueue::new(),
            bridge: AsyncMutex::new(None),
            inbox,
            calls: Mutex::new(HashMap::new()),
            cancel: CancellationToken::new(),
            tasks: TaskTracker::new(),
            subscriptions: Arc::new(Semaphore::new(256)),
            wake: Arc::new(Notify::new()),
        });
        let weak = Arc::downgrade(&host);
        let cancel = host.cancel.clone();
        let wake = host.wake.clone();
        host.tasks.spawn(async move{
            let mut tick=tokio::time::interval(Duration::from_millis(100));tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut health=tokio::time::interval(Duration::from_secs(10));
            loop{tokio::select!{biased;_=cancel.cancelled()=>break,_=wake.notified()=>(),_=tick.tick(),if weak.upgrade().is_some_and(|h|!h.calls.lock().unwrap().is_empty())=>{
                let Some(host)=weak.upgrade()else{break;};
                host.calls.lock().unwrap().retain(|_,call|call.deadline>Instant::now());
            },_=health.tick()=>{if let Some(host)=weak.upgrade(){host.health();}else{break;}
            },message=receive.recv()=>{let Some(message)=message else{break;};if let Some(host)=weak.upgrade(){host.received(message);}}}}
        });
        host
    }
    fn core(&self) -> Result<Arc<Core>> {
        self.core.upgrade().ok_or_else(|| error("DEVICE_OFFLINE"))
    }
    pub async fn load(self: &Arc<Self>) -> Result<()> {
        let host = self.clone();
        self.queue
            .run_owned(async move {
                let records = host.core()?.store.list::<Value>(Kind::Plugins).await?;
                if records.len() > 128 {
                    return Err(error("RESOURCE_LIMIT"));
                }
                for value in records {
                    let record: Record =
                        serde_json::from_value(value).map_err(|_| error("INVALID_ARGUMENT"))?;
                    record.manifest.validate()?;
                    if record.id != record.manifest.id {
                        return Err(error("INVALID_ARGUMENT"));
                    }
                    let enabled = record.enabled;
                    let id = record.id.clone();
                    let entry = Entry::new(record);
                    host.entries.lock().unwrap().insert(id, entry.clone());
                    if enabled && host.start(&entry).await.is_err() {
                        entry.state.lock().unwrap().status = "error".into();
                    }
                }
                Ok(())
            })
            .await
    }
    pub fn list(&self) -> Vec<Value> {
        self.entries
            .lock()
            .unwrap()
            .values()
            .map(|p| p.view())
            .collect()
    }
    pub fn extensions(&self, identity: &Identity) -> Result<Value> {
        let core = self.core()?;
        let token = core.auth.check(identity, Some("devices:read"), None)?;
        let entries = self.entries.lock().unwrap();
        Ok(Value::Array(entries.values().map(|entry|{
            let record=entry.record.lock().unwrap();let state=entry.state.lock().unwrap();
            json!({"id":record.id,"status":state.status,"panels":state.panels.iter().filter(|p|p.device_id.as_ref().filter(|id|!id.is_empty()).is_none_or(|id|token.devices.iter().any(|d|d=="*"||d==id))).collect::<Vec<_>>()})
        }).collect()))
    }
    fn authorize(&self, identity: &Identity) -> Result<()> {
        self.core()?
            .auth
            .check(identity, Some("plugins:manage"), None)
            .map(|_| ())
    }
    pub async fn install(
        self: &Arc<Self>,
        identity: &Identity,
        path: String,
        devices: Vec<String>,
    ) -> Result<Value> {
        self.authorize(identity)?;
        let host = self.clone();
        let identity = identity.clone();
        self.queue
            .run_owned(async move {
                host.authorize(&identity)?;
                let path = std::path::absolute(path).map_err(|_| GatewayError::invalid())?;
                let file = path.join("kvmhelm.plugin.json");
                let value = tokio::task::spawn_blocking(move || {
                    use std::io::Read;
                    let mut bytes = vec![];
                    std::fs::File::open(file)
                        .and_then(|f| f.take(256 * 1024 + 1).read_to_end(&mut bytes))
                        .map_err(|_| GatewayError::invalid())?;
                    if bytes.len() > 256 * 1024 {
                        return Err(error("RESOURCE_LIMIT"));
                    }
                    serde_json::from_slice(&bytes).map_err(|_| GatewayError::invalid())
                })
                .await
                .map_err(|_| error("PLUGIN_ERROR"))??;
                let manifest = Manifest::parse(value)?;
                host.check_data_id(&manifest.id)?;
                for device in &devices {
                    host.core()?.auth.check(&identity, None, Some(device))?;
                }
                let old = host.entries.lock().unwrap().get(&manifest.id).cloned();
                if old.is_none() && host.entries.lock().unwrap().len() >= 128 {
                    return Err(error("RESOURCE_LIMIT"));
                }
                if let Some(old) = &old {
                    host.stop(old).await;
                }
                let revision = old
                    .as_ref()
                    .map_or(0, |p| p.record.lock().unwrap().revision);
                let record = Record {
                    id: manifest.id.clone(),
                    path: path.to_string_lossy().into(),
                    manifest,
                    devices,
                    enabled: true,
                    revision,
                };
                host.authorize(&identity)?;
                let saved = host
                    .core()?
                    .store
                    .put(
                        Kind::Plugins,
                        &record.id,
                        serde_json::to_value(&record).unwrap(),
                        revision,
                    )
                    .await?;
                let record: Record =
                    serde_json::from_value(saved).map_err(|_| error("STORAGE_ERROR"))?;
                let entry = Entry::new(record);
                let id = entry.record.lock().unwrap().id.clone();
                host.entries.lock().unwrap().insert(id, entry.clone());
                if host.start(&entry).await.is_err() {
                    entry.state.lock().unwrap().status = "error".into();
                }
                Ok(entry.view())
            })
            .await
    }
    fn check_data_id(&self, id: &str) -> Result<()> {
        // These names alias a parent or another component on Windows. Trusted
        // code retains filesystem access; the host itself never follows them.
        let stem = id.split('.').next().unwrap_or("").to_ascii_uppercase();
        if id == "."
            || id == ".."
            || id.ends_with('.')
            || matches!(
                stem.as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
            )
            || self
                .entries
                .lock()
                .unwrap()
                .keys()
                .any(|old| old != id && old.eq_ignore_ascii_case(id))
        {
            return Err(GatewayError::invalid());
        }
        Ok(())
    }
    async fn start(self: &Arc<Self>, entry: &Arc<Entry>) -> Result<()> {
        entry.state.lock().unwrap().runtime_error = None;
        let result = self.start_process(entry).await;
        if let Err(error) = &result {
            let mut state = entry.state.lock().unwrap();
            state.status = "error".into();
            state.runtime_error = Some(error.clone());
            drop(state);
            if let Ok(core) = self.core() {
                core.record_error(&error.code);
            }
        }
        result
    }
    async fn start_process(self: &Arc<Self>, entry: &Arc<Entry>) -> Result<()> {
        if self.cancel.is_cancelled() {
            return Err(error("DEVICE_OFFLINE"));
        }
        let record = entry.record.lock().unwrap().clone();
        self.check_data_id(&record.id)?;
        let own =
            std::path::absolute(self.data.join(&record.id)).map_err(|_| error("STORAGE_ERROR"))?;
        let directory = own.clone();
        let base = self.data.clone();
        tokio::task::spawn_blocking(move || {
            private_fs::directory(&base)?;
            private_fs::directory(&directory)
        })
        .await
        .map_err(|_| error("STORAGE_ERROR"))?
        .map_err(|_| error("STORAGE_ERROR"))?;
        let mut current = self.bridge.lock().await;
        if current.as_ref().is_none_or(|b| !b.alive()) {
            if let Some(old) = current.take() {
                old.close().await;
            }
            let inbox = self.inbox.clone();
            *current = Some(Bridge::start(Arc::new(move |m| inbox.try_send(m).is_ok())).await?);
        }
        let bridge = current.as_ref().unwrap().clone();
        drop(current);
        let id = uuid::Uuid::new_v4().to_string();
        let executable = std::path::absolute(Path::new(&record.path).join(&record.manifest.entry))
            .map_err(|_| GatewayError::invalid())?;
        let exited = bridge.launch(&id, &record.path, &executable.to_string_lossy())?;
        let run = Arc::new(Run { id, bridge, exited });
        {
            let mut current = entry.run.lock().unwrap();
            *current = Some(run.clone());
            let mut state = entry.state.lock().unwrap();
            state.status = "starting".into();
            state.health = Instant::now();
        }
        let initialize = json!({"type":"initialize","apiVersion":"kvmhelm.plugin/v1","plugin_id":record.id,"permissions":record.manifest.permissions,"devices":record.devices,"data_path":own,"secret_refs":record.manifest.secret_refs,"budgets":{"max_buffer_bytes":1048576,"max_chunk_bytes":262144,"max_events_per_second":30}});
        if Self::tell(&run, initialize).is_err()
            || Self::tell(&run, json!({"type":"start"})).is_err()
        {
            let _ = run.bridge.command(json!({"type":"kill","id":run.id}));
            return Err(error("PLUGIN_ERROR"));
        }
        Ok(())
    }
    fn tell(run: &Run, message: Value) -> Result<()> {
        drop(run.bridge.enqueue(&run.id, message, None, None)?);
        Ok(())
    }
    fn status_event(&self, entry: &Entry) {
        let id = entry.record.lock().unwrap().id.clone();
        let status = entry.state.lock().unwrap().status.clone();
        if let Ok(core) = self.core() {
            core.event(
                "plugin.status",
                None,
                json!({"plugin_id":id,"status":status}),
            );
        }
    }
    async fn stop(&self, entry: &Arc<Entry>) {
        let run = entry.run.lock().unwrap().take();
        {
            let mut state = entry.state.lock().unwrap();
            state.status = "disabled".into();
            for (_, cancel) in state.subscriptions.drain() {
                cancel.cancel();
            }
            state.panels.clear();
        }
        if let Some(run) = run {
            self.calls.lock().unwrap().retain(|_, c| c.run != run.id);
            let _ = Self::tell(&run, json!({"type":"stop"}));
            let _ = Self::tell(&run, json!({"type":"dispose"}));
            if tokio::time::timeout(Duration::from_secs(2), run.exited.cancelled())
                .await
                .is_err()
            {
                if run
                    .bridge
                    .command(json!({"type":"kill","id":run.id}))
                    .is_err()
                {
                    run.bridge.abort();
                }
                if tokio::time::timeout(Duration::from_secs(2), run.exited.cancelled())
                    .await
                    .is_err()
                {
                    run.bridge.abort();
                }
            }
        }
        self.status_event(entry);
    }
    pub async fn operation(
        self: &Arc<Self>,
        identity: &Identity,
        id: String,
        op: String,
    ) -> Result<Value> {
        self.authorize(identity)?;
        if !matches!(op.as_str(), "enable" | "disable" | "uninstall" | "restart") {
            return Err(GatewayError::invalid());
        }
        let host = self.clone();
        let identity = identity.clone();
        self.queue
            .run_owned(async move {
                host.authorize(&identity)?;
                let entry = host
                    .entries
                    .lock()
                    .unwrap()
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| error("NOT_FOUND"))?;
                host.stop(&entry).await;
                let mut record = entry.record.lock().unwrap().clone();
                if op == "uninstall" {
                    host.core()?
                        .store
                        .delete(Kind::Plugins, &id, record.revision)
                        .await?;
                    host.entries.lock().unwrap().shift_remove(&id);
                    return Ok(json!({"ok":true}));
                }
                record.enabled = op != "disable";
                let saved = host
                    .core()?
                    .store
                    .put(
                        Kind::Plugins,
                        &id,
                        serde_json::to_value(&record).unwrap(),
                        record.revision,
                    )
                    .await?;
                record = serde_json::from_value(saved).map_err(|_| error("STORAGE_ERROR"))?;
                let enabled = record.enabled;
                *entry.record.lock().unwrap() = record;
                if enabled && let Err(error) = host.start(&entry).await {
                    entry.state.lock().unwrap().status = "error".into();
                    return Err(error);
                }
                Ok(json!({"ok":true}))
            })
            .await
    }
    fn received(self: &Arc<Self>, value: Value) {
        let kind = value["type"].as_str().unwrap_or("");
        let entries = self
            .entries
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for entry in entries {
            let run = entry.run.lock().unwrap().clone();
            let Some(run) = run else {
                continue;
            };
            if kind != "bridge.exit" && value["id"] != run.id {
                continue;
            }
            if kind == "bridge.exit" && value["bridge"] != run.bridge.id {
                continue;
            }
            if kind == "exit" || kind == "bridge.exit" {
                // Keep generation replacement behind all old-run cleanup,
                // including pending calls, subscriptions and status changes.
                let mut current = entry.run.lock().unwrap();
                if current.as_ref().is_none_or(|r| r.id != run.id) {
                    continue;
                }
                current.take();
                self.calls.lock().unwrap().retain(|_, c| c.run != run.id);
                let id = entry.record.lock().unwrap().id.clone();
                let status = {
                    let mut state = entry.state.lock().unwrap();
                    for (_, cancel) in state.subscriptions.drain() {
                        cancel.cancel();
                    }
                    if state.status != "disabled" {
                        state.status = "error".into();
                    }
                    state.status.clone()
                };
                if let Ok(core) = self.core() {
                    core.event(
                        "plugin.status",
                        None,
                        json!({"plugin_id":id,"status":status}),
                    );
                }
            } else if kind == "message" {
                let current = entry.run.lock().unwrap();
                if current.as_ref().is_none_or(|r| r.id != run.id) {
                    continue;
                }
                if self.message(&entry, &run, &value["message"]).is_err() {
                    let _ = Self::tell(
                        &run,
                        json!({"type":"error","code":"PLUGIN_REQUEST_REJECTED"}),
                    );
                }
            }
        }
    }
    fn message(self: &Arc<Self>, entry: &Arc<Entry>, run: &Arc<Run>, m: &Value) -> Result<()> {
        let Some(kind) = m["type"].as_str() else {
            return Ok(());
        };
        if matches!(kind, "ready" | "health") {
            let mut state = entry.state.lock().unwrap();
            state.health = Instant::now();
            let changed = state.status != "running";
            state.status = "running".into();
            drop(state);
            if changed {
                self.status_event(entry);
            }
            return Ok(());
        }
        if kind == "action.result" {
            if let Some(id) = m["request_id"].as_str() {
                let mut calls = self.calls.lock().unwrap();
                if calls.get(id).is_some_and(|c| c.run == run.id)
                    && let Some(call) = calls.remove(id)
                {
                    let _ = call.done.send(if m["ok"] == true {
                        Ok(json!({"ok":true}))
                    } else {
                        Err(error("PLUGIN_ERROR"))
                    });
                }
            }
            return Ok(());
        }
        let record = entry.record.lock().unwrap().clone();
        if !record.enabled {
            return Err(error("FORBIDDEN"));
        }
        {
            let mut state = entry.state.lock().unwrap();
            if state.event_at.elapsed() > Duration::from_secs(1) {
                state.event_at = Instant::now();
                state.events = 0;
            }
            state.events += 1;
            if state.events > 30 {
                return Err(error("RESOURCE_LIMIT"));
            }
        }
        let allowed = |permission: &str, device: Option<&str>| -> Result<()> {
            if !record.manifest.permissions.iter().any(|p| p == permission)
                || device
                    .filter(|id| !id.is_empty())
                    .is_some_and(|id| !record.devices.iter().any(|d| d == id))
            {
                return Err(error("FORBIDDEN"));
            }
            Ok(())
        };
        match kind {
            "devices" => {
                let core = self.core()?;
                let mut devices = vec![];
                for id in &record.devices {
                    let d = core.get(id)?;
                    let view = d.describe();
                    devices.push(json!({"device_id":id,"name":d.device.name,"capabilities":view["capabilities"]}));
                }
                Self::tell(run, json!({"type":"devices","devices":devices}))?;
            }
            "panel" => {
                let panel = Panel::parse(m["panel"].clone())?;
                allowed("ui:panel", panel.device_id.as_deref())?;
                if !record.manifest.ui_slots.contains(&panel.slot) {
                    return Err(error("FORBIDDEN"));
                }
                {
                    let mut state = entry.state.lock().unwrap();
                    state.panels.retain(|p| p.id != panel.id);
                    state.panels.push(panel.clone());
                    if state.panels.len() > 256 {
                        state.panels.remove(0);
                    }
                }
                self.core()?.event(
                    "plugin.panel",
                    panel.device_id.as_deref(),
                    json!({"plugin_id":record.id,"panel":panel}),
                );
            }
            "notification" => {
                let device = m["device_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| error("FORBIDDEN"))?;
                allowed("notifications:publish", Some(device))?;
                let text = m["text"].as_str().ok_or_else(GatewayError::invalid)?;
                bounded_text(text, 0, 2000)?;
                let mut data = json!({"plugin_id":record.id,"text":text});
                if let Some(reference) = m.get("reference") {
                    let reference = Reference::parse(reference.clone())?;
                    if !entry.state.lock().unwrap().panels.iter().any(|p| {
                        p.id == reference.panel_id && p.device_id.as_deref() == Some(device)
                    }) {
                        return Err(error("FORBIDDEN"));
                    }
                    data["reference"] = serde_json::to_value(reference).unwrap();
                }
                self.core()?
                    .event("plugin.notification", Some(device), data);
            }
            "unsubscribe" => {
                if let Some(id) = m["id"].as_str()
                    && let Some(cancel) = entry.state.lock().unwrap().subscriptions.remove(id)
                {
                    cancel.cancel();
                }
            }
            "subscribe" => {
                let source = m["source"]
                    .as_str()
                    .filter(|s| matches!(*s, "audio" | "video"))
                    .ok_or_else(GatewayError::invalid)?;
                let id = m["id"].as_str().ok_or_else(GatewayError::invalid)?;
                bounded_text(id, 0, 100)?;
                let device = m["device_id"].as_str().ok_or_else(GatewayError::invalid)?;
                allowed(
                    if source == "audio" {
                        "audio:read:from_target"
                    } else {
                        "video:read"
                    },
                    Some(device),
                )?;
                self.subscribe(
                    entry.clone(),
                    run.clone(),
                    id.into(),
                    device.into(),
                    source == "audio",
                )?;
            }
            _ => (),
        }
        Ok(())
    }
    fn subscribe(
        self: &Arc<Self>,
        entry: Arc<Entry>,
        run: Arc<Run>,
        id: String,
        device: String,
        audio: bool,
    ) -> Result<()> {
        let cancel = Arc::new(self.cancel.child_token());
        {
            let mut state = entry.state.lock().unwrap();
            if state.subscriptions.len() >= 64 {
                return Err(error("RESOURCE_LIMIT"));
            }
            if state.subscriptions.contains_key(&id) {
                return Err(error("REQUEST_CONFLICT"));
            }
            state.subscriptions.insert(id.clone(), cancel.clone());
        }
        let permit = match self.subscriptions.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                entry.state.lock().unwrap().subscriptions.remove(&id);
                return Err(error("RESOURCE_LIMIT"));
            }
        };
        let host = self.clone();
        self.tasks.spawn(async move {
            let _permit = permit;
            let plugin = entry.record.lock().unwrap().id.clone();
            let result = host
                .source(&run, &plugin, &id, &device, audio, &cancel)
                .await;
            let mut state = entry.state.lock().unwrap();
            if state
                .subscriptions
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, &cancel))
            {
                state.subscriptions.remove(&id);
                if let Err(error) = result {
                    let _ = Self::tell(&run, json!({"type":"gap","id":id,"code":error.code}));
                }
            }
        });
        Ok(())
    }
    async fn source(
        &self,
        run: &Run,
        plugin: &str,
        id: &str,
        device: &str,
        audio: bool,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let core = self.core()?;
        let runtime = core.get(device)?;
        tokio::select! {biased;_=cancel.cancelled()=>return Ok(()),result=core.connect(&runtime)=>result?};
        drop(core);
        if cancel.is_cancelled() {
            return Ok(());
        }
        let consumer = format!("plugin:{plugin}:{id}:{}", uuid::Uuid::new_v4());
        runtime.media.retain(&consumer, Profile::Warm);
        struct Lease(Arc<crate::media::Media>, String);
        impl Drop for Lease {
            fn drop(&mut self) {
                self.0.release(&self.1);
            }
        }
        let _lease = Lease(runtime.media.clone(), consumer);
        if audio {
            let mut stream = tokio::select! {biased;_=cancel.cancelled()=>return Ok(()),s=runtime.driver.subscribe_audio(cancel.clone())=>s?};
            loop {
                let chunk = tokio::select! {biased;_=cancel.cancelled()=>return Ok(()),chunk=stream.next()=>chunk.ok_or_else(||error("SOURCE_UNAVAILABLE"))??};
                let message = json!({"type":"media","id":id,"device_id":device,"chunk":{"device_id":chunk.device_id,"format":chunk.format,"sample_rate":chunk.sample_rate,"channels":chunk.channels,"sequence":chunk.sequence,"timestamp_ms":chunk.timestamp_ms,"connection_generation":chunk.connection_generation,"discontinuity":chunk.discontinuity}});
                if !run
                    .bridge
                    .send(&run.id, message, Some(chunk.data), cancel)
                    .await?
                {
                    return Err(error("RESOURCE_LIMIT"));
                }
            }
        } else {
            loop {
                let frame = tokio::select! {biased;_=cancel.cancelled()=>return Ok(()),frame=runtime.media.screenshot(Duration::from_millis(250),Duration::from_secs(3),false)=>frame?};
                let message = json!({"type":"media","id":id,"device_id":device,"chunk":frame.info});
                if cancel.is_cancelled() {
                    return Ok(());
                }
                if !run
                    .bridge
                    .send(&run.id, message, Some(frame.data), cancel)
                    .await?
                {
                    return Err(error("RESOURCE_LIMIT"));
                }
                tokio::select! {biased;_=cancel.cancelled()=>return Ok(()),_=tokio::time::sleep(Duration::from_millis(250))=>()};
            }
        }
    }
    pub async fn panel_action(
        self: &Arc<Self>,
        identity: &Identity,
        id: &str,
        panel: &str,
        action: &str,
        values: Value,
    ) -> Result<Value> {
        self.authorize(identity)?;
        let entry = self
            .entries
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| error("NOT_FOUND"))?;
        let run = entry
            .run
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| error("NOT_FOUND"))?;
        let panel = {
            let state = entry.state.lock().unwrap();
            if state.status != "running" {
                return Err(error("NOT_FOUND"));
            }
            state
                .panels
                .iter()
                .find(|p| p.id == panel)
                .cloned()
                .ok_or_else(|| error("NOT_FOUND"))?
        };
        if !entry.record.lock().unwrap().enabled {
            return Err(error("NOT_FOUND"));
        }
        if !panel.actions.as_ref().is_some_and(|actions| {
            actions
                .iter()
                .any(|a| a.id == action && a.disabled != Some(true))
        }) {
            return Err(error("FORBIDDEN"));
        }
        let values = values.as_object().ok_or_else(GatewayError::invalid)?;
        for (key, value) in values {
            panel
                .fields
                .as_ref()
                .and_then(|f| f.iter().find(|f| &f.id == key))
                .ok_or_else(GatewayError::invalid)?
                .check_value(value)?;
        }
        let core = self.core()?;
        if let Some(device) = panel.device_id.as_ref().filter(|id| !id.is_empty()) {
            core.auth
                .check(identity, Some("devices:read"), Some(device))?;
        }
        let identity = identity.clone();
        let check_core = Arc::downgrade(&core);
        let device = panel.device_id.clone().filter(|id| !id.is_empty());
        let expected_run = run.id.clone();
        let target = Arc::downgrade(&entry);
        let check = Arc::new(move || {
            let target = target.upgrade().ok_or_else(|| error("NOT_FOUND"))?;
            if target
                .run
                .lock()
                .unwrap()
                .as_ref()
                .is_none_or(|r| r.id != expected_run)
            {
                return Err(error("NOT_FOUND"));
            }
            let core = check_core
                .upgrade()
                .ok_or_else(|| error("DEVICE_OFFLINE"))?;
            core.auth.check(&identity, Some("plugins:manage"), None)?;
            if let Some(device) = &device {
                core.auth
                    .check(&identity, Some("devices:read"), Some(device))?;
            }
            Ok(())
        });
        let request = uuid::Uuid::new_v4().to_string();
        let (done, wait) = oneshot::channel();
        {
            let mut calls = self.calls.lock().unwrap();
            if calls.len() >= 32 {
                return Err(error("RESOURCE_LIMIT"));
            }
            calls.insert(
                request.clone(),
                Call {
                    run: run.id.clone(),
                    done,
                    deadline: Instant::now() + Duration::from_secs(10),
                },
            );
        }
        let message = json!({"type":"action","request_id":request,"panel_id":panel.id,"action":action,"values":values});
        self.wake.notify_one();
        let mut message = message;
        if let Some(device) = panel.device_id {
            message["device_id"] = device.into();
        }
        match run.bridge.enqueue(&run.id, message, None, Some(check)) {
            Ok(delivery) => {
                if !matches!(delivery.await, Ok(Ok(true))) {
                    self.calls.lock().unwrap().remove(&request);
                    return Err(error("PLUGIN_ERROR"));
                }
            }
            Err(error) => {
                self.calls.lock().unwrap().remove(&request);
                return Err(error);
            }
        }
        wait.await.map_err(|_| {
            GatewayError::message(
                "PLUGIN_ERROR",
                "Plugin antwortet nicht oder wurde beendet. Status vor erneutem Versuch prüfen.",
            )
        })?
    }
    fn health(self: &Arc<Self>) {
        let entries = self
            .entries
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for entry in entries {
            let run = entry.run.lock().unwrap().clone();
            if let Some(run) = run {
                if entry.state.lock().unwrap().health.elapsed() > Duration::from_secs(30) {
                    let host = self.clone();
                    let target = entry.clone();
                    let id = run.id.clone();
                    if self
                        .queue
                        .submit(async move {
                            if target
                                .run
                                .lock()
                                .unwrap()
                                .as_ref()
                                .is_some_and(|r| r.id == id)
                            {
                                host.stop(&target).await;
                                target.state.lock().unwrap().status = "error".into();
                                host.status_event(&target);
                            }
                        })
                        .is_err()
                    {
                        let _ = run.bridge.command(json!({"type":"kill","id":run.id}));
                    }
                } else {
                    let _ = Self::tell(&run, json!({"type":"health"}));
                }
            }
        }
    }
    pub(crate) fn stop_now(&self) {
        self.cancel.cancel();
        for entry in self.entries.lock().unwrap().values() {
            for cancel in entry.state.lock().unwrap().subscriptions.values() {
                cancel.cancel();
            }
        }
        if let Ok(bridge) = self.bridge.try_lock()
            && let Some(bridge) = bridge.as_ref()
        {
            bridge.abort();
        }
    }
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        self.queue.close().await;
        let entries = self
            .entries
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        join_all(entries.iter().map(|entry| self.stop(entry))).await;
        if let Some(bridge) = self.bridge.lock().await.take() {
            bridge.close().await;
        }
        self.tasks.close();
        self.tasks.wait().await;
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        self.stop_now();
    }
}
