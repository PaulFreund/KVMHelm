//! Shared bounded PCM sources. A slow subscriber drops old audio and receives
//! a discontinuity flag; it never slows input or other listeners.
use crate::{
    GatewayError, Result,
    drivers::{ActionContext, AudioStream},
    shared::monotonic_ms,
};
use futures_util::stream;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct AudioChunk {
    pub device_id: String,
    pub format: &'static str,
    pub sample_rate: u32,
    pub channels: u8,
    pub sequence: u64,
    pub timestamp_ms: f64,
    pub connection_generation: u64,
    pub discontinuity: bool,
    pub data: Arc<[u8]>,
}
pub struct Source {
    id: String,
    sender: broadcast::Sender<Result<AudioChunk>>,
    users: AtomicUsize,
    fault_generation: AtomicU64,
    sequence: AtomicU64,
    pub changed: Notify,
    pub closed: CancellationToken,
}
impl Source {
    pub fn new(id: String, capacity: usize, first_sequence: u64) -> Arc<Self> {
        Arc::new(Self {
            id,
            sender: broadcast::channel(capacity).0,
            users: AtomicUsize::new(0),
            fault_generation: AtomicU64::new(0),
            sequence: AtomicU64::new(first_sequence),
            changed: Notify::new(),
            closed: CancellationToken::new(),
        })
    }
    pub fn users(&self) -> usize {
        self.users.load(Ordering::Acquire)
    }
    pub fn publish(&self, data: Arc<[u8]>, generation: u64, discontinuity: bool) {
        if data.is_empty()
            || data.len() > 23040
            || !data.len().is_multiple_of(4)
            || self.closed.is_cancelled()
        {
            return;
        }
        let chunk = AudioChunk {
            device_id: self.id.clone(),
            format: "pcm_s16le",
            sample_rate: 48000,
            channels: 2,
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
            timestamp_ms: monotonic_ms(),
            connection_generation: generation,
            discontinuity,
            data,
        };
        let _ = self.sender.send(Ok(chunk));
    }
    pub fn fault(&self, code: &str) {
        self.fault_generation.fetch_add(1, Ordering::AcqRel);
        let _ = self.sender.send(Err(GatewayError::new(code)));
    }
    pub fn subscribe(self: &Arc<Self>, caller: CancellationToken) -> Result<AudioStream> {
        if self.closed.is_cancelled() {
            return Err(GatewayError::new("AUDIO_DISCONNECTED"));
        }
        let receiver = self.sender.subscribe();
        self.users.fetch_add(1, Ordering::AcqRel);
        self.changed.notify_one();
        let guard = Consumer(self.clone());
        let generation = self.fault_generation.load(Ordering::Acquire);
        Ok(Box::pin(stream::try_unfold(
            (receiver, guard, caller, false, generation),
            |(mut receiver, guard, caller, mut gap, generation)| async move {
                loop {
                    if guard.0.fault_generation.load(Ordering::Acquire) != generation {
                        return Err(GatewayError::new("AUDIO_DISCONNECTED"));
                    }
                    let received = tokio::select! {biased;
                        _=caller.cancelled()=>return Ok(None),
                        _=guard.0.closed.cancelled()=>return Err(GatewayError::new("AUDIO_DISCONNECTED")),
                        result=receiver.recv()=>result,
                    };
                    match received {
                        Ok(Ok(mut chunk)) => {
                            if guard.0.fault_generation.load(Ordering::Acquire) != generation {
                                continue;
                            }
                            chunk.discontinuity |= gap;
                            return Ok(Some((chunk, (receiver, guard, caller, false, generation))));
                        }
                        Ok(Err(error)) => return Err(error),
                        Err(broadcast::error::RecvError::Lagged(_)) => gap = true,
                        Err(_) => return Err(GatewayError::new("AUDIO_DISCONNECTED")),
                    }
                }
            },
        )))
    }
}

struct Consumer(Arc<Source>);
impl Drop for Consumer {
    fn drop(&mut self) {
        self.0.users.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_one();
    }
}

