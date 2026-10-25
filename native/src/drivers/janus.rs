//! PiKVM's actual janus.plugin.ustreamer audio protocol. Probe/retry and media
//! have their own lifetime and cannot take down a working HID/video link.
use super::{ActionContext, AudioStream, rtc_lan::RtcLan};
use crate::{
    GatewayError, Result,
    audio::{Microphone, OpusReceiver, Source},
    contracts::AudioCapabilities,
    network::{check_ice_candidate, local_ice_description},
    transport::{LanTransport, Socket},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use str0m::{
    Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig,
    change::SdpOffer,
    format::Codec,
    media::{Direction, Mid, Pt},
    rtp::{RtpWrite, Ssrc},
};
use tokio::{sync::watch, task::JoinHandle};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

type Features = Arc<dyn Fn(AudioCapabilities) + Send + Sync>;
struct Shared {
    source: Arc<Source>,
    mic: Microphone,
    features: watch::Sender<Option<AudioCapabilities>>,
    callback: Features,
    generation: AtomicU64,
}
pub struct JanusAudio {
    shared: Arc<Shared>,
    cancel: CancellationToken,
    worker: Mutex<Option<JoinHandle<()>>>,
}
fn error(code: &str) -> GatewayError {
    GatewayError::new(code)
}
fn unavailable(code: &str) -> AudioCapabilities {
    AudioCapabilities {
        from_target: false,
        to_target: false,
        formats: vec![],
        reason: Some(code.into()),
    }
}
impl JanusAudio {
    pub fn start(transport: Arc<LanTransport>, device_id: String, callback: Features) -> Arc<Self> {
        let shared = Arc::new(Shared {
            source: Source::new(device_id, 10, 0),
            mic: Microphone::default(),
            features: watch::channel(None).0,
            callback,
            generation: AtomicU64::new(0),
        });
        let cancel = CancellationToken::new();
        let lifetime = cancel.clone();
        let state = shared.clone();
        let worker = tokio::spawn(async move {
            for attempt in 0..=3u32 {
                let result = session(&transport, &state, &lifetime).await;
                if lifetime.is_cancelled() {
                    break;
                }
                let code = result
                    .err()
                    .map(|e| e.code)
                    .unwrap_or_else(|| "AUDIO_DISCONNECTED".into());
                let _ = state.mic.set(false, None);
                state.source.fault(&code);
                let caps = unavailable(&code);
                state.features.send_replace(Some(caps.clone()));
                (state.callback)(caps);
                if attempt == 3 {
                    break;
                }
                tokio::select! {biased;_=lifetime.cancelled()=>break,_=tokio::time::sleep(Duration::from_secs(1u64<<attempt))=>()}
            }
        });
        Arc::new(Self {
            shared,
            cancel,
            worker: Mutex::new(Some(worker)),
        })
    }
    pub async fn probe(&self) -> Result<AudioCapabilities> {
        let mut features = self.shared.features.subscribe();
        tokio::select! {biased;
            _=self.cancel.cancelled()=>Err(error("AUDIO_DISCONNECTED")),
            result=tokio::time::timeout(Duration::from_secs(6),async{
                loop{if let Some(value)=features.borrow_and_update().clone(){return Ok(value);}features.changed().await.map_err(|_|error("AUDIO_UNAVAILABLE"))?;}
            })=>result.map_err(|_|error("AUDIO_PROBE_TIMEOUT"))?,
        }
    }
    pub async fn subscribe(&self, cancel: CancellationToken) -> Result<AudioStream> {
        let caps = self.probe().await?;
        if !caps.from_target {
            return Err(error("UNSUPPORTED_ACTION"));
        }
        self.shared.source.subscribe(cancel)
    }
    pub async fn microphone(&self, enabled: bool, context: Option<ActionContext>) -> Result<()> {
        if enabled && !self.probe().await?.to_target {
            return Err(error("UNSUPPORTED_ACTION"));
        }
        self.shared.mic.set(enabled, context.as_ref())?;
        self.shared.source.changed.notify_one();
        Ok(())
    }
    pub fn send(&self, data: Arc<[u8]>, context: &ActionContext) -> Result<()> {
        self.shared.mic.push(data, context)
    }
    pub fn stop_microphone(&self) {
        let _ = self.shared.mic.set(false, None);
        self.shared.source.changed.notify_one();
    }
    pub async fn close(&self) {
        self.cancel.cancel();
        self.shared.source.closed.cancel();
        self.stop_microphone();
        let worker = self.worker.lock().unwrap().take();
        if let Some(mut worker) = worker
            && tokio::time::timeout(Duration::from_secs(2), &mut worker)
                .await
                .is_err()
        {
            worker.abort();
            let _ = worker.await;
        }
    }
}
impl Drop for JanusAudio {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.shared.source.closed.cancel();
        self.stop_microphone();
    }
}

