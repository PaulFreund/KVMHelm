use async_trait::async_trait;
use futures_util::stream;
use kvmhelm::{
    GatewayError, Result,
    auth::{Auth, Identity},
    contracts::{Action, Capabilities, Layout, SCOPES, Signal},
    core::{Core, Limits, ToolResult},
    drivers::{ActionContext, Delivery, DriverEvent, KvmDriver, VideoStream, simulator::Simulator},
    keyboard::validate_hid,
    media::{DriverFrame, Media, Mime, Strategy},
    secrets::Secrets,
    store::{Kind, Store},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Cursor,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Gate {
    entered: AtomicUsize,
    arrived: Notify,
    release: Notify,
}
impl Gate {
    async fn block(&self, cancellation: &CancellationToken) -> Result<()> {
        self.entered.fetch_add(1, Ordering::AcqRel);
        self.arrived.notify_waiters();
        tokio::select! {biased;_=cancellation.cancelled()=>Err(GatewayError::new("LEASE_REVOKED")),_=self.release.notified()=>Ok(())}
    }
    async fn entered(&self) {
        until(|| self.entered.load(Ordering::Acquire) > 0).await;
    }
}
async fn until(condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("condition must become true");
}
fn png(width: u32, height: u32) -> Arc<[u8]> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&vec![127u8; width as usize * height as usize])
            .unwrap();
    }
    out.into()
}
struct Driver {
    connected: AtomicBool,
    connects: AtomicUsize,
    snapshots: AtomicUsize,
    executes: AtomicUsize,
    actions: Mutex<Vec<Action>>,
    releases: AtomicUsize,
    capture_gate: Mutex<Option<Arc<Gate>>>,
    capability_gate: Mutex<Option<Arc<Gate>>>,
    release_gate: Mutex<Option<Arc<Gate>>>,
    disconnects: AtomicUsize,
    execute_gate: Mutex<Option<Arc<Gate>>>,
    capture_fault: AtomicUsize,
    execute_fault: AtomicBool,
    release_fault: AtomicBool,
    source_time: AtomicBool,
    shape: Mutex<(u32, u32)>,
    signal: Mutex<Signal>,
    events: broadcast::Sender<DriverEvent>,
    caps: Capabilities,
    stream: AtomicBool,
    layout: Layout,
}
impl Driver {
    fn new(layout: Layout, caps: Capabilities) -> Arc<Self> {
        let (events, _) = broadcast::channel(16);
        Arc::new(Self {
            connected: AtomicBool::new(false),
            connects: AtomicUsize::new(0),
            snapshots: AtomicUsize::new(0),
            executes: AtomicUsize::new(0),
            actions: Mutex::new(Vec::new()),
            releases: AtomicUsize::new(0),
            capture_gate: Mutex::new(None),
            capability_gate: Mutex::new(None),
            release_gate: Mutex::new(None),
            disconnects: AtomicUsize::new(0),
            execute_gate: Mutex::new(None),
            capture_fault: AtomicUsize::new(0),
            execute_fault: AtomicBool::new(false),
            release_fault: AtomicBool::new(false),
            source_time: AtomicBool::new(true),
            shape: Mutex::new((128, 72)),
            signal: Mutex::new(Signal::Present),
            events,
            caps,
            stream: AtomicBool::new(false),
            layout,
        })
    }
    fn raw(&self) -> DriverFrame {
        let (width, height) = *self.shape.lock().unwrap();
        DriverFrame {
            data: png(width, height),
            width,
            height,
            mime_type: Mime::Png,
            signal: *self.signal.lock().unwrap(),
            source_timestamp: self
                .source_time
                .load(Ordering::Acquire)
                .then_some(kvmhelm::monotonic_ms()),
        }
    }
}
#[async_trait]
impl KvmDriver for Driver {
    fn events(&self) -> broadcast::Receiver<DriverEvent> {
        self.events.subscribe()
    }
    async fn connect(&self, _: CancellationToken) -> Result<()> {
        self.connects.fetch_add(1, Ordering::AcqRel);
        self.connected.store(true, Ordering::Release);
        Ok(())
    }
    async fn disconnect(&self) -> Result<()> {
        self.disconnects.fetch_add(1, Ordering::AcqRel);
        self.connected.store(false, Ordering::Release);
        Ok(())
    }
    async fn capabilities(&self) -> Result<Capabilities> {
        let gate = self.capability_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.block(&CancellationToken::new()).await?;
        }
        let mut caps = self.caps.clone();
        caps.video.stream = self.stream.load(Ordering::Acquire);
        Ok(caps)
    }
    async fn snapshot(&self, cancellation: CancellationToken) -> Result<DriverFrame> {
        self.snapshots.fetch_add(1, Ordering::AcqRel);
        let frame = self.raw();
        let gate = self.capture_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.block(&cancellation).await?;
        }
        match self.capture_fault.load(Ordering::Acquire) {
            0 => Ok(frame),
            1 => Err(GatewayError::new("FRAME_TIMEOUT")),
            3 => Err(GatewayError::new("NO_SIGNAL")),
            _ => Err(GatewayError::new("DEVICE_OFFLINE")),
        }
    }
    async fn subscribe_video(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Option<VideoStream>> {
        if !self.stream.load(Ordering::Acquire) {
            return Ok(None);
        }
        let raw = self.raw();
        Ok(Some(Box::pin(stream::unfold(
            (raw, cancellation),
            |(raw, cancel)| async move {
                tokio::select! {biased;_=cancel.cancelled()=>None,_=tokio::time::sleep(Duration::from_millis(2))=>Some((Ok(raw.clone()),(raw,cancel)))}
            },
        ))))
    }
    fn validate(&self, actions: &[Action], w: u32, h: u32) -> Result<()> {
        validate_hid(actions, w, h, true, true, self.layout)
    }
    async fn execute(&self, action: &Action, context: &ActionContext) -> Result<Delivery> {
        context.check()?;
        self.actions.lock().unwrap().push(action.clone());
        self.executes.fetch_add(1, Ordering::AcqRel);
        let gate = self.execute_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.block(&context.cancellation).await?;
        }
        if self.execute_fault.load(Ordering::Acquire) {
            return Err(GatewayError::new("EXECUTION_UNKNOWN"));
        }
        context.check()?;
        Ok(Delivery::Sent)
    }
    async fn release_all_inputs(&self) -> Result<()> {
        let gate = self.release_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.block(&CancellationToken::new()).await?;
        }
        self.releases.fetch_add(1, Ordering::AcqRel);
        if self.release_fault.load(Ordering::Acquire) {
            Err(GatewayError::new("EXECUTION_UNKNOWN"))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn accepted_configuration_updates_finish_after_the_requesting_caller_disappears() {
    let f = Fixture::new().await;
    let original = f.core.get(&f.id).unwrap().device.clone();
    f.core
        .save_device(
            &f.a,
            serde_json::to_value(original).unwrap(),
            Some(json!({"password":"old fixture"})),
            None,
        )
        .await
        .unwrap();
    let mut config = f.core.get(&f.id).unwrap().device.clone();
    let old_ref = config.secret_ref.clone();
    let old_path = f.core.secrets.path(old_ref.as_deref().unwrap()).unwrap();
    config.name = "Updated after cancellation".into();
    let driver = f.driver();
    let gate = Arc::new(Gate::default());
    *driver.release_gate.lock().unwrap() = Some(gate.clone());
    let core = f.core.clone();
    let identity = f.a.clone();
    let caller = tokio::spawn(async move {
        core.save_device(
            &identity,
            serde_json::to_value(config).unwrap(),
            Some(json!({"password":"new fixture"})),
            None,
        )
        .await
    });
    gate.entered().await;
    caller.abort();
    assert!(caller.await.err().unwrap().is_cancelled());
    let stored = f
        .store
        .get::<Value>(Kind::Devices, &f.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored["name"], "Updated after cancellation");
    gate.release.notify_one();
    until(|| {
        f.core
            .get(&f.id)
            .is_ok_and(|r| r.device.name == "Updated after cancellation")
    })
    .await;
    until(|| !old_path.exists()).await;
    assert!(f.core.secrets.get(old_ref.as_deref()).await.is_err());
    let current = f.core.get(&f.id).unwrap();
    assert_eq!(current.device.revision, 3);
    assert_eq!(
        f.core
            .secrets
            .get(current.device.secret_ref.as_deref())
            .await
            .unwrap()["password"],
        "new fixture"
    );
    f.close().await;
}

#[tokio::test]
async fn connection_deadlines_include_capability_discovery_and_cleanup_partial_connections() {
    let f = Fixture::new().await;
    let driver = f.driver();
    let gate = Arc::new(Gate::default());
    *driver.capability_gate.lock().unwrap() = Some(gate.clone());
    tokio::time::pause();
    let core = f.core.clone();
    let runtime = f.core.get(&f.id).unwrap();
    let target = runtime.clone();
    let connection = tokio::spawn(async move { core.connect(&target).await });
    gate.entered().await;
    tokio::time::advance(Duration::from_secs(8)).await;
    let error = connection.await.unwrap().unwrap_err();
    assert_eq!(error.code, "DEVICE_OFFLINE");
    assert!(!driver.connected.load(Ordering::Acquire));
    assert_eq!(driver.disconnects.load(Ordering::Acquire), 1);
    f.close().await;
}

#[tokio::test]
async fn a_hung_shared_capture_times_out_and_allows_a_new_capture() {
    let (media, driver) = media_fixture().await;
    let gate = Arc::new(Gate::default());
    *driver.capture_gate.lock().unwrap() = Some(gate.clone());
    tokio::time::pause();
    let pending = {
        let media = media.clone();
        tokio::spawn(async move { media.capture().await })
    };
    gate.entered().await;
    tokio::time::advance(Duration::from_secs(6)).await;
    assert_eq!(pending.await.unwrap().err().unwrap().code, "FRAME_TIMEOUT");
    assert!(media.latest().is_none());
    *driver.capture_gate.lock().unwrap() = None;
    assert!(media.capture().await.is_ok());
    media.stop().await;
}
struct Fixture {
    core: Arc<Core>,
    store: Store,
    auth: Arc<Auth>,
    a: Identity,
    b: Identity,
    id: String,
    drivers: Arc<Mutex<HashMap<String, Arc<Driver>>>>,
    _dir: tempfile::TempDir,
}
impl Fixture {
    async fn new() -> Self {
        Self::limited(Limits::default(), 120000).await
    }
    async fn limited(limits: Limits, lease: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).await.unwrap();
        let auth = Arc::new(Auth::load(store.clone()).await.unwrap());
        let token = auth
            .create(
                json!({"name":"fixture","scopes":SCOPES,"devices":["*"]}),
                None,
            )
            .await
            .unwrap()
            .token
            .to_string();
        let a = auth.authenticate(&token, "a").unwrap();
        let b = auth.authenticate(&token, "b").unwrap();
        let core = Core::new(
            store.clone(),
            Secrets::open(dir.path().join("secrets")).await.unwrap(),
            auth.clone(),
            limits,
            lease,
        )
        .unwrap();
        let caps = Simulator::new(Layout::Us).capabilities().await.unwrap();
        let drivers = Arc::new(Mutex::new(HashMap::new()));
        let record = drivers.clone();
        core.register_factory(
            "fixture",
            Arc::new(move |device, _, _| {
                let driver = Driver::new(device.keyboard_layout, caps.clone());
                record
                    .lock()
                    .unwrap()
                    .insert(device.device_id.unwrap(), driver.clone());
                Ok(driver)
            }),
        );
        core.load().await.unwrap();
        assert!(core.list(&a).unwrap().is_empty());
        let device=core.save_device(&a,json!({"name":"Fixture","driver_id":"fixture","transport":"simulator","media_profile":"active","minimum_action_latency_ms":0}),None,None).await.unwrap();
        Self {
            core,
            store,
            auth,
            a,
            b,
            id: device["device_id"].as_str().unwrap().into(),
            drivers,
            _dir: dir,
        }
    }
    fn driver(&self) -> Arc<Driver> {
        self.drivers.lock().unwrap()[&self.id].clone()
    }
    async fn open(&self, mode: &str) -> ToolResult {
        self.core
            .tool(
                &self.a,
                "open_computer",
                json!({"computer_id":self.id,"mode":mode,"request_id":format!("open-{mode}")}),
            )
            .await
    }
    async fn close(self) {
        self.core.shutdown().await;
        self.store.close().await.unwrap();
    }
}
fn session(result: &ToolResult) -> String {
    assert!(result.success(), "{}", result.metadata());
    result.body["session"]["session_id"]
        .as_str()
        .unwrap()
        .into()
}
fn reference(result: &ToolResult) -> Value {
    let info = &result.images[0].info;
    json!({"frame_id":info.frame_id,"view_id":info.view_id,"input_revision":info.input_revision})
}
fn batch(session: &str, reference: Value, id: &str, actions: Value) -> Value {
    json!({"session_id":session,"request_id":id,"reference":reference,"actions":actions,"observation":{"mode":"none"}})
}
fn code(result: &ToolResult) -> &str {
    result.body["error"]["code"].as_str().unwrap()
}

