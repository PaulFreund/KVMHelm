use crate::{
    GatewayError, Result,
    contracts::{Profile, Signal},
    drivers::KvmDriver,
    shared::{Flight, monotonic_ms, unknown},
};
use futures_util::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{broadcast, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub enum Mime {
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/jpeg")]
    Jpeg,
}
impl Mime {
    pub fn name(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
}

#[derive(Clone)]
pub struct DriverFrame {
    pub data: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    pub mime_type: Mime,
    pub source_timestamp: Option<f64>,
    pub signal: Signal,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FrameInfo {
    pub frame_id: String,
    pub view_id: String,
    pub device_id: String,
    pub connection_generation: u64,
    pub input_revision: i64,
    pub width: u32,
    pub height: u32,
    pub mime_type: Mime,
    pub source_timestamp: Option<f64>,
    pub received_at_monotonic_ms: f64,
    pub source_time_uncertainty_ms: Option<f64>,
    pub received_age_ms: f64,
    pub estimated_capture_age_ms: Option<f64>,
    pub freshness_basis: String,
    pub after_action: String,
    pub signal: Signal,
    pub stale: bool,
}
#[derive(Clone)]
pub struct Frame {
    pub data: Arc<[u8]>,
    pub info: FrameInfo,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Strategy {
    Snapshot,
    Mjpeg,
}
impl Strategy {
    pub fn name(self) -> &'static str {
        if self == Self::Snapshot {
            "snapshot"
        } else {
            "mjpeg"
        }
    }
}

#[derive(Default, Serialize)]
struct Metrics {
    snapshot_ms: VecDeque<f64>,
    frame_intervals_ms: VecDeque<f64>,
    frames: u64,
    dropped: u64,
    bytes: usize,
}
struct Capture {
    result: Arc<Flight<Frame>>,
    cancellation: CancellationToken,
}
struct Running {
    epoch: u64,
    cancellation: CancellationToken,
}
struct State {
    latest: Option<Frame>,
    view: String,
    generation: u64,
    revision: i64,
    shape: Option<(u32, u32, Mime)>,
    last_received: Option<f64>,
    consumers: BTreeMap<String, Profile>,
    strategy: Strategy,
    metrics: Metrics,
    pending: Option<Capture>,
    paused: bool,
    stopped: bool,
    running: Option<Running>,
    epoch: u64,
    benchmark: Option<Arc<Flight<Value>>>,
}
pub struct Media {
    device_id: String,
    driver: Arc<dyn KvmDriver>,
    budget: usize,
    state: Mutex<State>,
    latest: watch::Sender<Option<Frame>>,
    faults: broadcast::Sender<GatewayError>,
    worker: Mutex<Option<JoinHandle<()>>>,
    lifetime: CancellationToken,
}

impl Media {
    pub fn new(
        device_id: String,
        driver: Arc<dyn KvmDriver>,
        lifetime: CancellationToken,
    ) -> Arc<Self> {
        let (latest, _) = watch::channel(None);
        let (faults, _) = broadcast::channel(16);
        Arc::new(Self {
            device_id,
            driver,
            budget: 8 * 1024 * 1024,
            lifetime,
            latest,
            faults,
            worker: Mutex::new(None),
            state: Mutex::new(State {
                latest: None,
                view: uuid::Uuid::new_v4().to_string(),
                generation: 0,
                revision: 0,
                shape: None,
                last_received: None,
                consumers: BTreeMap::new(),
                strategy: Strategy::Snapshot,
                metrics: Metrics::default(),
                pending: None,
                paused: false,
                stopped: false,
                running: None,
                epoch: 0,
                benchmark: None,
            }),
        })
    }
    pub fn subscribe(&self) -> watch::Receiver<Option<Frame>> {
        self.latest.subscribe()
    }
    pub fn faults(&self) -> broadcast::Receiver<GatewayError> {
        self.faults.subscribe()
    }
    pub fn latest(&self) -> Option<Frame> {
        self.state.lock().unwrap().latest.clone()
    }
    pub fn identity(&self) -> (String, u64, i64) {
        let s = self.state.lock().unwrap();
        (s.view.clone(), s.generation, s.revision)
    }
    pub fn input_revision(&self) -> i64 {
        self.state.lock().unwrap().revision
    }
    pub fn bump_input_revision(&self) -> i64 {
        let mut s = self.state.lock().unwrap();
        s.revision += 1;
        s.revision
    }
    pub fn consumer_count(&self) -> usize {
        self.state.lock().unwrap().consumers.len()
    }
    pub fn dropped(&self) {
        self.state.lock().unwrap().metrics.dropped += 1;
    }
    pub fn describe(&self) -> Value {
        let s = self.state.lock().unwrap();
        json!({"strategy":s.strategy.name(),"metrics":s.metrics,"frame_age_ms":s.latest.as_ref().map(|f|(monotonic_ms()-f.info.received_at_monotonic_ms).max(0.0))})
    }
    pub fn consumers(&self) -> Value {
        json!(
            self.state
                .lock()
                .unwrap()
                .consumers
                .iter()
                .map(|(id, p)| json!({"id":id,"profile":p.name()}))
                .collect::<Vec<_>>()
        )
    }
    pub fn strategy(&self) -> Strategy {
        self.state.lock().unwrap().strategy
    }
    pub fn reconnect(&self) {
        let mut s = self.state.lock().unwrap();
        s.generation += 1;
        s.view = uuid::Uuid::new_v4().to_string();
        s.latest = None;
        s.shape = None;
        if let Some(pending) = s.pending.take() {
            pending.cancellation.cancel();
            pending
                .result
                .finish(Err(GatewayError::new("VIEW_CHANGED")));
        }
        if let Some(running) = &s.running {
            running.cancellation.cancel();
        }
        self.latest.send_replace(None);
    }
    pub fn retain(self: &Arc<Self>, id: &str, profile: Profile) {
        self.state
            .lock()
            .unwrap()
            .consumers
            .insert(id.into(), profile);
        self.ensure();
    }
    pub fn release(&self, id: &str) {
        let mut s = self.state.lock().unwrap();
        s.consumers.remove(id);
        if s.consumers.is_empty()
            && let Some(running) = &s.running
        {
            running.cancellation.cancel();
        }
    }
    pub fn ingest(&self, raw: DriverFrame) -> Result<Frame> {
        let (_, generation, revision) = self.identity();
        self.ingest_at(raw, generation, revision)
    }
    fn ingest_at(&self, raw: DriverFrame, generation: u64, revision: i64) -> Result<Frame> {
        if raw.data.len() > self.budget || u64::from(raw.width) * u64::from(raw.height) > 16_777_216
        {
            return Err(GatewayError::message(
                "RESOURCE_LIMIT",
                "Frame budget exceeded",
            ));
        }
        if raw.width == 0 || raw.height == 0 || raw.data.is_empty() {
            return Err(GatewayError::new("FRAME_TIMEOUT"));
        }
        let mut s = self.state.lock().unwrap();
        if s.stopped || self.lifetime.is_cancelled() {
            return Err(GatewayError::new("DEVICE_OFFLINE"));
        }
        if s.generation != generation {
            return Err(GatewayError::new("VIEW_CHANGED"));
        }
        let shape = (raw.width, raw.height, raw.mime_type);
        if s.shape != Some(shape) {
            s.view = uuid::Uuid::new_v4().to_string();
            s.shape = Some(shape);
        }
        let now = monotonic_ms();
        if let Some(last) = s.last_received {
            s.metrics.frame_intervals_ms.push_back(now - last);
            if s.metrics.frame_intervals_ms.len() > 128 {
                s.metrics.frame_intervals_ms.pop_front();
            }
        }
        s.last_received = Some(now);
        s.metrics.frames += 1;
        s.metrics.bytes = raw.data.len();
        let frame = Frame {
            data: raw.data,
            info: FrameInfo {
                frame_id: uuid::Uuid::new_v4().to_string(),
                view_id: s.view.clone(),
                device_id: self.device_id.clone(),
                connection_generation: s.generation,
                input_revision: revision,
                width: raw.width,
                height: raw.height,
                mime_type: raw.mime_type,
                source_timestamp: raw.source_timestamp,
                received_at_monotonic_ms: now,
                source_time_uncertainty_ms: raw.source_timestamp.map(|_| 0.0),
                received_age_ms: 0.0,
                estimated_capture_age_ms: None,
                freshness_basis: if raw.source_timestamp.is_some() {
                    "source_time"
                } else {
                    "receive_time_only"
                }
                .into(),
                after_action: "not_applicable".into(),
                signal: raw.signal,
                stale: false,
            },
        };
        s.latest = Some(frame.clone());
        self.latest.send_replace(Some(frame.clone()));
        Ok(frame)
    }
    pub fn age(frame: &Frame, after: bool) -> Frame {
        let mut result = frame.clone();
        let now = monotonic_ms();
        let age = (now - frame.info.received_at_monotonic_ms).max(0.0);
        result.info.received_age_ms = age;
        result.info.estimated_capture_age_ms =
            frame.info.source_timestamp.map(|t| (now - t).max(0.0));
        result.info.stale = age > 1500.0;
        result.info.after_action = if after {
            if frame.info.source_timestamp.is_some() {
                "verified"
            } else {
                "unverified"
            }
        } else {
            "not_applicable"
        }
        .into();
        result
    }
    pub async fn capture(self: &Arc<Self>) -> Result<Frame> {
        let result = {
            let mut s = self.state.lock().unwrap();
            if s.stopped || self.lifetime.is_cancelled() {
                return Err(GatewayError::new("DEVICE_OFFLINE"));
            }
            if let Some(pending) = &s.pending {
                pending.result.clone()
            } else {
                let result = Arc::new(Flight::new());
                let cancellation = self.lifetime.child_token();
                s.pending = Some(Capture {
                    result: result.clone(),
                    cancellation: cancellation.clone(),
                });
                let media = self.clone();
                let result_task = result.clone();
                let generation = s.generation;
                let revision = s.revision;
                tokio::spawn(async move {
                    let start = monotonic_ms();
                    let raw = tokio::select! {
                        biased;
                        _=cancellation.cancelled()=>Err(GatewayError::new("VIEW_CHANGED")),
                        result=tokio::time::timeout(Duration::from_secs(5),AssertUnwindSafe(media.driver.snapshot(cancellation.clone())).catch_unwind())=>match result {
                            Ok(Ok(value))=>value,Ok(Err(_))=>Err(unknown()),Err(_)=>Err(GatewayError::new("FRAME_TIMEOUT")),
                        },
                    };
                    cancellation.cancel();
                    let value = raw.and_then(|raw| media.ingest_at(raw, generation, revision));
                    {
                        let mut s = media.state.lock().unwrap();
                        s.metrics.snapshot_ms.push_back(monotonic_ms() - start);
                        if s.metrics.snapshot_ms.len() > 64 {
                            s.metrics.snapshot_ms.pop_front();
                        }
                        if s.pending
                            .as_ref()
                            .is_some_and(|p| Arc::ptr_eq(&p.result, &result_task))
                        {
                            s.pending = None;
                        }
                    }
                    result_task.finish(value);
                });
                result
            }
        };
        result.wait().await
    }
    pub async fn screenshot(
        self: &Arc<Self>,
        max_age: Duration,
        timeout: Duration,
        after: bool,
    ) -> Result<Frame> {
        let cutoff = monotonic_ms();
        {
            let s = self.state.lock().unwrap();
            if !after
                && let Some(latest) = &s.latest
                && latest.info.input_revision == s.revision
                && cutoff - latest.info.received_at_monotonic_ms <= max_age.as_secs_f64() * 1000.0
                && latest.info.signal != Signal::Absent
            {
                return Ok(Self::age(latest, false));
            }
        }
        let work = async {
            let mut receiver = self.subscribe();
            loop {
                let stream = {
                    let s = self.state.lock().unwrap();
                    s.strategy == Strategy::Mjpeg && s.running.is_some()
                };
                let frame = if stream {
                    loop {
                        if let Some(frame) = receiver.borrow_and_update().clone()
                            && frame.info.received_at_monotonic_ms >= cutoff
                        {
                            break frame;
                        }
                        receiver
                            .changed()
                            .await
                            .map_err(|_| GatewayError::new("DEVICE_OFFLINE"))?;
                    }
                } else {
                    let pending = {
                        let s = self.state.lock().unwrap();
                        s.pending
                            .as_ref()
                            .filter(|_| {
                                after
                                    || s.latest
                                        .as_ref()
                                        .is_none_or(|f| f.info.input_revision != s.revision)
                            })
                            .map(|p| p.result.clone())
                    };
                    if let Some(pending) = pending {
                        let frame = pending.wait().await?;
                        if after { self.capture().await? } else { frame }
                    } else {
                        self.capture().await?
                    }
                };
                if frame.info.input_revision != self.input_revision()
                    || (after && frame.info.received_at_monotonic_ms < cutoff)
                {
                    continue;
                }
                if frame.info.signal == Signal::Absent {
                    return Err(GatewayError::new("NO_SIGNAL"));
                }
                return Ok(Self::age(&frame, after));
            }
        };
        tokio::select! {
            biased;
            _=self.lifetime.cancelled()=>Err(GatewayError::new("DEVICE_OFFLINE")),
            result=tokio::time::timeout(timeout,work)=>result.unwrap_or_else(|_|Err(GatewayError::message("FRAME_TIMEOUT","No fresh complete frame received"))),
        }
    }
    fn ensure(self: &Arc<Self>) {
        let mut s = self.state.lock().unwrap();
        if s.stopped
            || s.paused
            || s.running.is_some()
            || s.consumers.is_empty()
            || self.lifetime.is_cancelled()
        {
            return;
        }
        s.epoch += 1;
        let epoch = s.epoch;
        let cancellation = self.lifetime.child_token();
        s.running = Some(Running {
            epoch,
            cancellation: cancellation.clone(),
        });
        let media = self.clone();
        let task = tokio::spawn(async move {
            let _ = AssertUnwindSafe(media.run(cancellation.clone()))
                .catch_unwind()
                .await;
            cancellation.cancel();
            {
                let mut s = media.state.lock().unwrap();
                if s.running.as_ref().is_some_and(|r| r.epoch == epoch) {
                    s.running = None;
                }
            }
            media.ensure();
        });
        *self.worker.lock().unwrap() = Some(task);
    }
    async fn run(self: &Arc<Self>, cancellation: CancellationToken) {
        let mut failures = 0u32;
        while !cancellation.is_cancelled() {
            let step = async {
                if self.strategy() == Strategy::Mjpeg {
                    let (_, generation, _) = self.identity();
                    let stream = self.driver.subscribe_video(cancellation.clone()).await?;
                    if let Some(mut stream) = stream {
                        while let Some(frame) = stream.next().await {
                            self.ingest_at(frame?, generation, self.input_revision())?;
                        }
                    } else {
                        return Err(GatewayError::new("UNSUPPORTED_ACTION"));
                    }
                } else {
                    self.capture().await?;
                    let fps = {
                        let s = self.state.lock().unwrap();
                        if s.consumers.values().any(|p| *p == Profile::Active) {
                            15
                        } else if s.consumers.values().any(|p| *p == Profile::Overview) {
                            5
                        } else {
                            1
                        }
                    };
                    tokio::time::sleep(Duration::from_millis(1000 / fps)).await;
                }
                Ok(())
            };
            let result = tokio::select! {biased;_=cancellation.cancelled()=>return,result=AssertUnwindSafe(step).catch_unwind()=>result.unwrap_or_else(|_|Err(unknown()))};
            if let Err(error) = result {
                let _ = self.faults.send(error);
                {
                    let mut s = self.state.lock().unwrap();
                    if s.strategy == Strategy::Mjpeg {
                        s.strategy = Strategy::Snapshot;
                    }
                }
                let backoff = 250u64.saturating_mul(1u64 << failures.min(6)).min(10000);
                failures = failures.saturating_add(1);
                tokio::select! {biased;_=cancellation.cancelled()=>return,_=tokio::time::sleep(Duration::from_millis(backoff))=>()}
            } else {
                failures = 0;
            }
        }
    }
    pub async fn pause(&self) {
        {
            let mut s = self.state.lock().unwrap();
            s.paused = true;
            if let Some(running) = &s.running {
                running.cancellation.cancel();
            }
        }
        let worker = self.worker.lock().unwrap().take();
        if let Some(worker) = worker {
            let _ = worker.await;
        }
    }
    pub fn resume(self: &Arc<Self>) {
        self.state.lock().unwrap().paused = false;
        self.ensure();
    }
    pub async fn stop(&self) {
        {
            let mut s = self.state.lock().unwrap();
            s.stopped = true;
            s.consumers.clear();
            if let Some(pending) = s.pending.take() {
                pending.cancellation.cancel();
                pending
                    .result
                    .finish(Err(GatewayError::new("DEVICE_OFFLINE")));
            }
        }
        self.pause().await;
        self.state.lock().unwrap().latest = None;
        self.latest.send_replace(None);
    }
    pub async fn benchmark(self: &Arc<Self>) -> Result<Value> {
        let flight = {
            let mut s = self.state.lock().unwrap();
            if let Some(flight) = &s.benchmark {
                flight.clone()
            } else {
                let flight = Arc::new(Flight::new());
                s.benchmark = Some(flight.clone());
                let media = self.clone();
                let complete = flight.clone();
                tokio::spawn(async move {
                    let value = AssertUnwindSafe(media.measure())
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|_| Err(unknown()));
                    media.state.lock().unwrap().benchmark = None;
                    complete.finish(value);
                });
                flight
            }
        };
        flight.wait().await
    }
    async fn measure(self: &Arc<Self>) -> Result<Value> {
        let mut snapshots = Vec::with_capacity(5);
        for _ in 0..5 {
            let start = monotonic_ms();
            self.capture().await?;
            snapshots.push(monotonic_ms() - start);
        }
        let cancellation = self.lifetime.child_token();
        let _cancel = cancellation.clone().drop_guard();
        let (_, generation, _) = self.identity();
        let stream = async {
            let Some(mut stream) = self.driver.subscribe_video(cancellation.clone()).await? else {
                return Ok::<Option<f64>, GatewayError>(None);
            };
            let start = monotonic_ms();
            let mut count = 0;
            while let Some(raw) = stream.next().await {
                self.ingest_at(raw?, generation, self.input_revision())?;
                count += 1;
                if count == 5 {
                    return Ok(Some((monotonic_ms() - start) / f64::from(count)));
                }
            }
            Ok(None)
        };
        let stream_ms = tokio::time::timeout(Duration::from_millis(2500), stream)
            .await
            .ok()
            .and_then(std::result::Result::ok)
            .flatten();
        let average = snapshots.iter().sum::<f64>() / snapshots.len() as f64;
        let strategy = if stream_ms.is_some_and(|v| v < average * 0.8) {
            Strategy::Mjpeg
        } else {
            Strategy::Snapshot
        };
        {
            let mut s = self.state.lock().unwrap();
            if s.strategy != strategy {
                s.strategy = strategy;
                if let Some(running) = &s.running {
                    running.cancellation.cancel();
                }
            }
        }
        Ok(
            json!({"snapshot_mean_ms":average,"stream_frame_interval_ms":stream_ms,"strategy":strategy.name(),"basis":"local receive timing; not target capture latency","hysteresis":0.2}),
        )
    }
}