pub(crate) struct OpusReceiver {
    decoder: opus::Decoder,
    previous: Option<u16>,
    gap: bool,
    samples: Vec<i16>,
}
impl OpusReceiver {
    pub fn new() -> Result<Self> {
        Ok(Self {
            decoder: opus::Decoder::new(48000, opus::Channels::Stereo)
                .map_err(|_| GatewayError::new("AUDIO_UNAVAILABLE"))?,
            previous: None,
            gap: true,
            samples: vec![0; 11520],
        })
    }
    pub fn decode(&mut self, sequence: u16, payload: &[u8]) -> Option<(Arc<[u8]>, bool)> {
        if let Some(previous) = self.previous {
            let delta = sequence.wrapping_sub(previous);
            if delta == 0 || delta > 32768 {
                return None;
            }
            if delta != 1 {
                self.gap = true;
            }
        }
        self.previous = Some(sequence);
        match self.decoder.decode(payload, &mut self.samples, false) {
            Ok(frames) if frames > 0 && frames <= 5760 => {
                let bytes = self.samples[..frames * 2]
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect::<Vec<_>>();
                let gap = std::mem::replace(&mut self.gap, false);
                Some((bytes.into(), gap))
            }
            _ => {
                self.gap = true;
                None
            }
        }
    }
    pub fn discontinuity(&mut self) {
        self.gap = true;
    }
}
struct MicBlock {
    data: Arc<[u8]>,
    offset: usize,
    context: ActionContext,
}
pub(crate) struct Microphone {
    pub enabled: AtomicBool,
    queue: Mutex<(VecDeque<MicBlock>, usize)>,
    activation: Mutex<Option<ActionContext>>,
}
impl Default for Microphone {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            queue: Mutex::new((VecDeque::new(), 0)),
            activation: Mutex::new(None),
        }
    }
}
impl Microphone {
    pub fn set(&self, enabled: bool, context: Option<&ActionContext>) -> Result<()> {
        if enabled {
            context
                .ok_or_else(|| GatewayError::new("FORBIDDEN"))?
                .check()?;
        }
        let mut q = self.queue.lock().unwrap();
        q.0.clear();
        q.1 = 0;
        *self.activation.lock().unwrap() = context.cloned().filter(|_| enabled);
        self.enabled.store(enabled, Ordering::Release);
        Ok(())
    }
    pub fn authorized(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
            && self
                .activation
                .lock()
                .unwrap()
                .clone()
                .is_some_and(|c| c.check().is_ok())
    }
    pub fn push(&self, data: Arc<[u8]>, context: &ActionContext) -> Result<()> {
        context.check()?;
        if data.len() > 16384 {
            return Err(GatewayError::new("RESOURCE_LIMIT"));
        }
        let mut q = self.queue.lock().unwrap();
        if !self.enabled.load(Ordering::Acquire) {
            return Err(GatewayError::new("FORBIDDEN"));
        }
        if q.1 + data.len() > 48000 || q.0.len() >= 64 {
            q.0.clear();
            q.1 = 0;
        }
        if !data.is_empty() {
            q.1 += data.len();
            q.0.push_back(MicBlock {
                data,
                offset: 0,
                context: context.clone(),
            });
        }
        Ok(())
    }
    pub fn frame(&self) -> Option<Vec<i16>> {
        let mut q = self.queue.lock().unwrap();
        if !self.enabled.load(Ordering::Acquire) || q.1 < 1920 {
            return None;
        }
        // Validate every accepted block again at transmission, including blocks
        // which contribute only part of this 20 ms frame.
        if q.0.iter().any(|b| b.context.check().is_err()) {
            q.0.clear();
            q.1 = 0;
            return None;
        }
        let mut data = Vec::with_capacity(1920);
        while data.len() < 1920 {
            let block = q.0.front_mut().unwrap();
            let count = (1920 - data.len()).min(block.data.len() - block.offset);
            data.extend_from_slice(&block.data[block.offset..block.offset + count]);
            block.offset += count;
            if block.offset == block.data.len() {
                q.0.pop_front();
            }
            q.1 -= count;
        }
        Some(
            data.as_chunks::<2>()
                .0
                .iter()
                .map(|s| i16::from_le_bytes([s[0], s[1]]))
                .collect(),
        )
    }
    pub fn clear(&self) {
        let mut q = self.queue.lock().unwrap();
        q.0.clear();
        q.1 = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    fn context() -> ActionContext {
        ActionContext {
            cancellation: CancellationToken::new(),
            width: 0,
            height: 0,
            assert_control: Arc::new(|| Ok(())),
        }
    }
    #[tokio::test]
    async fn listeners_share_bytes_and_lag_drops_old_audio_without_blocking() {
        let source = Source::new("fixture".into(), 2, 0);
        let mut fast = source.subscribe(CancellationToken::new()).unwrap();
        let mut slow = source.subscribe(CancellationToken::new()).unwrap();
        assert_eq!(source.users(), 2);
        let data: Arc<[u8]> = vec![1, 0, 2, 0].into();
        source.publish(data.clone(), 7, true);
        let a = fast.next().await.unwrap().unwrap();
        let b = slow.next().await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&a.data, &b.data));
        assert_eq!(a.format, "pcm_s16le");
        assert_eq!(
            (
                a.sample_rate,
                a.channels,
                a.sequence,
                a.connection_generation
            ),
            (48000, 2, 0, 7)
        );
        for _ in 0..5 {
            source.publish(data.clone(), 7, false);
            assert!(!fast.next().await.unwrap().unwrap().discontinuity);
        }
        let lagged = slow.next().await.unwrap().unwrap();
        assert!(lagged.discontinuity);
        assert_eq!(lagged.sequence, 4);
        drop(fast);
        assert_eq!(source.users(), 1);
        drop(slow);
        assert_eq!(source.users(), 0);
    }
    #[tokio::test]
    async fn source_fault_discards_queued_old_generation_but_new_listeners_can_recover() {
        let source = Source::new("fixture".into(), 2, 0);
        let mut old = source.subscribe(CancellationToken::new()).unwrap();
        source.publish(vec![0; 4].into(), 1, false);
        source.fault("AUDIO_DISCONNECTED");
        source.publish(vec![1; 4].into(), 2, true);
        assert_eq!(
            old.next().await.unwrap().unwrap_err().code,
            "AUDIO_DISCONNECTED"
        );
        drop(old);
        let cancel = CancellationToken::new();
        let mut fresh = source.subscribe(cancel.clone()).unwrap();
        source.publish(vec![1; 4].into(), 2, true);
        assert_eq!(
            fresh.next().await.unwrap().unwrap().connection_generation,
            2
        );
        cancel.cancel();
        assert!(fresh.next().await.is_none());
        assert_eq!(source.users(), 0);
        source.closed.cancel();
        assert!(source.subscribe(CancellationToken::new()).is_err());
    }
    #[test]
    fn opus_is_decoded_as_stereo_pcm_and_loss_wrap_duplicates_and_corruption_are_bounded() {
        let mut encoder =
            opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let tone = (0..960)
            .map(|i| (12000.0 * (i as f64 * std::f64::consts::TAU * 1000.0 / 48000.0).sin()) as i16)
            .collect::<Vec<_>>();
        let packet = encoder.encode_vec(&tone, 4000).unwrap();
        let mut receiver = OpusReceiver::new().unwrap();
        let (pcm, gap) = receiver.decode(65535, &packet).unwrap();
        assert!(gap);
        assert_eq!(pcm.len(), 3840);
        assert!(pcm.iter().any(|v| *v != 0));
        assert!(pcm.as_chunks::<4>().0.iter().all(|v| v[0..2] == v[2..4]));
        assert!(!receiver.decode(0, &packet).unwrap().1);
        assert!(receiver.decode(0, &packet).is_none());
        assert!(receiver.decode(65535, &packet).is_none());
        assert!(receiver.decode(2, &packet).unwrap().1);
        assert!(receiver.decode(3, &[255]).is_none());
        assert!(receiver.decode(4, &packet).unwrap().1);
        receiver.discontinuity();
        assert!(receiver.decode(5, &packet).unwrap().1);
    }
    #[test]
    fn microphone_bounds_bytes_blocks_and_checks_revocation_at_frame_consumption() {
        let mic = Microphone::default();
        let ctx = context();
        assert!(mic.push(vec![0; 1920].into(), &ctx).is_err());
        assert!(mic.set(true, None).is_err());
        mic.set(true, Some(&ctx)).unwrap();
        assert!(mic.authorized());
        assert_eq!(
            mic.push(vec![0; 16385].into(), &ctx).unwrap_err().code,
            "RESOURCE_LIMIT"
        );
        for _ in 0..4 {
            mic.push(vec![1; 16000].into(), &ctx).unwrap();
        }
        assert_eq!(mic.queue.lock().unwrap().1, 16000);
        mic.clear();
        for _ in 0..65 {
            mic.push(vec![1].into(), &ctx).unwrap();
        }
        assert_eq!(mic.queue.lock().unwrap().0.len(), 1);
        mic.clear();
        mic.push(vec![1; 1000].into(), &ctx).unwrap();
        mic.push(vec![2; 920].into(), &ctx).unwrap();
        assert_eq!(mic.frame().unwrap().len(), 960);
        assert!(mic.frame().is_none());
        mic.push(vec![1; 1920].into(), &ctx).unwrap();
        ctx.cancellation.cancel();
        assert!(!mic.authorized());
        assert!(mic.frame().is_none());
        assert_eq!(mic.queue.lock().unwrap().1, 0);
        assert!(mic.set(true, Some(&ctx)).is_err());
        mic.set(false, None).unwrap();
        assert!(!mic.authorized());
    }
}