#[tokio::test]
async fn control_without_video_sends_only_requested_keyboard_and_mouse_input() {
    for fault in [1, 3] {
        let f = Fixture::new().await;
        let driver = f.driver();
        driver.capture_fault.store(fault, Ordering::Release);
        let open = f.open("observe").await;
        let id = session(&open);
        assert!(open.images.is_empty());
        assert_eq!(
            open.body["video_error"]["code"],
            if fault == 1 {
                "FRAME_TIMEOUT"
            } else {
                "NO_SIGNAL"
            }
        );
        assert!(f.core.get(&f.id).unwrap().lease().is_none());
        let acquired = f
            .core
            .tool(
                &f.a,
                "computer_control",
                json!({"session_id":id,"request_id":"acquire","operation":"acquire"}),
            )
            .await;
        assert!(acquired.success());
        assert_eq!(driver.executes.load(Ordering::Acquire), 0);
        let actions = json!([
            {"type":"keypress","keys":["ENTER"]},
            {"type":"move","x":32767,"y":0},
            {"type":"click","x":16384,"y":16384},
            {"type":"type","text":"no video"}
        ]);
        let input = batch(
            &id,
            acquired.body["control_reference"].clone(),
            "ordinary-input",
            actions,
        );
        let sent = f.core.tool(&f.a, "computer", input.clone()).await;
        assert!(sent.success(), "{}", sent.metadata());
        assert!(sent.images.is_empty());
        assert_eq!(driver.executes.load(Ordering::Acquire), 4);
        assert_eq!(
            *driver.actions.lock().unwrap(),
            vec![
                Action::Keypress {
                    keys: vec!["ENTER".into()]
                },
                Action::Move { x: 32767, y: 0 },
                Action::Click {
                    x: 16384,
                    y: 16384,
                    button: Default::default()
                },
                Action::Type {
                    text: "no video".into()
                },
            ]
        );
        assert_eq!(
            f.core.tool(&f.a, "computer", input).await.metadata(),
            sent.metadata()
        );
        assert_eq!(driver.executes.load(Ordering::Acquire), 4);
        assert_eq!(driver.disconnects.load(Ordering::Acquire), 0);
        f.core
            .tool(&f.a, "close_computer", json!({"session_id":id}))
            .await;
        assert!(f.core.get(&f.id).unwrap().lease().is_none());
        assert_eq!(f.core.get(&f.id).unwrap().media.consumer_count(), 0);
        f.close().await;
    }
}

