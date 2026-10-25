//! Bounded RFC 6184 packetization-mode 1. Incomplete access units never reach
//! the decoder; cached SPS/PPS precede IDRs after loss and decoder backpressure.
use super::frames;
use crate::{GatewayError, Result, contracts::Signal, media::DriverFrame};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{mpsc, watch},
};
use tokio_util::sync::CancellationToken;

const PREFIX: &[u8] = &[0, 0, 0, 1];
const AUD: &[u8] = &[0, 0, 0, 1, 9, 0xf0];
pub struct AccessUnit {
    pub data: Vec<u8>,
    pub keyframe: bool,
}
#[derive(Default)]
pub struct Assembly {
    pub unit: Option<AccessUnit>,
    pub lost: bool,
}
#[derive(Default)]
pub struct H264Assembler {
    sequence: Option<u16>,
    timestamp: Option<u32>,
    size: usize,
    units: Vec<Vec<u8>>,
    fragment: Option<Vec<u8>>,
    damaged: bool,
    parameters: Vec<(u8, Vec<u8>)>,
}
impl H264Assembler {
    fn clear(&mut self) {
        self.units.clear();
        self.fragment = None;
        self.size = 0;
    }
    pub fn push(
        &mut self,
        sequence: u16,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
    ) -> Assembly {
        let mut result = Assembly::default();
        if payload.is_empty() {
            return result;
        }
        let gap = if let Some(previous) = self.sequence {
            let delta = sequence.wrapping_sub(previous);
            if delta == 0 || delta > 32768 {
                return result;
            }
            delta != 1
        } else {
            false
        };
        self.sequence = Some(sequence);
        if gap {
            self.clear();
            self.damaged = true;
            result.lost = true;
        }
        if self.timestamp != Some(timestamp) {
            // A new timestamp without the preceding marker is incomplete too.
            result.lost |= !self.units.is_empty() || self.fragment.is_some();
            self.clear();
            self.timestamp = Some(timestamp);
            self.damaged = gap;
        }
        self.size = self.size.saturating_add(payload.len());
        if self.size > frames::FRAME_BYTES || self.units.len() >= 4096 {
            self.clear();
            self.damaged = true;
            result.lost = true;
        } else if !self.damaged {
            match payload[0] & 31 {
                1..=23 => {
                    if self.fragment.is_some() {
                        self.damaged = true;
                    }
                    self.units.push(payload.to_vec());
                }
                24 => {
                    let mut offset = 1;
                    while offset < payload.len() && self.units.len() < 4096 {
                        let Some(length) = payload.get(offset..offset + 2) else {
                            self.damaged = true;
                            break;
                        };
                        let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
                        offset += 2;
                        let Some(unit) =
                            payload.get(offset..offset + length).filter(|_| length > 0)
                        else {
                            self.damaged = true;
                            break;
                        };
                        if !(1..=23).contains(&(unit[0] & 31)) {
                            self.damaged = true;
                            break;
                        }
                        self.units.push(unit.to_vec());
                        offset += length;
                    }
                    self.damaged |= offset != payload.len() || self.fragment.is_some();
                }
                28 if payload.len() >= 3 => {
                    let start = payload[1] & 128 != 0;
                    let end = payload[1] & 64 != 0;
                    let nal = (payload[0] & 224) | (payload[1] & 31);
                    if payload[1] & 32 != 0 || !(1..=23).contains(&(nal & 31)) || start && end {
                        self.damaged = true;
                    } else if start {
                        if self.fragment.is_some() {
                            self.damaged = true;
                        }
                        let mut fragment = vec![nal];
                        fragment.extend_from_slice(&payload[2..]);
                        self.fragment = Some(fragment);
                    } else if let Some(fragment) = self.fragment.as_mut().filter(|f| f[0] == nal) {
                        fragment.extend_from_slice(&payload[2..]);
                        if end {
                            self.units.push(self.fragment.take().unwrap());
                        }
                    } else {
                        self.damaged = true;
                    }
                }
                _ => self.damaged = true,
            }
        }
        if marker {
            if !self.damaged && self.fragment.is_none() && !self.units.is_empty() {
                for unit in &self.units {
                    let kind = unit[0] & 31;
                    if matches!(kind, 7 | 8) && unit.len() <= 65536 {
                        if let Some((_, previous)) =
                            self.parameters.iter_mut().find(|(k, _)| *k == kind)
                        {
                            *previous = unit.clone();
                        } else {
                            self.parameters.push((kind, unit.clone()));
                        }
                    }
                }
                let keyframe = self.units.iter().any(|n| n[0] & 31 == 5);
                let mut data = Vec::new();
                if keyframe {
                    for (_, parameter) in &self.parameters {
                        data.extend_from_slice(PREFIX);
                        data.extend_from_slice(parameter);
                    }
                }
                for unit in &self.units {
                    if keyframe && matches!(unit[0] & 31, 7 | 8) {
                        continue;
                    }
                    data.extend_from_slice(PREFIX);
                    data.extend_from_slice(unit);
                }
                if data.len() <= frames::FRAME_BYTES {
                    result.unit = Some(AccessUnit { data, keyframe });
                } else {
                    result.lost = true;
                }
            } else {
                result.lost = true;
            }
            self.clear();
        }
        result
    }
}

