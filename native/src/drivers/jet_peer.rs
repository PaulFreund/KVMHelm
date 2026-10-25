//! One owned, bounded WebRTC/JSON-RPC event loop. str0m does no socket I/O;
//! every inbound source and outbound destination passes our LAN guard here.
use super::{
    ActionContext,
    h264::{Decoder, H264Assembler},
    rtc_lan::RtcLan,
};
use crate::{
    GatewayError, Result,
    audio::{OpusReceiver, Source},
    media::DriverFrame,
    network::{check_ice_candidate, check_ice_description},
    transport::Socket,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use str0m::{
    Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig,
    change::SdpPendingOffer,
    channel::ChannelId,
    media::{Direction, KeyframeRequestKind, MediaKind, Mid},
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

const RPC_TIMEOUT: Duration = Duration::from_secs(2);
type EventCallback = Arc<dyn Fn(&str, &Value) + Send + Sync>;
struct Request {
    method: &'static str,
    params: Value,
    context: Option<ActionContext>,
    done: oneshot::Sender<Result<Value>>,
    _slot: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}
struct Pending {
    request: Request,
    deadline: Instant,
}
pub(super) struct Peer {
    sender: mpsc::Sender<Request>,
    slots: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    pub frames: watch::Receiver<Option<DriverFrame>>,
    pub audio: Arc<Source>,
    ready: watch::Receiver<bool>,
    fault: watch::Receiver<Option<GatewayError>>,
    pub cancel: CancellationToken,
    worker: Mutex<Option<JoinHandle<()>>>,
}
fn offline() -> GatewayError {
    GatewayError::new("DEVICE_OFFLINE")
}
fn invalid_signal() -> GatewayError {
    GatewayError::message("DEVICE_OFFLINE", "Invalid JetKVM signaling")
}
fn has_error(value: &Value) -> bool {
    match value.get("error") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64() != Some(0.0),
        Some(Value::String(value)) => !value.is_empty(),
        _ => true,
    }
}
fn rtc_error(_: impl std::fmt::Display) -> GatewayError {
    offline()
}

impl Peer {
    pub async fn start(
        mut ws: Socket,
        device_id: String,
        callback: EventCallback,
        disconnected: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Arc<Self>> {
        let lan = RtcLan::new(&ws).await?;
        let mut rtc = RtcConfig::new()
            .clear_codecs()
            .enable_h264(true)
            .enable_opus(true, false)
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            .set_rtp_mode(true)
            .set_sctp_max_buffered_amount(65536)
            .build(Instant::now());
        lan.add_candidates(&mut rtc)?;
        let mut change = rtc.sdp_api();
        let channel = change.add_channel("rpc".into());
        let video = change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        let audio_mid = change.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
        let (offer, pending) = change.apply().ok_or_else(offline)?;
        let encoded = STANDARD.encode(serde_json::to_vec(&offer).map_err(rtc_error)?);
        let decoder = Decoder::start().await?;
        let frames = decoder.frames.clone();
        tokio::time::timeout(
            Duration::from_secs(5),
            ws.send(Message::Text(
                json!({"type":"offer","data":{"sd":encoded,"iceServers":[]}})
                    .to_string()
                    .into(),
            )),
        )
        .await
        .map_err(rtc_error)?
        .map_err(rtc_error)?;
        let (sender, receiver) = mpsc::channel(64);
        let (ready, ready_rx) = watch::channel(false);
        let (fault, fault_rx) = watch::channel(None);
        let cancel = CancellationToken::new();
        let lifetime = cancel.clone();
        let audio = Source::new(device_id, 8, 1);
        let audio_source = audio.clone();
        let worker = tokio::spawn(async move {
            let mut engine = EngineState {
                rtc,
                pending_offer: Some(pending),
                channel,
                video,
                audio_mid,
                audio: audio_source.clone(),
                opus: None,
                audio_available: false,
                open: false,
                candidates: 0,
                sequence: 0,
                pending: HashMap::new(),
                assembler: H264Assembler::default(),
                need_key: true,
                last_pli: None,
                video_ssrc: None,
                callback,
                ready,
            };
            if let Err(error) = run(&mut engine, ws, lan, receiver, &decoder, &lifetime).await {
                fault.send_replace(Some(error));
            }
            engine.pending.clear(); // All uncertain calls fail, never resend.
            engine.rtc.disconnect();
            audio_source.closed.cancel();
            decoder.close().await;
            if !lifetime.is_cancelled() {
                disconnected();
            }
            lifetime.cancel();
        });
        Ok(Arc::new(Self {
            sender,
            slots: Arc::new(Semaphore::new(64)),
            bytes: Arc::new(Semaphore::new(65536)),
            frames,
            audio,
            ready: ready_rx,
            fault: fault_rx,
            cancel,
            worker: Mutex::new(Some(worker)),
        }))
    }
    pub async fn wait_ready(&self, caller: &CancellationToken) -> Result<()> {
        let mut ready = self.ready.clone();
        let mut fault = self.fault.clone();
        tokio::select! { biased;
            _=caller.cancelled()=>Err(offline()), _=self.cancel.cancelled()=>Err(offline()),
            result=fault.wait_for(Option::is_some)=>Err(result.ok().and_then(|error|error.clone()).unwrap_or_else(offline)),
            result=tokio::time::timeout(Duration::from_secs(6), async {
                loop { if *ready.borrow_and_update() { return Ok(()); } ready.changed().await.map_err(rtc_error)?; }
            })=>result.map_err(|_|GatewayError::message("DEVICE_OFFLINE","JetKVM RPC channel did not open"))?,
        }
    }
    pub async fn call(
        &self,
        method: &'static str,
        params: Value,
        context: Option<&ActionContext>,
    ) -> Result<Value> {
        if self.cancel.is_cancelled() || !*self.ready.borrow() {
            return Err(offline());
        }
        if let Some(context) = context {
            context.check()?;
        }
        let length = serde_json::to_vec(&params).map_err(rtc_error)?.len() + method.len() + 128;
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| GatewayError::new("RESOURCE_LIMIT"))?;
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned(u32::try_from(length).map_err(rtc_error)?)
            .map_err(|_| GatewayError::new("RESOURCE_LIMIT"))?;
        let (done, wait) = oneshot::channel();
        self.sender
            .try_send(Request {
                method,
                params,
                context: context.cloned(),
                done,
                _slot: slot,
                _bytes: bytes,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => GatewayError::new("RESOURCE_LIMIT"),
                _ => offline(),
            })?;
        wait.await
            .unwrap_or_else(|_| Err(GatewayError::new("EXECUTION_UNKNOWN")))
    }
    pub async fn frame(&self, caller: &CancellationToken) -> Result<DriverFrame> {
        // Only frames published after capture begins qualify; a prior frame must
        // never be stamped with a later input revision by the shared media core.
        let mut frames = self.frames.clone();
        frames.borrow_and_update();
        tokio::select! { biased;
            _=caller.cancelled()=>Err(GatewayError::new("FRAME_TIMEOUT")),
            _=self.cancel.cancelled()=>Err(offline()),
            result=tokio::time::timeout(Duration::from_millis(1500), async {
                loop { frames.changed().await.map_err(rtc_error)?;
                    if let Some(frame) = frames.borrow_and_update().clone() { return Ok(frame); }
                }
            })=>result.map_err(|_|GatewayError::new("FRAME_TIMEOUT"))?,
        }
    }
    pub async fn close(&self) {
        self.cancel.cancel();
        self.audio.closed.cancel();
        let worker = self.worker.lock().unwrap().take();
        if let Some(mut worker) = worker
            && tokio::time::timeout(Duration::from_secs(3), &mut worker)
                .await
                .is_err()
        {
            worker.abort();
            let _ = worker.await;
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.audio.closed.cancel();
    }
}

struct EngineState {
    rtc: Rtc,
    pending_offer: Option<SdpPendingOffer>,
    channel: ChannelId,
    video: Mid,
    audio_mid: Mid,
    audio: Arc<Source>,
    opus: Option<OpusReceiver>,
    audio_available: bool,
    open: bool,
    candidates: usize,
    sequence: u64,
    pending: HashMap<u64, Pending>,
    assembler: H264Assembler,
    need_key: bool,
    last_pli: Option<Instant>,
    video_ssrc: Option<str0m::rtp::Ssrc>,
    callback: EventCallback,
    ready: watch::Sender<bool>,
}
impl EngineState {
    fn request_key(&mut self) {
        self.need_key = true;
        if self
            .last_pli
            .is_none_or(|last| last.elapsed() > Duration::from_millis(500))
            && let Some(ssrc) = self.video_ssrc
            && let Some(stream) = self.rtc.direct_api().stream_rx(&ssrc)
        {
            stream.request_keyframe(KeyframeRequestKind::Pli);
            self.last_pli = Some(Instant::now());
        }
    }
    fn signal(&mut self, data: &[u8]) -> Result<()> {
        let message: Value = serde_json::from_slice(data).map_err(|_| invalid_signal())?;
        if has_error(&message) {
            return Err(offline());
        }
        match message["type"].as_str() {
            Some("answer") => {
                let encoded = message["data"]
                    .as_str()
                    .filter(|s| s.len() <= 65536)
                    .ok_or_else(invalid_signal)?;
                let data = STANDARD.decode(encoded).map_err(|_| invalid_signal())?;
                let value: Value = serde_json::from_slice(&data).map_err(|_| invalid_signal())?;
                let sdp = value["sdp"].as_str().ok_or_else(invalid_signal)?;
                check_ice_description(sdp)?;
                let count = sdp
                    .lines()
                    .filter(|line| line.starts_with("a=candidate:"))
                    .count();
                self.candidates += count;
                if self.candidates > 256 || value["type"] != "answer" {
                    return Err(invalid_signal());
                }
                let answer = serde_json::from_value(value).map_err(|_| invalid_signal())?;
                let pending = self.pending_offer.take().ok_or_else(invalid_signal)?;
                self.rtc
                    .sdp_api()
                    .accept_answer(pending, answer)
                    .map_err(|_| {
                        GatewayError::message("DEVICE_OFFLINE", "JetKVM SDP answer rejected")
                    })?;
            }
            Some("new-ice-candidate") => {
                let Some(candidate) = message["data"]["candidate"].as_str() else {
                    return Ok(());
                };
                if candidate.is_empty() {
                    return Ok(());
                }
                check_ice_candidate(candidate)?;
                self.candidates += 1;
                if self.candidates > 256 {
                    return Err(GatewayError::new("RESOURCE_LIMIT"));
                }
                let candidate =
                    Candidate::from_sdp_string(candidate).map_err(|_| invalid_signal())?;
                // str0m retains candidates added before the SDP answer as well.
                self.rtc.add_remote_candidate(candidate);
            }
            _ => (),
        }
        Ok(())
    }
    fn rpc_message(&mut self, data: &[u8]) -> Result<()> {
        if data.len() > 65536 {
            return Err(GatewayError::new("RESOURCE_LIMIT"));
        }
        let Ok(value) = serde_json::from_slice::<Value>(data) else {
            return Ok(());
        };
        let id = value["id"].as_u64().or_else(|| {
            value["id"]
                .as_f64()
                .filter(|n| n.fract() == 0.0 && *n >= 0.0 && *n <= 9_007_199_254_740_991.0)
                .map(|n| n as u64)
        });
        if let Some(pending) = id.and_then(|id| self.pending.remove(&id)) {
            let result = if has_error(&value) {
                Err(GatewayError::message(
                    "EXECUTION_UNKNOWN",
                    "JetKVM rejected RPC",
                ))
            } else {
                Ok(value.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = pending.request.done.send(result);
        } else if let Some(method) = value["method"].as_str() {
            if method == "otherSessionConnected" {
                return Err(offline());
            }
            (self.callback)(method, &value["params"]);
        }
        Ok(())
    }
    fn event(&mut self, event: Event, decoder: &Decoder) -> Result<()> {
        match event {
            Event::ChannelOpen(id, label) if id == self.channel && label == "rpc" => {
                self.open = true;
                self.ready.send_replace(true);
            }
            Event::ChannelOpen(_, _) | Event::ChannelClose(_) | Event::Closed => {
                return Err(GatewayError::message(
                    "DEVICE_OFFLINE",
                    "JetKVM RPC channel closed",
                ));
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                return Err(GatewayError::message(
                    "DEVICE_OFFLINE",
                    "JetKVM ICE disconnected",
                ));
            }
            Event::ChannelData(data) if data.id == self.channel => {
                self.rpc_message(&data.data)?;
            }
            Event::RtpPacket(packet) => {
                let is_video = self
                    .rtc
                    .direct_api()
                    .stream_rx(&packet.header.ssrc)
                    .is_some_and(|stream| stream.mid() == self.video);
                if is_video {
                    if self.video_ssrc != Some(packet.header.ssrc) {
                        self.assembler = H264Assembler::default();
                        self.video_ssrc = Some(packet.header.ssrc);
                        self.request_key();
                    }
                    let result = self.assembler.push(
                        packet.header.sequence_number,
                        packet.header.timestamp,
                        packet.header.marker,
                        &packet.payload,
                    );
                    if result.lost {
                        self.request_key();
                    }
                    if let Some(unit) = result.unit {
                        if self.need_key && !unit.keyframe {
                            self.request_key();
                        } else {
                            if unit.keyframe {
                                self.need_key = false;
                            }
                            if !decoder.write(unit.data) {
                                self.request_key();
                            }
                        }
                    }
                } else if self
                    .rtc
                    .direct_api()
                    .stream_rx(&packet.header.ssrc)
                    .is_some_and(|s| s.mid() == self.audio_mid)
                {
                    if !self.audio_available {
                        self.audio_available = true;
                        (self.callback)("audioAvailable", &Value::Null);
                    }
                    if self.audio.users() > 0 {
                        if self.opus.is_none() {
                            self.opus = Some(OpusReceiver::new()?);
                        }
                        if let Some((data, gap)) = self
                            .opus
                            .as_mut()
                            .unwrap()
                            .decode(packet.header.sequence_number, &packet.payload)
                        {
                            self.audio.publish(data, 0, gap);
                        }
                    } else if let Some(decoder) = self.opus.as_mut() {
                        decoder.discontinuity();
                    }
                }
            }
            _ => (),
        }
        Ok(())
    }
    fn dispatch(&mut self, request: Request) {
        let checked = request
            .context
            .as_ref()
            .map_or(Ok(()), ActionContext::check);
        if let Err(error) = checked {
            let _ = request.done.send(Err(error));
            return;
        }
        if !self.open {
            let _ = request.done.send(Err(offline()));
            return;
        }
        self.sequence += 1;
        let id = self.sequence;
        let bytes =
            json!({"jsonrpc":"2.0","id":id,"method":request.method,"params":request.params})
                .to_string();
        let result = self
            .rtc
            .channel(self.channel)
            .ok_or_else(offline)
            .and_then(|mut c| {
                c.write(false, bytes.as_bytes())
                    .map_err(|_| GatewayError::new("EXECUTION_UNKNOWN"))
            });
        match result {
            Ok(true) => {
                self.pending.insert(
                    id,
                    Pending {
                        request,
                        deadline: Instant::now() + RPC_TIMEOUT,
                    },
                );
            }
            Ok(false) => {
                let _ = request.done.send(Err(GatewayError::new("RESOURCE_LIMIT")));
            }
            Err(error) => {
                let _ = request.done.send(Err(error));
            }
        }
    }
    fn expire(&mut self) {
        let expired = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= Instant::now())
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in expired {
            if let Some(p) = self.pending.remove(&id) {
                let _ = p.request.done.send(Err(GatewayError::message(
                    "EXECUTION_UNKNOWN",
                    "JetKVM RPC response timed out",
                )));
            }
        }
    }
}
async fn run(
    engine: &mut EngineState,
    mut ws: Socket,
    mut lan: RtcLan,
    mut requests: mpsc::Receiver<Request>,
    decoder: &Decoder,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    heartbeat.tick().await;
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_pong = Instant::now();
    loop {
        if cancel.is_cancelled() || !engine.rtc.is_alive() {
            return Ok(());
        }
        engine.expire();
        let timeout = loop {
            match engine.rtc.poll_output().map_err(rtc_error)? {
                Output::Timeout(timeout) => break timeout,
                Output::Event(event) => engine.event(event, decoder)?,
                Output::Transmit(transmit) => lan.transmit(&transmit)?,
            }
        };
        let timeout = timeout.min(
            engine
                .pending
                .values()
                .map(|p| p.deadline)
                .min()
                .unwrap_or(timeout),
        );
        tokio::select! {
            _=cancel.cancelled()=>return Ok(()),
            _=decoder.fault.cancelled()=>return Err(GatewayError::message("DEVICE_OFFLINE","JetKVM video decoder stopped")),
            _=tokio::time::sleep_until(timeout.into())=>engine.rtc.handle_input(Input::Timeout(Instant::now())).map_err(rtc_error)?,
            request=requests.recv()=>match request {Some(request)=>engine.dispatch(request),None=>return Ok(())},
            result=lan.receive(&mut engine.rtc)=>result?,
            incoming=ws.next()=>match incoming {
                Some(Ok(Message::Text(text)))=>engine.signal(text.as_bytes())?,
                Some(Ok(Message::Binary(bytes)))=>engine.signal(&bytes)?,
                Some(Ok(Message::Pong(_)))=>last_pong=Instant::now(),
                Some(Ok(Message::Ping(_)))=>{tokio::time::timeout(Duration::from_secs(2),ws.flush()).await.map_err(rtc_error)?.map_err(rtc_error)?;},
                Some(Ok(Message::Close(_)))|Some(Err(_))|None=>return Err(offline()),
                _=>(),
            },
            _=heartbeat.tick()=>{
                if last_pong.elapsed()>Duration::from_secs(30) { return Err(offline()); }
                tokio::time::timeout(Duration::from_secs(2),ws.send(Message::Ping(vec![].into()))).await.map_err(rtc_error)?.map_err(rtc_error)?;
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    fn engine() -> EngineState {
        let mut rtc = RtcConfig::new()
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            .build(Instant::now());
        let mut change = rtc.sdp_api();
        let channel = change.add_channel("rpc".into());
        let video = change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        let audio_mid = change.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
        let (_, pending) = change.apply().unwrap();
        EngineState {
            rtc,
            pending_offer: Some(pending),
            channel,
            video,
            audio_mid,
            audio: Source::new("fixture".into(), 8, 1),
            opus: None,
            audio_available: false,
            open: false,
            candidates: 0,
            sequence: 0,
            pending: HashMap::new(),
            assembler: H264Assembler::default(),
            need_key: true,
            last_pli: None,
            video_ssrc: None,
            callback: Arc::new(|_, _| ()),
            ready: watch::channel(false).0,
        }
    }
    #[test]
    fn public_sdp_and_trickled_candidates_are_rejected_before_rtc_mutation() {
        let mut e = engine();
        let candidate = "candidate:fixture 1 udp 1 8.8.8.8 443 typ host";
        let trickle = json!({"type":"new-ice-candidate","data":{"candidate":candidate}});
        assert_eq!(
            e.signal(trickle.to_string().as_bytes()).unwrap_err().code,
            "INVALID_ADDRESS"
        );
        let answer = json!({"type":"answer","data":STANDARD.encode(json!({"type":"answer","sdp":format!("a={candidate}\r\n")}).to_string())});
        assert_eq!(
            e.signal(answer.to_string().as_bytes()).unwrap_err().code,
            "INVALID_ADDRESS"
        );
        assert_eq!(e.candidates, 0);
        assert!(e.pending_offer.is_some());
    }
    #[tokio::test]
    async fn nullable_errors_numeric_ids_and_unsolicited_events_match_reference_dispatch() {
        let mut e = engine();
        let events = Arc::new(Mutex::new(Vec::new()));
        let received = events.clone();
        e.callback =
            Arc::new(move |method, value| received.lock().unwrap().push(json!([method, value])));
        for error in [Value::Null, json!(false), json!(0), json!("")] {
            let (done, wait) = oneshot::channel();
            let request = Request {
                method: "keyboardReport",
                params: Value::Null,
                context: None,
                done,
                _slot: Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap(),
                _bytes: Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap(),
            };
            e.pending.insert(
                7,
                Pending {
                    request,
                    deadline: Instant::now() + RPC_TIMEOUT,
                },
            );
            let text = format!("{{\"id\":7.0,\"result\":\"ack\",\"error\":{error}}}");
            e.rpc_message(text.as_bytes()).unwrap();
            assert_eq!(wait.await.unwrap().unwrap(), json!("ack"));
        }
        e.rpc_message(br#"{"id":999,"method":"usbState","params":"configured"}"#)
            .unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            vec![json!(["usbState", "configured"])]
        );
        assert_eq!(
            e.rpc_message(&vec![b' '; 65537]).unwrap_err().code,
            "RESOURCE_LIMIT"
        );
        assert!(has_error(&json!({"error":{"code":-1}})));
    }
    #[tokio::test]
    async fn control_is_rechecked_at_actual_channel_dispatch() {
        let mut e = engine();
        let allowed = Arc::new(AtomicBool::new(true));
        let check = allowed.clone();
        let context = ActionContext {
            cancellation: CancellationToken::new(),
            width: 640,
            height: 480,
            assert_control: Arc::new(move || {
                if check.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    Err(GatewayError::new("LEASE_REVOKED"))
                }
            }),
        };
        context.check().unwrap();
        let (done, wait) = oneshot::channel();
        let request = Request {
            method: "keyboardReport",
            params: json!({"modifier":0,"keys":[4]}),
            context: Some(context),
            done,
            _slot: Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap(),
            _bytes: Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap(),
        };
        allowed.store(false, Ordering::Release);
        e.dispatch(request);
        assert_eq!(wait.await.unwrap().unwrap_err().code, "LEASE_REVOKED");
        assert_eq!(e.sequence, 0);
        assert!(e.pending.is_empty());
    }
    #[tokio::test]
    async fn cancelled_rpc_waiters_do_not_free_accepted_queue_capacity() {
        let (sender, mut incoming) = mpsc::channel(64);
        let peer = Arc::new(Peer {
            sender,
            slots: Arc::new(Semaphore::new(64)),
            bytes: Arc::new(Semaphore::new(65536)),
            frames: watch::channel(None).1,
            audio: Source::new("fixture".into(), 8, 1),
            ready: watch::channel(true).1,
            fault: watch::channel(None).1,
            cancel: CancellationToken::new(),
            worker: Mutex::new(None),
        });
        let mut waiters = Vec::new();
        for _ in 0..64 {
            let p = peer.clone();
            waiters.push(tokio::spawn(async move {
                p.call("keyboardReport", json!({"keys":[4]}), None).await
            }));
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while incoming.len() != 64 {
                tokio::task::yield_now().await
            }
        })
        .await
        .unwrap();
        assert_eq!(
            peer.call("keyboardReport", json!({"keys":[5]}), None)
                .await
                .unwrap_err()
                .code,
            "RESOURCE_LIMIT"
        );
        for waiter in waiters {
            waiter.abort();
            let _ = waiter.await;
        }
        assert_eq!(
            peer.call("keyboardReport", json!({"keys":[5]}), None)
                .await
                .unwrap_err()
                .code,
            "RESOURCE_LIMIT"
        );
        while let Ok(request) = incoming.try_recv() {
            drop(request);
        }
        assert_eq!(peer.slots.available_permits(), 64);
        assert_eq!(peer.bytes.available_permits(), 65536);
    }
}