#[tokio::test]
async fn no_video_control_still_requires_authority_and_exclusive_lease() {
    let f = Fixture::new().await;
    let driver = f.driver();
    driver.capture_fault.store(3, Ordering::Release);
    assert_eq!(
        code(
            &f.core
                .tool(
                    &f.a,
                    "open_computer",
                    json!({"computer_id":f.id,"mode":"control"})
                )
                .await
        ),
        "INVALID_ARGUMENT"
    );
    let observed = f.open("observe").await;
    let id = session(&observed);
    assert_eq!(
        code(
            &f.core
                .tool(
                    &f.a,
                    "computer",
                    batch(
                        &id,
                        observed.body["control_reference"].clone(),
                        "no-lease",
                        json!([{"type":"keypress","keys":["ENTER"]}])
                    )
                )
                .await
        ),
        "LEASE_REVOKED"
    );
    f.core.stop_control(&f.a, &f.id, false).await.unwrap();
    assert_eq!(code(&f.open("control").await), "CONTROL_SUSPENDED");
    f.core.stop_control(&f.a, &f.id, true).await.unwrap();
    let token = f
        .auth
        .create(
            json!({"name":"observe only","scopes":["video:read"],"devices":["*"]}),
            None,
        )
        .await
        .unwrap()
        .token
        .to_string();
    let viewer = f.auth.authenticate(&token, "viewer").unwrap();
    assert_eq!(
        code(
            &f.core
                .tool(
                    &viewer,
                    "open_computer",
                    json!({"computer_id":f.id,"mode":"control","request_id":"forbidden"})
                )
                .await
        ),
        "FORBIDDEN"
    );
    let opened = f
        .core
        .tool(
            &f.a,
            "open_computer",
            json!({"computer_id":f.id,"mode":"control","request_id":"permitted"}),
        )
        .await;
    session(&opened);
    assert_eq!(
        code(
            &f.core
                .tool(
                    &f.b,
                    "open_computer",
                    json!({"computer_id":f.id,"mode":"control","request_id":"busy"})
                )
                .await
        ),
        "CONTROL_BUSY"
    );
    assert_eq!(driver.executes.load(Ordering::Acquire), 0);
    f.close().await;
}

#[tokio::test]
async fn uncertain_input_without_video_is_released_and_never_replayed() {
    let f = Fixture::new().await;
    let driver = f.driver();
    driver.capture_fault.store(3, Ordering::Release);
    driver.execute_fault.store(true, Ordering::Release);
    let opened = f.open("control").await;
    let input = batch(
        &session(&opened),
        opened.body["control_reference"].clone(),
        "uncertain-input",
        json!([{"type":"keypress","keys":["ENTER"]}]),
    );
    let first = f.core.tool(&f.a, "computer", input.clone()).await;
    assert_eq!(code(&first), "EXECUTION_UNKNOWN");
    assert_eq!(first.body["actions"][0]["status"], "unknown");
    assert!(driver.releases.load(Ordering::Acquire) > 0);
    assert_eq!(
        f.core
            .tool(&f.a, "computer", input.clone())
            .await
            .metadata(),
        first.metadata()
    );
    let mut changed = input;
    changed["actions"] = json!([{"type":"keypress","keys":["ESC"]}]);
    assert_eq!(
        code(&f.core.tool(&f.a, "computer", changed).await),
        "REQUEST_CONFLICT"
    );
    assert_eq!(driver.executes.load(Ordering::Acquire), 1);
    f.close().await;
}

#[tokio::test]
async fn accepted_no_video_input_finishes_once_when_its_caller_disappears() {
    let f = Fixture::new().await;
    let driver = f.driver();
    driver.capture_fault.store(3, Ordering::Release);
    let opened = f.open("control").await;
    let id = session(&opened);
    let gate = Arc::new(Gate::default());
    *driver.execute_gate.lock().unwrap() = Some(gate.clone());
    let input = batch(
        &id,
        opened.body["control_reference"].clone(),
        "disconnected-input",
        json!([{"type":"keypress","keys":["ENTER"]}]),
    );
    let core = f.core.clone();
    let identity = f.a.clone();
    let args = input.clone();
    let caller = tokio::spawn(async move { core.tool(&identity, "computer", args).await });
    gate.entered().await;
    caller.abort();
    assert!(caller.await.err().unwrap().is_cancelled());
    gate.release.notify_one();
    assert!(f.core.tool(&f.a, "computer", input).await.success());
    assert_eq!(driver.executes.load(Ordering::Acquire), 1);
    f.core
        .tool(&f.a, "close_computer", json!({"session_id":id}))
        .await;
    assert!(f.core.get(&f.id).unwrap().lease().is_none());
    f.close().await;
}

#[tokio::test]
async fn no_video_input_validates_the_full_batch_view_and_revision() {
    let f = Fixture::new().await;
    let driver = f.driver();
    driver.capture_fault.store(3, Ordering::Release);
    let opened = f.open("control").await;
    let id = session(&opened);
    let reference = opened.body["control_reference"].clone();
    let invalid = batch(
        &id,
        reference.clone(),
        "outside",
        json!([{"type":"keypress","keys":["ENTER"]},{"type":"move","x":32768,"y":0}]),
    );
    assert_eq!(
        code(&f.core.tool(&f.a, "computer", invalid).await),
        "VIEW_CHANGED"
    );
    assert_eq!(driver.executes.load(Ordering::Acquire), 0);
    let first = f
        .core
        .tool(
            &f.a,
            "computer",
            batch(
                &id,
                reference.clone(),
                "first",
                json!([{"type":"keypress","keys":["ENTER"]}]),
            ),
        )
        .await;
    assert!(first.success());
    assert_ne!(
        first.body["control_reference"]["input_revision"],
        reference["input_revision"]
    );
    assert_eq!(
        code(
            &f.core
                .tool(
                    &f.a,
                    "computer",
                    batch(
                        &id,
                        reference,
                        "stale",
                        json!([{"type":"keypress","keys":["ENTER"]}])
                    )
                )
                .await
        ),
        "STALE_REFERENCE"
    );
    f.core.get(&f.id).unwrap().media.reconnect();
    assert_eq!(
        code(
            &f.core
                .tool(
                    &f.a,
                    "computer",
                    batch(
                        &id,
                        first.body["control_reference"].clone(),
                        "old-view",
                        json!([{"type":"keypress","keys":["ENTER"]}])
                    )
                )
                .await
        ),
        "VIEW_CHANGED"
    );
    assert_eq!(driver.executes.load(Ordering::Acquire), 1);
    f.close().await;
}

#[tokio::test]
async fn video_recovery_does_not_interrupt_normalized_control_input() {
    let f = Fixture::new().await;
    let driver = f.driver();
    driver.capture_fault.store(3, Ordering::Release);
    let opened = f.open("control").await;
    let id = session(&opened);
    let gate = Arc::new(Gate::default());
    *driver.execute_gate.lock().unwrap() = Some(gate.clone());
    let request = batch(
        &id,
        opened.body["control_reference"].clone(),
        "during-recovery",
        json!([{"type":"move","x":16384,"y":16384}]),
    );
    let core = f.core.clone();
    let identity = f.a.clone();
    let call = tokio::spawn(async move { core.tool(&identity, "computer", request).await });
    gate.entered().await;
    let runtime = f.core.get(&f.id).unwrap();
    let before = runtime.media.identity().0;
    runtime.media.ingest(driver.raw()).unwrap();
    assert_ne!(runtime.media.identity().0, before);
    gate.release.notify_one();
    let sent = call.await.unwrap();
    assert!(sent.success(), "{}", sent.metadata());
    assert_eq!(
        sent.body["control_reference"]["view_id"],
        opened.body["control_reference"]["view_id"]
    );
    *driver.execute_gate.lock().unwrap() = None;
    let next = f
        .core
        .tool(
            &f.a,
            "computer",
            batch(
                &id,
                sent.body["control_reference"].clone(),
                "still-no-video",
                json!([{"type":"keypress","keys":["ENTER"]}]),
            ),
        )
        .await;
    assert!(next.success());
    assert_eq!(driver.executes.load(Ordering::Acquire), 2);
    f.close().await;
}