pub(crate) struct Decoder {
    sender: mpsc::Sender<Vec<u8>>,
    pub frames: watch::Receiver<Option<DriverFrame>>,
    pub fault: CancellationToken,
    cancel: CancellationToken,
    worker: tokio::task::JoinHandle<()>,
}
pub fn ffmpeg_path() -> PathBuf {
    if let Some(path) = std::env::var_os("KVMHELM_FFMPEG").filter(|p| !p.is_empty()) {
        return path.into();
    }
    // A release may put its independently licensed helper beside the binary.
    if let Ok(exe) = std::env::current_exe() {
        let path = exe.with_file_name(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from("ffmpeg")
}
impl Decoder {
    pub async fn start() -> Result<Self> {
        Self::start_at(ffmpeg_path()).await
    }
    async fn start_at(path: PathBuf) -> Result<Self> {
        let mut command = Command::new(path);
        command
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-max_alloc",
                "67108864",
                "-protocol_whitelist",
                "pipe",
                "-probesize",
                "32",
                "-analyzeduration",
                "0",
                "-flags",
                "low_delay",
                "-threads",
                "2",
                "-thread_type",
                "slice",
                "-f",
                "h264",
                "-max_pixels",
                "16777216",
                "-i",
                "pipe:0",
                "-an",
                "-c:v",
                "mjpeg",
                "-threads",
                "2",
                "-q:v",
                "3",
                "-fps_mode",
                "passthrough",
                "-f",
                "image2pipe",
                "-flush_packets",
                "1",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW; helpers are never interactive.
        let mut child = command.spawn().map_err(|_|GatewayError::message("DEPENDENCY_MISSING",
            "JetKVM requires FFmpeg with H.264 decoding and MJPEG encoding. Install ffmpeg or set KVMHELM_FFMPEG."))?;
        let mut input = child.stdin.take().unwrap();
        let mut output = child.stdout.take().unwrap();
        let (sender, mut incoming) = mpsc::channel::<Vec<u8>>(1);
        let (frames, receiver) = watch::channel(None);
        let cancel = CancellationToken::new();
        let fault = CancellationToken::new();
        let lifetime = cancel.clone();
        let failed = fault.clone();
        let worker = tokio::spawn(async move {
            let writer = async {
                while let Some(data) = incoming.recv().await {
                    input.write_all(&data).await.map_err(|_| ())?;
                    input.write_all(AUD).await.map_err(|_| ())?;
                    input.flush().await.map_err(|_| ())?;
                }
                Ok::<(), ()>(())
            };
            let reader = async {
                let mut parser = frames::Mjpeg::default();
                let mut buffer = [0; 16384];
                loop {
                    let count = output.read(&mut buffer).await.map_err(|_| ())?;
                    if count == 0 {
                        return Err::<(), ()>(());
                    }
                    parser.push(&buffer[..count]).map_err(|_| ())?;
                    // Coalesce completed frames before metadata work; watch is latest only.
                    let mut latest = None;
                    while let Some(data) = parser.next_frame().map_err(|_| ())? {
                        latest = Some(data);
                    }
                    if let Some(data) = latest {
                        let frame = frames::inspect(data, Signal::Present)
                            .await
                            .map_err(|_| ())?;
                        frames.send_replace(Some(frame));
                    }
                }
            };
            tokio::select! { _=lifetime.cancelled()=>(), _=child.wait()=>failed.cancel(), _=writer=>failed.cancel(), _=reader=>failed.cancel() }
            // Always reap our own helper. No process-name or process-tree kills.
            let _ = child.start_kill();
            let _ = child.wait().await;
        });
        Ok(Self {
            sender,
            frames: receiver,
            fault,
            cancel,
            worker,
        })
    }
    pub fn write(&self, data: Vec<u8>) -> bool {
        data.len() <= frames::FRAME_BYTES && self.sender.try_send(data).is_ok()
    }
    pub async fn close(mut self) {
        self.cancel.cancel();
        if tokio::time::timeout(Duration::from_secs(2), &mut self.worker)
            .await
            .is_err()
        {
            self.worker.abort();
            let _ = (&mut self.worker).await;
        }
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn missing_decoder_is_an_explicit_dependency_error_without_a_worker() {
        let dir = tempfile::tempdir().unwrap();
        let result = Decoder::start_at(dir.path().join("nonexistent-ffmpeg")).await;
        assert_eq!(result.err().unwrap().code, "DEPENDENCY_MISSING");
    }
    #[test]
    fn incomplete_oversized_and_malformed_access_units_require_recovery() {
        let mut a = H264Assembler::default();
        assert!(a.push(1, 1, false, &[0x7c, 0x85, 1]).unit.is_none());
        assert!(a.push(2, 2, true, &[0x61, 2]).lost); // Missing marker, new timestamp.
        assert!(a.push(3, 3, true, &[0x7c, 0x45, 2]).lost); // Orphan fragment.
        assert!(a.push(4, 4, true, &[24, 0, 8, 0x65]).lost);
        assert!(a.push(5, 5, true, &[0x7c, 0xc5, 2]).lost); // Start+end forbidden.
        let oversized = vec![0x61; frames::FRAME_BYTES + 1];
        let result = a.push(6, 6, true, &oversized);
        assert!(result.lost && result.unit.is_none());
        let result = a.push(7, 7, true, &[0x65, 1]);
        assert!(!result.lost && result.unit.unwrap().keyframe);
    }
    #[test]
    fn ordered_parameter_cache_survives_loss_and_sequence_wrap() {
        let mut a = H264Assembler::default();
        a.push(65534, 1, true, &[0x67, 1]);
        a.push(65535, 2, true, &[0x68, 2]);
        let result = a.push(0, 3, true, &[0x65, 3]);
        assert!(!result.lost);
        assert_eq!(
            result.unit.unwrap().data,
            vec![
                0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3
            ]
        );
        assert!(a.push(0, 3, true, &[0x65, 3]).unit.is_none());
        assert!(a.push(2, 4, false, &[0x7c, 0x85, 3]).lost);
        assert!(a.push(3, 4, true, &[0x7c, 0x45, 4]).unit.is_none());
        let unit = a.push(4, 5, true, &[0x65, 5]).unit.unwrap();
        assert!(unit.keyframe && unit.data.starts_with(&[0, 0, 0, 1, 0x67, 1]));
    }
}