struct Signaling {
    ws: Socket,
    session: Option<u64>,
    handle: Option<u64>,
}
impl Signaling {
    async fn send(&mut self, mut value: Value, cancel: &CancellationToken) -> Result<()> {
        if let Some(id) = self.session {
            value["session_id"] = json!(id);
        }
        if let Some(id) = self.handle {
            value["handle_id"] = json!(id);
        }
        tokio::select! {biased;_=cancel.cancelled()=>Err(error("AUDIO_DISCONNECTED")),
        result=tokio::time::timeout(Duration::from_secs(2),self.ws.send(Message::Text(value.to_string().into())))=>result.map_err(|_|error("AUDIO_SIGNALING_FAILED"))?.map_err(|_|error("AUDIO_SIGNALING_FAILED"))}
    }
    async fn receive(&mut self, cancel: &CancellationToken) -> Result<Value> {
        loop {
            let incoming = tokio::select! {biased;_=cancel.cancelled()=>return Err(error("AUDIO_DISCONNECTED")),v=self.ws.next()=>v};
            match incoming {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_slice(text.as_bytes())
                        .map_err(|_| error("AUDIO_SIGNALING_FAILED"));
                }
                Some(Ok(Message::Ping(_))) => {
                    tokio::time::timeout(Duration::from_secs(2), self.ws.flush())
                        .await
                        .map_err(|_| error("AUDIO_SIGNALING_FAILED"))?
                        .map_err(|_| error("AUDIO_SIGNALING_FAILED"))?;
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                    return Err(error("AUDIO_DISCONNECTED"));
                }
                _ => (),
            }
        }
    }
    async fn request(&mut self, mut value: Value, cancel: &CancellationToken) -> Result<Value> {
        let transaction = uuid::Uuid::new_v4().to_string();
        value["transaction"] = json!(transaction);
        self.send(value, cancel).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let value = self.receive(cancel).await?;
                if value["transaction"] == transaction && value["janus"] != "ack" {
                    if value.get("error").is_some_and(|e| !e.is_null()) {
                        return Err(error("AUDIO_SIGNALING_FAILED"));
                    }
                    return Ok(value);
                }
            }
        })
        .await
        .map_err(|_| error("AUDIO_PROBE_TIMEOUT"))?
    }
}
struct Peer {
    rtc: Rtc,
    lan: RtcLan,
    mid: Mid,
    pt: Pt,
    ssrc: Option<Ssrc>,
    seq: u64,
    time: u32,
    decoder: OpusReceiver,
    encoder: opus::Encoder,
    candidates: usize,
    generation: u64,
}
impl Peer {
    async fn offer(
        signal: &Socket,
        jsep: &Value,
        microphone: bool,
        generation: u64,
    ) -> Result<(Self, String)> {
        let sdp = jsep["sdp"]
            .as_str()
            .filter(|s| s.len() <= 65536)
            .ok_or_else(|| error("AUDIO_NEGOTIATION_FAILED"))?;
        if jsep["type"] != "offer" {
            return Err(error("AUDIO_NEGOTIATION_FAILED"));
        }
        let (sdp, mid, candidates) = limited_offer(sdp, microphone)?;
        let lan = RtcLan::new(signal).await?;
        let mut rtc = RtcConfig::new()
            .clear_codecs()
            .enable_opus(true, false)
            .enable_h264(true)
            .set_send_buffer_audio(8)
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            .set_rtp_mode(true)
            .build(Instant::now());
        lan.add_candidates(&mut rtc)?;
        let offer =
            SdpOffer::from_sdp_string(&sdp).map_err(|_| error("AUDIO_NEGOTIATION_FAILED"))?;
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|_| error("AUDIO_NEGOTIATION_FAILED"))?;
        let pts = rtc
            .media(mid)
            .ok_or_else(|| error("AUDIO_NEGOTIATION_FAILED"))?
            .remote_pts();
        let pt = rtc
            .codec_config()
            .find(|p| pts.contains(&p.pt()) && p.spec().codec == Codec::Opus)
            .ok_or_else(|| error("AUDIO_NEGOTIATION_FAILED"))?
            .pt();
        // accept_offer already creates the sender announced in its SDP. Using
        // a second random SSRC here would send media the target never mapped.
        let ssrc = if microphone
            && rtc
                .media(mid)
                .is_some_and(|m| matches!(m.direction(), Direction::SendRecv | Direction::SendOnly))
        {
            let mut direct = rtc.direct_api();
            let stream = direct
                .stream_tx_by_mid(mid, None)
                .ok_or_else(|| error("AUDIO_NEGOTIATION_FAILED"))?;
            stream.set_unpaced(true);
            Some(stream.ssrc())
        } else {
            None
        };
        Ok((
            Self {
                rtc,
                lan,
                mid,
                pt,
                ssrc,
                seq: u64::from(rand::random::<u16>()),
                time: rand::random(),
                decoder: OpusReceiver::new()?,
                encoder: opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip)
                    .map_err(|_| error("AUDIO_UNAVAILABLE"))?,
                candidates,
                generation,
            },
            answer.to_sdp_string(),
        ))
    }
    fn event(&mut self, event: Event, shared: &Shared) -> Result<()> {
        match event {
            Event::RtpPacket(packet) => {
                if self
                    .rtc
                    .direct_api()
                    .stream_rx(&packet.header.ssrc)
                    .is_some_and(|s| s.mid() == self.mid)
                    && shared.source.users() > 0
                {
                    if let Some((data, gap)) = self
                        .decoder
                        .decode(packet.header.sequence_number, &packet.payload)
                    {
                        shared.source.publish(data, self.generation, gap);
                    }
                } else {
                    self.decoder.discontinuity();
                }
            }
            Event::Closed | Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                return Err(error("AUDIO_DISCONNECTED"));
            }
            _ => (),
        }
        Ok(())
    }
    fn microphone(&mut self, shared: &Shared) -> Result<()> {
        if !self.rtc.is_connected() || !shared.mic.authorized() {
            return Ok(());
        }
        if let Some(samples) = shared.mic.frame() {
            let payload = self
                .encoder
                .encode_vec(&samples, 4000)
                .map_err(|_| error("AUDIO_UNAVAILABLE"))?;
            if let Some(ssrc) = self.ssrc
                && let Some(stream) = self.rtc.direct_api().stream_tx(&ssrc)
            {
                stream.write_rtp(RtpWrite::new(
                    self.pt,
                    self.seq.into(),
                    self.time,
                    Instant::now(),
                    payload,
                ));
                self.seq += 1;
                self.time = self.time.wrapping_add(960);
            }
        }
        Ok(())
    }
}
fn limited_offer(sdp: &str, microphone: bool) -> Result<(String, Mid, usize)> {
    // Also apply the SDP default/inherited direction. Omitting a=sendrecv must
    // not accidentally advertise a microphone or camera send direction.
    let sdp = local_ice_description(sdp);
    let candidates = sdp
        .lines()
        .filter(|line| line.starts_with("a=candidate:"))
        .count();
    if candidates > 256 {
        return Err(error("RESOURCE_LIMIT"));
    }
    let direction = |line: &str| {
        matches!(
            line,
            "a=sendrecv" | "a=sendonly" | "a=recvonly" | "a=inactive"
        )
    };
    let mut blocks: Vec<Vec<&str>> = vec![vec![]];
    for line in sdp.lines() {
        if line.starts_with("m=") {
            blocks.push(vec![]);
        }
        blocks.last_mut().unwrap().push(line);
    }
    if blocks.len() > 5 {
        return Err(error("RESOURCE_LIMIT"));
    }
    let inherited = blocks[0]
        .iter()
        .copied()
        .find(|line| direction(line))
        .unwrap_or("a=sendrecv");
    let mut result = blocks[0]
        .iter()
        .copied()
        .filter(|line| !direction(line))
        .collect::<Vec<_>>();
    let mut mid = None;
    for block in &blocks[1..] {
        let audio = block
            .first()
            .is_some_and(|line| line.starts_with("m=audio "));
        if audio {
            let id = block
                .iter()
                .find_map(|line| line.strip_prefix("a=mid:"))
                .ok_or_else(|| error("AUDIO_NEGOTIATION_FAILED"))?;
            if mid.is_some()
                || id.is_empty()
                || id.len() > 16
                || !id.bytes().all(|b| b.is_ascii_alphanumeric())
            {
                return Err(error("AUDIO_NEGOTIATION_FAILED"));
            }
            mid = Some(Mid::from(id));
        }
        let offered = block
            .iter()
            .copied()
            .find(|line| direction(line))
            .unwrap_or(inherited);
        let selected = if !audio {
            "a=inactive"
        } else if !microphone {
            match offered {
                "a=sendrecv" => "a=sendonly",
                "a=recvonly" => "a=inactive",
                _ => offered,
            }
        } else {
            offered
        };
        result.extend(block.iter().copied().filter(|line| !direction(line)));
        result.push(selected);
    }
    Ok((
        result.join("\r\n") + "\r\n",
        mid.ok_or_else(|| error("AUDIO_NEGOTIATION_FAILED"))?,
        candidates,
    ))
}
fn update_features(shared: &Shared, value: &Value) -> bool {
    let result = &value["plugindata"]["data"]["result"];
    if result["status"] != "features" {
        return false;
    }
    let audio = result["features"]["audio"] == true;
    let mic = result["features"]["mic"] == true;
    let caps = AudioCapabilities {
        from_target: audio,
        to_target: mic,
        formats: if audio {
            vec!["pcm_s16le/48000/2".into()]
        } else {
            vec![]
        },
        reason: Some(
            if audio {
                ""
            } else {
                "PiKVM reports no audio source"
            }
            .into(),
        ),
    };
    shared.features.send_replace(Some(caps.clone()));
    (shared.callback)(caps);
    true
}
async fn session(
    transport: &Arc<LanTransport>,
    shared: &Shared,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "sec-websocket-protocol",
        reqwest::header::HeaderValue::from_static("janus-protocol"),
    );
    let ws = transport
        .websocket_headers("/janus/ws", headers, cancel)
        .await
        .map_err(|_| error("AUDIO_UNAVAILABLE"))?;
    let mut signal = Signaling {
        ws,
        session: None,
        handle: None,
    };
    signal.session = Some(
        signal.request(json!({"janus":"create"}), cancel).await?["data"]["id"]
            .as_u64()
            .ok_or_else(|| error("AUDIO_SIGNALING_FAILED"))?,
    );
    signal.handle = Some(
        signal
            .request(
                json!({"janus":"attach","plugin":"janus.plugin.ustreamer"}),
                cancel,
            )
            .await?["data"]["id"]
            .as_u64()
            .ok_or_else(|| error("AUDIO_SIGNALING_FAILED"))?,
    );
    signal.send(json!({"janus":"message","transaction":uuid::Uuid::new_v4().to_string(),"body":{"request":"features"}}),cancel).await?;
    tokio::time::timeout(Duration::from_millis(5500), async {
        loop {
            let value = signal.receive(cancel).await?;
            if update_features(shared, &value) {
                return Ok::<(), GatewayError>(());
            }
        }
    })
    .await
    .map_err(|_| error("AUDIO_PROBE_TIMEOUT"))??;
    let mut previous = (false, false);
    let mut peer: Option<Peer> = None;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    heartbeat.tick().await;
    let mut clock = tokio::time::interval(Duration::from_millis(20));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let mic = shared.mic.authorized();
        let wanted = (shared.source.users() > 0 || mic, mic);
        if wanted != previous {
            peer = None;
            shared.mic.clear();
            shared.generation.fetch_add(1, Ordering::AcqRel);
            if signal.handle.is_some() {
                signal.request(json!({"janus":"detach"}), cancel).await?;
                signal.handle = None;
            }
            previous = wanted;
            if wanted.0 {
                signal.handle = Some(
                    signal
                        .request(
                            json!({"janus":"attach","plugin":"janus.plugin.ustreamer"}),
                            cancel,
                        )
                        .await?["data"]["id"]
                        .as_u64()
                        .ok_or_else(|| error("AUDIO_SIGNALING_FAILED"))?,
                );
                // Coalesce changes which happened while an attach was pending.
                let latest_mic = shared.mic.authorized();
                if (shared.source.users() > 0 || latest_mic, latest_mic) != wanted {
                    continue;
                }
                signal.send(json!({"janus":"message","transaction":uuid::Uuid::new_v4().to_string(),"body":{"request":"watch","params":{"orientation":0,"audio":true,"mic":mic,"camera":false}}}),cancel).await?;
            }
        }
        let timeout = if let Some(p) = peer.as_mut() {
            loop {
                match p
                    .rtc
                    .poll_output()
                    .map_err(|_| error("AUDIO_NEGOTIATION_FAILED"))?
                {
                    Output::Timeout(t) => break t,
                    Output::Event(e) => p.event(e, shared)?,
                    Output::Transmit(packet) => {
                        let media = packet.contents.first().is_some_and(|b| b & 0xc0 == 0x80)
                            && packet.contents.get(1).is_some_and(|b| b & 0x7f == *p.pt);
                        if !media || shared.mic.authorized() {
                            p.lan.transmit(&packet)?;
                        }
                    }
                }
            }
        } else {
            Instant::now() + Duration::from_secs(60)
        };
        tokio::select! {
            biased;
            _=cancel.cancelled()=>{let _=signal.send(json!({"janus":"destroy","transaction":uuid::Uuid::new_v4().to_string()}),&CancellationToken::new()).await;return Ok(());},
            _=shared.source.changed.notified()=>(),
            _=clock.tick(), if mic && peer.is_some()=>{if let Some(p)=peer.as_mut(){p.microphone(shared)?;}},
            _=heartbeat.tick()=>signal.send(json!({"janus":"keepalive","transaction":uuid::Uuid::new_v4().to_string()}),cancel).await?,
            result=async{if let Some(p)=peer.as_mut(){p.lan.receive(&mut p.rtc).await}else{std::future::pending().await}}=>result?,
            _=tokio::time::sleep_until(timeout.into())=>{if let Some(p)=peer.as_mut(){p.rtc.handle_input(Input::Timeout(Instant::now())).map_err(|_|error("AUDIO_NEGOTIATION_FAILED"))?;}},
            received=signal.receive(cancel)=>{
                let value=received?;update_features(shared,&value);
                if value["sender"].as_u64()==signal.handle && signal.handle.is_some() {
                    if let Some(jsep)=value.get("jsep").filter(|_|wanted.0) {
                        let (new,answer)=Peer::offer(&signal.ws,jsep,mic,shared.generation.load(Ordering::Acquire)).await?;
                        peer=Some(new);
                        signal.send(json!({"janus":"message","transaction":uuid::Uuid::new_v4().to_string(),"body":{"request":"start"},"jsep":{"type":"answer","sdp":answer}}),cancel).await?;
                    }
                    if value["janus"]=="trickle" && value["candidate"]["completed"]!=true
                        && let (Some(p),Some(candidate))=(peer.as_mut(),value["candidate"]["candidate"].as_str()) {
                            check_ice_candidate(candidate)?;p.candidates+=1;if p.candidates>256{return Err(error("RESOURCE_LIMIT"));}
                            p.rtc.add_remote_candidate(Candidate::from_sdp_string(candidate).map_err(|_|error("AUDIO_NEGOTIATION_FAILED"))?);
                    }
                    if matches!(value["janus"].as_str(),Some("hangup"|"detached")) && wanted.0{return Err(error("AUDIO_DISCONNECTED"));}
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offers_limit_tracks_candidates_and_default_or_inherited_send_directions() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:1\r\n";
        let (listen, mid, _) = limited_offer(sdp, false).unwrap();
        assert_eq!(&*mid, "0");
        assert!(listen.contains("a=sendonly\r\nm=video"));
        assert!(listen.ends_with("a=inactive\r\n"));
        assert!(
            limited_offer(sdp, true)
                .unwrap()
                .0
                .contains("a=sendrecv\r\nm=video")
        );
        let inherited = sdp.replacen("v=0", "v=0\r\na=recvonly", 1);
        assert_eq!(
            limited_offer(&inherited, false)
                .unwrap()
                .0
                .matches("a=inactive")
                .count(),
            2
        );
        let candidates =
            sdp.to_owned() + &"a=candidate:1 1 UDP 1 127.0.0.1 9999 typ host\r\n".repeat(257);
        assert_eq!(
            limited_offer(&candidates, false).unwrap_err().code,
            "RESOURCE_LIMIT"
        );
        let duplicate = sdp.to_owned() + "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:2\r\n";
        assert_eq!(
            limited_offer(&duplicate, false).unwrap_err().code,
            "AUDIO_NEGOTIATION_FAILED"
        );
    }
}