#[tokio::test]
async fn six_native_tools_share_exclusive_control_and_identical_retries_never_send_twice() {
    let f = Fixture::new().await;
    let listed = f.core.tool(&f.a, "list_computers", json!({})).await;
    assert_eq!(listed.body["computers"].as_array().unwrap().len(), 1);
    let opened = f.open("control").await;
    let session = session(&opened);
    let busy = f
        .core
        .tool(
            &f.b,
            "open_computer",
            json!({"computer_id":f.id,"mode":"control","request_id":"other"}),
        )
        .await;
    assert_eq!(code(&busy), "CONTROL_BUSY");
    let request = batch(
        &session,
        reference(&opened),
        "click",
        json!([{"type":"click","x":10,"y":10}]),
    );
    let (a, b) = tokio::join!(
        f.core.tool(&f.a, "computer", request.clone()),
        f.core.tool(&f.a, "computer", request.clone())
    );
    assert!(a.success());
    assert_eq!(a.metadata(), b.metadata());
    assert_eq!(f.driver().executes.load(Ordering::Acquire), 1);
    assert_eq!(a.body["actions"][0]["status"], "sent");
    let mut conflict = request.clone();
    conflict["actions"][0]["x"] = 11.into();
    assert_eq!(
        code(&f.core.tool(&f.a, "computer", conflict).await),
        "REQUEST_CONFLICT"
    );
    let stale = f
        .core
        .tool(
            &f.a,
            "computer",
            batch(
                &session,
                reference(&opened),
                "stale",
                json!([{"type":"click","x":1,"y":1}]),
            ),
        )
        .await;
    assert_eq!(code(&stale), "STALE_REFERENCE");
    let fresh = f
        .core
        .tool(&f.a, "computer_screenshot", json!({"session_id":session}))
        .await;
    assert!(fresh.success());
    assert!(
        f.core
            .tool(
                &f.a,
                "computer_control",
                json!({"session_id":session,"request_id":"renew","operation":"renew"})
            )
            .await
            .success()
    );
    assert!(
        f.core
            .tool(&f.a, "close_computer", json!({"session_id":session}))
            .await
            .success()
    );
    assert!(f.core.get(&f.id).unwrap().lease().is_none());
    assert_eq!(
        code(
            &f.core
                .tool(&f.a, "computer_screenshot", json!({"session_id":session}))
                .await
        ),
        "SESSION_LOST"
    );
    f.close().await;
}

#[tokio::test]
async fn device_lists_preserve_creation_order_rather_than_sorting_uuids() {
    let f = Fixture::new().await;
    for (id, name) in [
        ("ffffffff-ffff-4fff-8fff-ffffffffffff", "Z first"),
        ("00000000-0000-4000-8000-000000000001", "A second"),
    ] {
        f.core.save_device(&f.a,json!({"device_id":id,"name":name,"driver_id":"fixture","transport":"simulator","media_profile":"active"}),None,None).await.unwrap();
    }
    assert_eq!(
        f.core
            .list(&f.a)
            .unwrap()
            .iter()
            .map(|device| device["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["Fixture", "Z first", "A second"]
    );
    f.close().await;
}

#[tokio::test]
async fn a_session_id_is_not_authority_and_revocation_synchronously_closes_its_sessions() {
    let f = Fixture::new().await;
    let open = f.open("control").await;
    let id = session(&open);
    assert_eq!(
        code(
            &f.core
                .tool(&f.b, "computer_screenshot", json!({"session_id":id}))
                .await
        ),
        "FORBIDDEN"
    );
    f.auth.revoke(&f.a.token_id).await.unwrap();
    assert!(f.core.sessions(&f.a).is_err());
    assert!(f.core.get(&f.id).unwrap().lease().is_none());
    assert_eq!(
        code(
            &f.core
                .tool(&f.a, "computer_screenshot", json!({"session_id":id}))
                .await
        ),
        "AUTH_FAILED"
    );
    f.close().await;
}

#[tokio::test]
async fn entire_batches_are_validated_before_any_input_or_image_side_effects() {
    let f = Fixture::new().await;
    let open = f.open("control").await;
    let id = session(&open);
    let invalid = batch(
        &id,
        reference(&open),
        "invalid",
        json!([{"type":"click","x":1,"y":1},{"type":"type","text":"ok🙂"}]),
    );
    assert_eq!(
        code(&f.core.tool(&f.a, "computer", invalid).await),
        "UNSUPPORTED_TEXT"
    );
    assert_eq!(f.driver().executes.load(Ordering::Acquire), 0);
    let bounds = batch(
        &id,
        reference(&open),
        "bounds",
        json!([{"type":"drag","path":[{"x":1,"y":1},{"x":128,"y":1}]}]),
    );
    assert_eq!(
        code(&f.core.tool(&f.a, "computer", bounds).await),
        "VIEW_CHANGED"
    );
    let screenshots = batch(
        &id,
        reference(&open),
        "images",
        json!([{"type":"screenshot"},{"type":"screenshot"},{"type":"screenshot"},{"type":"screenshot"},{"type":"screenshot"}]),
    );
    assert_eq!(
        code(&f.core.tool(&f.a, "computer", screenshots).await),
        "RESOURCE_LIMIT"
    );
    let waits = batch(
        &id,
        reference(&open),
        "waits",
        json!([{"type":"wait","duration_ms":5000},{"type":"wait","duration_ms":5000},{"type":"wait","duration_ms":5000},{"type":"wait","duration_ms":5000},{"type":"wait","duration_ms":5000},{"type":"wait","duration_ms":1}]),
    );
    assert_eq!(
        code(&f.core.tool(&f.a, "computer", waits).await),
        "RESOURCE_LIMIT"
    );
    assert_eq!(f.driver().executes.load(Ordering::Acquire), 0);
    f.close().await;
}

#[tokio::test]
async fn uncertain_input_stops_following_actions_and_is_cached_without_replay() {
    let f = Fixture::new().await;
    let open = f.open("control").await;
    let id = session(&open);
    let driver = f.driver();
    driver.execute_fault.store(true, Ordering::Release);
    let request = batch(
        &id,
        reference(&open),
        "uncertain",
        json!([{"type":"click","x":1,"y":1},{"type":"click","x":2,"y":2}]),
    );
    let failed = f.core.tool(&f.a, "computer", request.clone()).await;
    assert_eq!(code(&failed), "EXECUTION_UNKNOWN");
    assert_eq!(failed.body["actions"][0]["status"], "unknown");
    assert_eq!(failed.body["actions"][1]["status"], "not_started");
    assert!(
        f.core
            .get(&f.id)
            .unwrap()
            .last_input_completed_at()
            .is_some()
    );
    driver.execute_fault.store(false, Ordering::Release);
    let retry = f.core.tool(&f.a, "computer", request).await;
    assert_eq!(failed.metadata(), retry.metadata());
    assert_eq!(driver.executes.load(Ordering::Acquire), 1);
    assert!(driver.releases.load(Ordering::Acquire) > 0);
    f.close().await;
}

#[tokio::test]
async fn a_cancelled_caller_does_not_cancel_or_repeat_accepted_input() {
    let f = Fixture::new().await;
    let open = f.open("control").await;
    let id = session(&open);
    let driver = f.driver();
    let gate = Arc::new(Gate::default());
    *driver.execute_gate.lock().unwrap() = Some(gate.clone());
    let request = batch(
        &id,
        reference(&open),
        "cancelled",
        json!([{"type":"click","x":1,"y":1}]),
    );
    let core = f.core.clone();
    let identity = f.a.clone();
    let input = request.clone();
    let caller = tokio::spawn(async move { core.tool(&identity, "computer", input).await });
    gate.entered().await;
    caller.abort();
    assert!(caller.await.err().unwrap().is_cancelled());
    gate.release.notify_one();
    let result = f.core.tool(&f.a, "computer", request).await;
    assert!(result.success());
    assert_eq!(driver.executes.load(Ordering::Acquire), 1);
    f.close().await;
}

#[tokio::test]
async fn emergency_stop_aborts_running_input_and_resume_grants_no_lease() {
    let f = Fixture::new().await;
    let open = f.open("control").await;
    let id = session(&open);
    let driver = f.driver();
    let gate = Arc::new(Gate::default());
    *driver.execute_gate.lock().unwrap() = Some(gate.clone());
    let core = f.core.clone();
    let identity = f.a.clone();
    let request = batch(
        &id,
        reference(&open),
        "blocked",
        json!([{"type":"click","x":1,"y":1},{"type":"click","x":2,"y":2}]),
    );
    let input = tokio::spawn(async move { core.tool(&identity, "computer", request).await });
    gate.entered().await;
    f.core.stop_control(&f.b, &f.id, false).await.unwrap();
    let failed = input.await.unwrap();
    assert!(!failed.success());
    assert_eq!(failed.body["actions"][1]["status"], "not_started");
    let runtime = f.core.get(&f.id).unwrap();
    assert!(runtime.lease().is_none());
    assert!(runtime.suspended());
    assert!(runtime.last_input_completed_at().is_some());
    assert_eq!(
        code(
            &f.core
                .tool(
                    &f.a,
                    "computer_control",
                    json!({"session_id":id,"request_id":"acquire-stopped","operation":"acquire"})
                )
                .await
        ),
        "CONTROL_SUSPENDED"
    );
    f.core.stop_control(&f.b, &f.id, true).await.unwrap();
    assert!(runtime.lease().is_none());
    assert!(!runtime.suspended());
    f.close().await;
}

#[tokio::test]
async fn the_full_device_queue_rejects_overload_and_still_allows_release_inputs() {
    let f = Fixture::new().await;
    let open = f.open("control").await;
    let id = session(&open);
    let driver = f.driver();
    let gate = Arc::new(Gate::default());
    *driver.execute_gate.lock().unwrap() = Some(gate.clone());
    let core = f.core.clone();
    let identity = f.a.clone();
    let request = batch(
        &id,
        reference(&open),
        "blocked",
        json!([{"type":"click","x":1,"y":1}]),
    );
    let running = tokio::spawn(async move { core.tool(&identity, "computer", request).await });
    gate.entered().await;
    let mut tasks = tokio::task::JoinSet::new();
    for n in 0..63 {
        let core = f.core.clone();
        let identity = f.a.clone();
        let id = id.clone();
        tasks.spawn(async move {
            core.tool(
                &identity,
                "computer_control",
                json!({"session_id":id,"request_id":format!("renew-{n}"),"operation":"renew"}),
            )
            .await
        });
    }
    let runtime = f.core.get(&f.id).unwrap();
    until(|| runtime.pending() == 64).await;
    let overflow = json!({"session_id":id,"request_id":"overflow","operation":"renew"});
    assert_eq!(
        code(
            &f.core
                .tool(&f.a, "computer_control", overflow.clone())
                .await
        ),
        "RESOURCE_LIMIT"
    );
    tokio::time::timeout(Duration::from_secs(3), f.core.release_inputs(&f.a, &id))
        .await
        .unwrap()
        .unwrap();
    assert!(!running.await.unwrap().success());
    while let Some(result) = tasks.join_next().await {
        assert!(result.unwrap().success());
    }
    assert_eq!(
        code(&f.core.tool(&f.a, "computer_control", overflow).await),
        "RESOURCE_LIMIT"
    );
    assert_eq!(driver.executes.load(Ordering::Acquire), 1);
    f.close().await;
}

#[tokio::test]
async fn independent_devices_make_progress_while_another_device_is_blocked() {
    let f = Fixture::new().await;
    let second=f.core.save_device(&f.a,json!({"name":"Second","driver_id":"fixture","transport":"simulator","minimum_action_latency_ms":0}),None,None).await.unwrap();
    let first = f.open("control").await;
    let first_id = session(&first);
    let second_open = f
        .core
        .tool(
            &f.a,
            "open_computer",
            json!({"computer_id":second["device_id"],"mode":"control","request_id":"second"}),
        )
        .await;
    let second_id = session(&second_open);
    let gate = Arc::new(Gate::default());
    *f.driver().execute_gate.lock().unwrap() = Some(gate.clone());
    let core = f.core.clone();
    let identity = f.a.clone();
    let request = batch(
        &first_id,
        reference(&first),
        "first-input",
        json!([{"type":"click","x":1,"y":1}]),
    );
    let running = tokio::spawn(async move { core.tool(&identity, "computer", request).await });
    gate.entered().await;
    let other = tokio::time::timeout(
        Duration::from_secs(1),
        f.core.tool(
            &f.a,
            "computer",
            batch(
                &second_id,
                reference(&second_open),
                "second-input",
                json!([{"type":"click","x":1,"y":1}]),
            ),
        ),
    )
    .await
    .unwrap();
    assert!(other.success());
    gate.release.notify_one();
    assert!(running.await.unwrap().success());
    f.close().await;
}

#[tokio::test]
async fn a_full_journal_can_release_and_close_without_forgetting_or_reusing_old_ids() {
    let f = Fixture::new().await;
    let open = f.open("control").await;
    let id = session(&open);
    for n in 0..256 {
        assert!(
            f.core
                .tool(
                    &f.a,
                    "computer_control",
                    json!({"session_id":id,"request_id":n.to_string(),"operation":"renew"})
                )
                .await
                .success()
        );
    }
    assert!(
        f.core
            .tool(
                &f.a,
                "computer_control",
                json!({"session_id":id,"request_id":"0","operation":"renew"})
            )
            .await
            .success()
    );
    assert_eq!(
        code(
            &f.core
                .tool(
                    &f.a,
                    "computer_control",
                    json!({"session_id":id,"request_id":"0","operation":"acquire"})
                )
                .await
        ),
        "REQUEST_CONFLICT"
    );
    let release = f
        .core
        .tool(
            &f.a,
            "computer_control",
            json!({"session_id":id,"request_id":"release","operation":"release"}),
        )
        .await;
    assert!(release.success());
    assert_eq!(release.body["session_closed"], true);
    assert!(f.core.get(&f.id).unwrap().lease().is_none());
    f.close().await;
}

#[tokio::test]
async fn opening_reservations_limit_concurrent_opens_and_failed_opens_release_capacity() {
    let f = Fixture::limited(
        Limits {
            max_sessions: 2,
            ..Limits::default()
        },
        120000,
    )
    .await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let core = f.core.clone();
        let identity = f.a.clone();
        let id = f.id.clone();
        tasks.spawn(async move {
            core.tool(
                &identity,
                "open_computer",
                json!({"computer_id":id,"mode":"observe"}),
            )
            .await
        });
    }
    let mut opened = Vec::new();
    while let Some(result) = tasks.join_next().await {
        let result = result.unwrap();
        if result.success() {
            opened.push(session(&result));
        } else {
            assert_eq!(code(&result), "RESOURCE_LIMIT");
        }
    }
    assert_eq!(opened.len(), 2);
    for id in opened {
        f.core.close_session(&id).await;
    }
    assert_eq!(
        code(
            &f.core
                .tool(
                    &f.a,
                    "open_computer",
                    json!({"computer_id":"missing","mode":"observe"})
                )
                .await
        ),
        "DEVICE_OFFLINE"
    );
    assert!(f.open("observe").await.success());
    f.close().await;
}

#[tokio::test]
async fn open_request_journals_are_partitioned_by_token_and_bounded_by_client_budget() {
    let f = Fixture::limited(
        Limits {
            max_open_clients: 2,
            ..Limits::default()
        },
        120000,
    )
    .await;
    let first = f.open("observe").await;
    let first_id = session(&first);
    let token = f
        .auth
        .create(
            json!({"name":"other","scopes":["devices:read","video:read"],"devices":["*"]}),
            None,
        )
        .await
        .unwrap();
    let other = f.auth.authenticate(&token.token, "a").unwrap();
    let second = f
        .core
        .tool(
            &other,
            "open_computer",
            json!({"computer_id":f.id,"mode":"observe","request_id":"open-observe"}),
        )
        .await;
    assert!(second.success());
    assert_ne!(first_id, session(&second));
    assert_eq!(
        code(
            &f.core
                .tool(
                    &f.b,
                    "open_computer",
                    json!({"computer_id":f.id,"mode":"observe","request_id":"budget"})
                )
                .await
        ),
        "RESOURCE_LIMIT"
    );
    f.core.disconnect_client(&other).await;
    assert!(
        f.core
            .tool(
                &f.b,
                "open_computer",
                json!({"computer_id":f.id,"mode":"observe","request_id":"budget"})
            )
            .await
            .success()
    );
    f.close().await;
}

#[tokio::test]
async fn replay_eviction_preserves_action_status_and_never_resends_input() {
    let f = Fixture::limited(
        Limits {
            replay_bytes: 1,
            ..Limits::default()
        },
        120000,
    )
    .await;
    let open = f.open("control").await;
    let id = session(&open);
    let mut request = batch(
        &id,
        reference(&open),
        "with-frame",
        json!([{"type":"click","x":1,"y":1}]),
    );
    request.as_object_mut().unwrap().remove("observation");
    let original = f.core.tool(&f.a, "computer", request.clone()).await;
    assert!(original.success());
    assert_eq!(original.images.len(), 1);
    let other = f
        .core
        .tool(
            &f.b,
            "open_computer",
            json!({"computer_id":f.id,"mode":"observe","request_id":"other-frame"}),
        )
        .await;
    assert!(other.success());
    let replay = f.core.tool(&f.a, "computer", request).await;
    assert!(replay.success());
    assert!(replay.images.is_empty());
    assert_eq!(replay.body["replay_image_evicted"], true);
    assert_eq!(replay.body["actions"], original.body["actions"]);
    assert_eq!(f.driver().executes.load(Ordering::Acquire), 1);
    f.close().await;
}

#[tokio::test]
async fn temporary_video_timeouts_preserve_control_but_disconnect_revokes_it() {
    let f = Fixture::new().await;
    let open = f.open("control").await;
    let id = session(&open);
    let runtime = f.core.get(&f.id).unwrap();
    let lease = runtime.lease().unwrap();
    let driver = f.driver();
    driver.capture_fault.store(1, Ordering::Release);
    let result = f
        .core
        .tool(&f.a, "computer_screenshot", json!({"session_id":id}))
        .await;
    assert_eq!(code(&result), "FRAME_TIMEOUT");
    assert_eq!(runtime.lease().unwrap().generation, lease.generation);
    assert_eq!(driver.connects.load(Ordering::Acquire), 1);
    let _ = driver.events.send(DriverEvent::DisconnectedAt(99));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(runtime.lease().unwrap().generation, lease.generation);
    let _ = driver.events.send(DriverEvent::DisconnectedAt(0));
    until(|| runtime.lease().is_none()).await;
    driver.capture_fault.store(0, Ordering::Release);
    f.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_open_results_obey_the_replay_budget_without_recapturing_on_retry() {
    let f = Fixture::limited(
        Limits {
            replay_bytes: 1,
            ..Limits::default()
        },
        120000,
    )
    .await;
    let ready = Arc::new(tokio::sync::Barrier::new(16));
    let mut calls = Vec::new();
    for n in 0..16 {
        let core = f.core.clone();
        let identity = f.a.clone();
        let request =
            json!({"computer_id":f.id,"mode":"observe","request_id":format!("parallel-{n}")});
        let ready = ready.clone();
        calls.push(tokio::spawn(async move {
            ready.wait().await;
            let result = core.tool(&identity, "open_computer", request.clone()).await;
            assert!(result.success(), "{}", result.metadata());
            request
        }));
    }
    let mut requests = Vec::new();
    for call in calls {
        requests.push(call.await.unwrap());
    }
    // Isolate retry effects from the normal session-driven preview polling.
    let media = f.core.get(&f.id).unwrap().media.clone();
    media.pause().await;
    media.capture().await.unwrap();
    let captures = f.driver().snapshots.load(Ordering::Acquire);
    let mut retained = 0;
    for request in requests {
        let replay = f.core.tool(&f.a, "open_computer", request).await;
        assert!(replay.success());
        if replay.images.is_empty() {
            assert_eq!(replay.body["replay_image_evicted"], true);
            assert_eq!(replay.body["frames"].as_array().unwrap().len(), 1);
        } else {
            retained += 1;
        }
    }
    assert_eq!(retained, 1);
    assert_eq!(f.driver().snapshots.load(Ordering::Acquire), captures);
    assert_eq!(f.driver().executes.load(Ordering::Acquire), 0);
    f.close().await;
}

#[tokio::test]
async fn the_final_observation_cannot_extend_the_action_batch_deadline() {
    let f = Fixture::new().await;
    let open = f.open("control").await;
    let driver = f.driver();
    let input = Arc::new(Gate::default());
    let capture = Arc::new(Gate::default());
    *driver.execute_gate.lock().unwrap() = Some(input.clone());
    *driver.capture_gate.lock().unwrap() = Some(capture.clone());
    let mut request = batch(
        &session(&open),
        reference(&open),
        "bounded-final-observation",
        json!([{"type":"click","x":1,"y":1}]),
    );
    request["observation"] = json!({"mode":"after_actions","timeout_ms":10000});
    let core = f.core.clone();
    let identity = f.a.clone();
    tokio::time::pause();
    let start = tokio::time::Instant::now();
    let call = tokio::spawn(async move { core.tool(&identity, "computer", request).await });
    input.entered().await;
    tokio::time::advance(Duration::from_secs(29)).await;
    input.release.notify_one();
    capture.entered().await;
    tokio::time::advance(Duration::from_millis(1001)).await;
    let result = call.await.unwrap();
    assert_eq!(code(&result), "FRAME_TIMEOUT");
    assert_eq!(result.body["actions"][0]["status"], "sent");
    assert!(start.elapsed() < Duration::from_secs(31));
    assert_eq!(driver.executes.load(Ordering::Acquire), 1);
    capture.release.notify_one();
    tokio::time::resume();
    f.close().await;
}

#[tokio::test]
async fn minimum_latency_is_a_fixed_deadline_after_the_last_input() {
    let f = Fixture::new().await;
    let mut config = f.core.get(&f.id).unwrap().device.clone();
    config.minimum_action_latency_ms = 200;
    f.core
        .save_device(&f.a, serde_json::to_value(config).unwrap(), None, None)
        .await
        .unwrap();
    let open = f.open("control").await;
    let id = session(&open);
    let request = batch(
        &id,
        reference(&open),
        "timed",
        json!([{"type":"type","text":"W"},{"type":"click","x":1,"y":1}]),
    );
    assert!(
        f.core
            .tool(&f.a, "computer", request.clone())
            .await
            .success()
    );
    let runtime = f.core.get(&f.id).unwrap();
    let completed = runtime.last_input_completed_at().unwrap();
    assert!(f.core.tool(&f.a, "computer", request).await.success());
    assert_eq!(runtime.last_input_completed_at(), Some(completed));
    let observed = f
        .core
        .tool(&f.a, "computer_screenshot", json!({"session_id":id}))
        .await;
    assert!(observed.success());
    assert!(observed.images[0].info.received_at_monotonic_ms - completed >= 199.0);
    assert_eq!(runtime.last_input_completed_at(), Some(completed));
    let later = tokio::time::Instant::now();
    assert!(
        f.core
            .tool(&f.a, "computer_screenshot", json!({"session_id":id}))
            .await
            .success()
    );
    assert!(later.elapsed() < Duration::from_millis(180));
    f.close().await;
}

#[tokio::test]
async fn standalone_simulator_produces_a_real_bounded_png_with_visual_marker_and_action_counter() {
    let simulator = Simulator::new(Layout::Us);
    simulator.connect(CancellationToken::new()).await.unwrap();
    let frame = simulator.snapshot(CancellationToken::new()).await.unwrap();
    assert_eq!((frame.width, frame.height), (1280, 720));
    assert_eq!(frame.mime_type, Mime::Png);
    assert!(frame.data.len() < 8 * 1024 * 1024);
    let decoder = png::Decoder::new(Cursor::new(frame.data.as_ref()));
    let mut reader = decoder.read_info().unwrap();
    let mut bytes = vec![0u8; reader.output_buffer_size().unwrap()];
    reader.next_frame(&mut bytes).unwrap();
    assert_eq!((reader.info().width, reader.info().height), (1280, 720));
    assert_eq!(simulator.marker().len(), 8);
    assert!(bytes.contains(&2));
    let context = ActionContext {
        cancellation: CancellationToken::new(),
        width: 1280,
        height: 720,
        assert_control: Arc::new(|| Ok(())),
    };
    simulator
        .execute(
            &Action::Type {
                text: "fixture".into(),
            },
            &context,
        )
        .await
        .unwrap();
    assert_eq!(simulator.action_count(), 1);
    simulator.disconnect().await.unwrap();
}

async fn media_fixture() -> (Arc<Media>, Arc<Driver>) {
    let driver = Driver::new(
        Layout::Us,
        Simulator::new(Layout::Us).capabilities().await.unwrap(),
    );
    driver.connect(CancellationToken::new()).await.unwrap();
    let media = Media::new("fixture".into(), driver.clone(), CancellationToken::new());
    media.reconnect();
    (media, driver)
}

#[tokio::test]
async fn simultaneous_screenshots_share_a_capture_and_keep_immutable_revision_metadata() {
    let (media, driver) = media_fixture().await;
    let gate = Arc::new(Gate::default());
    *driver.capture_gate.lock().unwrap() = Some(gate.clone());
    let first = {
        let media = media.clone();
        tokio::spawn(async move {
            media
                .screenshot(Duration::ZERO, Duration::from_secs(3), false)
                .await
        })
    };
    gate.entered().await;
    let second = media.screenshot(Duration::ZERO, Duration::from_secs(3), false);
    tokio::pin!(second);
    assert!(futures_util::poll!(second.as_mut()).is_pending());
    gate.release.notify_one();
    let first = first.await.unwrap().unwrap();
    let second = second.await.unwrap();
    assert_eq!(first.info.frame_id, second.info.frame_id);
    assert_eq!(driver.snapshots.load(Ordering::Acquire), 1);
    media.bump_input_revision();
    assert_eq!(Media::age(&first, false).info.input_revision, 0);
    assert_eq!(first.info.input_revision, 0);
    media.stop().await;
}

#[tokio::test]
async fn reconnect_discards_an_older_in_flight_capture_and_shape_changes_invalidate_views() {
    let (media, driver) = media_fixture().await;
    let gate = Arc::new(Gate::default());
    *driver.capture_gate.lock().unwrap() = Some(gate.clone());
    let pending = {
        let media = media.clone();
        tokio::spawn(async move { media.capture().await })
    };
    gate.entered().await;
    media.reconnect();
    assert_eq!(pending.await.unwrap().err().unwrap().code, "VIEW_CHANGED");
    assert!(media.latest().is_none());
    *driver.capture_gate.lock().unwrap() = None;
    let first = media.capture().await.unwrap();
    *driver.shape.lock().unwrap() = (64, 36);
    let second = media.capture().await.unwrap();
    assert_ne!(first.info.view_id, second.info.view_id);
    assert_eq!(
        first.info.connection_generation,
        second.info.connection_generation
    );
    media.stop().await;
}

#[tokio::test]
async fn a_capture_started_before_input_keeps_its_old_revision() {
    let (media, driver) = media_fixture().await;
    let gate = Arc::new(Gate::default());
    *driver.capture_gate.lock().unwrap() = Some(gate.clone());
    let pending = {
        let media = media.clone();
        tokio::spawn(async move { media.capture().await })
    };
    gate.entered().await;
    media.bump_input_revision();
    gate.release.notify_one();
    let old = pending.await.unwrap().unwrap();
    assert_eq!(old.info.input_revision, 0);
    assert_eq!(media.input_revision(), 1);
    *driver.capture_gate.lock().unwrap() = None;
    let fresh = media
        .screenshot(Duration::from_secs(5), Duration::from_secs(1), false)
        .await
        .unwrap();
    assert_eq!(fresh.info.input_revision, 1);
    assert_ne!(old.info.frame_id, fresh.info.frame_id);
    media.stop().await;
}

#[tokio::test]
async fn media_signal_freshness_budgets_and_latest_only_subscribers_are_bounded() {
    let (media, driver) = media_fixture().await;
    driver.source_time.store(false, Ordering::Release);
    let frame = media.capture().await.unwrap();
    assert_eq!(Media::age(&frame, true).info.after_action, "unverified");
    assert_eq!(frame.info.freshness_basis, "receive_time_only");
    let mut receiver = media.subscribe();
    let mut last = frame.clone();
    for _ in 0..20 {
        last = media.ingest(driver.raw()).unwrap();
    }
    assert_eq!(
        receiver.borrow_and_update().as_ref().unwrap().info.frame_id,
        last.info.frame_id
    );
    let mut raw = driver.raw();
    raw.width = 16_777_216;
    raw.height = 2;
    assert_eq!(media.ingest(raw).err().unwrap().code, "RESOURCE_LIMIT");
    let mut raw = driver.raw();
    raw.data = vec![0u8; 8 * 1024 * 1024 + 1].into();
    assert_eq!(media.ingest(raw).err().unwrap().code, "RESOURCE_LIMIT");
    *driver.signal.lock().unwrap() = Signal::Absent;
    assert_eq!(
        media
            .screenshot(Duration::ZERO, Duration::from_secs(1), false)
            .await
            .err()
            .unwrap()
            .code,
        "NO_SIGNAL"
    );
    media.stop().await;
}

#[tokio::test]
async fn benchmark_selects_a_shared_stream_with_hysteresis_and_stop_releases_consumers() {
    let (media, driver) = media_fixture().await;
    driver.stream.store(true, Ordering::Release);
    let gate = Arc::new(Gate::default());
    *driver.capture_gate.lock().unwrap() = Some(gate.clone());
    let benchmark = {
        let media = media.clone();
        tokio::spawn(async move { media.benchmark().await })
    };
    // Five deliberately slow snapshots make the 2ms stream preferable.
    for count in 1..=5 {
        until(|| gate.entered.load(Ordering::Acquire) >= count).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        gate.release.notify_one();
    }
    let measured = benchmark.await.unwrap().unwrap();
    assert_eq!(measured["strategy"], "mjpeg");
    assert_eq!(measured["hysteresis"], 0.2);
    assert!(media.strategy() == Strategy::Mjpeg);
    media.retain("viewer", kvmhelm::contracts::Profile::Active);
    let frame = media
        .screenshot(Duration::ZERO, Duration::from_secs(1), true)
        .await
        .unwrap();
    assert_eq!(frame.info.device_id, "fixture");
    media.stop().await;
    assert_eq!(media.consumer_count(), 0);
    assert!(media.latest().is_none());
}

#[tokio::test]
async fn credential_conflicts_device_replacement_and_deletion_preserve_secret_boundaries() {
    let f = Fixture::new().await;
    let device = f.core.get(&f.id).unwrap().device.clone();
    let saved = f
        .core
        .save_device(
            &f.a,
            serde_json::to_value(&device).unwrap(),
            Some(json!({"password":"fixture"})),
            Some(json!("fixture-ca")),
        )
        .await
        .unwrap();
    let current = f.core.get(&f.id).unwrap().device.clone();
    assert_eq!(saved["revision"], 2);
    assert_eq!(
        f.core
            .secrets
            .get(current.secret_ref.as_deref())
            .await
            .unwrap()["password"],
        "fixture"
    );
    assert_eq!(
        f.core
            .save_device(
                &f.a,
                serde_json::to_value(device).unwrap(),
                Some(json!({"password":"stale"})),
                None
            )
            .await
            .unwrap_err()
            .code,
        "REVISION_CONFLICT"
    );
    let mut forbidden = current.clone();
    forbidden.secret_ref = Some("foreign".into());
    assert_eq!(
        f.core
            .save_device(&f.a, serde_json::to_value(forbidden).unwrap(), None, None)
            .await
            .unwrap_err()
            .code,
        "FORBIDDEN"
    );
    f.open("observe").await;
    assert_eq!(
        f.core
            .delete_device(&f.a, &f.id, current.revision, false)
            .await
            .unwrap_err()
            .code,
        "CONFIRM_REQUIRED"
    );
    f.core
        .delete_device(&f.a, &f.id, current.revision, true)
        .await
        .unwrap();
    assert!(f.core.get(&f.id).is_err());
    assert!(
        f.core
            .secrets
            .get(current.secret_ref.as_deref())
            .await
            .is_err()
    );
    assert!(
        f.core
            .secrets
            .get(current.tls.ca_ref.as_deref())
            .await
            .is_err()
    );
    assert!(
        f.store
            .get::<Value>(Kind::Devices, &f.id)
            .await
            .unwrap()
            .is_none()
    );
    f.close().await;
}

#[tokio::test]
async fn event_history_is_bounded_and_restart_cursors_do_not_hide_new_events() {
    let f = Fixture::new().await;
    for _ in 0..140 {
        f.core.stop_control(&f.a, &f.id, false).await.unwrap();
    }
    let (history, _) = f.core.subscribe_events(Some("old-daemon"), u64::MAX);
    assert_eq!(history.len(), 256);
    assert!(
        history
            .iter()
            .all(|e| e.historical && e.instance_id == f.core.instance_id)
    );
    let cursor = history.last().unwrap().event_id;
    let (empty, mut live) = f.core.subscribe_events(Some(&f.core.instance_id), cursor);
    assert!(empty.is_empty());
    f.core.stop_control(&f.a, &f.id, true).await.unwrap();
    assert_eq!(live.recv().await.unwrap().kind, "control.resumed");
    f.close().await;
}

#[tokio::test]
async fn acquiring_control_requires_a_new_observation_and_cannot_relabel_an_old_frame() {
    let f = Fixture::new().await;
    let open = f.open("observe").await;
    let id = session(&open);
    let runtime = f.core.get(&f.id).unwrap();
    runtime.media.pause().await;
    let old = f
        .core
        .tool(&f.a, "computer_screenshot", json!({"session_id":id}))
        .await;
    let before = old.images[0].info.input_revision;
    assert!(
        f.core
            .tool(
                &f.a,
                "computer_control",
                json!({"session_id":id,"request_id":"acquire","operation":"acquire"})
            )
            .await
            .success()
    );
    assert_eq!(
        Media::age(&old.images[0], false).info.input_revision,
        before
    );
    let fresh = f
        .core
        .tool(
            &f.a,
            "computer_screenshot",
            json!({"session_id":id,"max_age_ms":5000}),
        )
        .await;
    assert_ne!(old.images[0].info.frame_id, fresh.images[0].info.frame_id);
    let mut relabel = reference(&old);
    relabel["input_revision"] = runtime.media.input_revision().into();
    assert_eq!(
        code(
            &f.core
                .tool(
                    &f.a,
                    "computer",
                    batch(
                        &id,
                        relabel,
                        "relabel",
                        json!([{"type":"click","x":1,"y":1}])
                    )
                )
                .await
        ),
        "STALE_REFERENCE"
    );
    f.close().await;
}

#[tokio::test]
async fn an_expired_lease_is_revoked_and_a_failed_input_release_suspends_control() {
    let f = Fixture::limited(Limits::default(), 100).await;
    let open = f.open("control").await;
    let id = session(&open);
    let runtime = f.core.get(&f.id).unwrap();
    let expiry = runtime.lease().unwrap().expires_at;
    tokio::time::sleep(Duration::from_millis(
        expiry.saturating_sub(kvmhelm::auth::now_ms()) + 5,
    ))
    .await;
    assert_eq!(
        code(
            &f.core
                .tool(
                    &f.a,
                    "computer_control",
                    json!({"session_id":id,"request_id":"expired","operation":"renew"})
                )
                .await
        ),
        "LEASE_EXPIRED"
    );
    f.core.sweep().await;
    assert!(runtime.lease().is_none());
    assert!(
        f.core
            .tool(
                &f.a,
                "computer_control",
                json!({"session_id":id,"request_id":"reacquire","operation":"acquire"})
            )
            .await
            .success()
    );
    let screenshot = f
        .core
        .tool(&f.a, "computer_screenshot", json!({"session_id":id}))
        .await;
    let driver = f.driver();
    driver.execute_fault.store(true, Ordering::Release);
    driver.release_fault.store(true, Ordering::Release);
    let failed = f
        .core
        .tool(
            &f.a,
            "computer",
            batch(
                &id,
                reference(&screenshot),
                "failed-release",
                json!([{"type":"click","x":1,"y":1}]),
            ),
        )
        .await;
    assert!(!failed.success());
    assert!(runtime.suspended());
    driver.release_fault.store(false, Ordering::Release);
    f.core.stop_control(&f.a, &f.id, false).await.unwrap();
    f.core.stop_control(&f.a, &f.id, true).await.unwrap();
    assert!(runtime.lease().is_none());
    assert!(!runtime.suspended());
    f.close().await;
}

#[tokio::test]
async fn live_capabilities_signal_state_and_mcp_image_envelopes_preserve_the_contract() {
    use base64::Engine;
    let f = Fixture::new().await;
    let open = f.open("observe").await;
    let packed = kvmhelm::mcp::pack(&open);
    assert_eq!(packed["isError"], false);
    assert_eq!(packed["content"][1]["mimeType"], "image/png");
    assert_eq!(
        packed["content"][1]["annotations"]["audience"],
        json!(["user", "assistant"])
    );
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(packed["content"][1]["data"].as_str().unwrap())
            .unwrap(),
        open.images[0].data.as_ref()
    );
    assert_eq!(
        serde_json::from_str::<Value>(packed["content"][0]["text"].as_str().unwrap()).unwrap(),
        packed["structuredContent"]
    );
    let runtime = f.core.get(&f.id).unwrap();
    let driver = f.driver();
    driver.stream.store(true, Ordering::Release);
    driver
        .events
        .send(DriverEvent::CapabilitiesChanged)
        .unwrap();
    until(|| runtime.describe()["capabilities"]["video"]["stream"] == true).await;
    for _ in 0..4 {
        let mut frame = driver.raw();
        frame.signal = Signal::Absent;
        runtime.media.ingest(frame).unwrap();
        assert_eq!(runtime.describe()["status"], "no_signal");
    }
    runtime.media.ingest(driver.raw()).unwrap();
    assert_eq!(runtime.describe()["status"], "ready");
    f.close().await;
}
