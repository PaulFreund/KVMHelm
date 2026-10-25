use super::{ActionContext, Delivery, DriverEvent, KvmDriver};
use crate::{
    GatewayError, Result,
    auth::now_ms,
    contracts::{Action, Capabilities, Signal},
    keyboard::validate_hid,
    media::{DriverFrame, Mime},
    shared::monotonic_ms,
};
use async_trait::async_trait;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

pub struct Simulator {
    connected: AtomicBool,
    count: AtomicU64,
    marker: String,
    layout: crate::contracts::Layout,
    events: broadcast::Sender<DriverEvent>,
}
impl Simulator {
    pub fn new(layout: crate::contracts::Layout) -> Self {
        let (events, _) = broadcast::channel(16);
        Self {
            connected: AtomicBool::new(false),
            count: AtomicU64::new(0),
            marker: uuid::Uuid::new_v4().simple().to_string()[..8].to_ascii_uppercase(),
            layout,
            events,
        }
    }
    pub fn action_count(&self) -> u64 {
        self.count.load(Ordering::Acquire)
    }
    pub fn marker(&self) -> &str {
        &self.marker
    }
}
#[async_trait]
impl KvmDriver for Simulator {
    fn events(&self) -> broadcast::Receiver<DriverEvent> {
        self.events.subscribe()
    }
    async fn connect(&self, cancellation: CancellationToken) -> Result<()> {
        if cancellation.is_cancelled() {
            return Err(GatewayError::new("DEVICE_OFFLINE"));
        }
        self.connected.store(true, Ordering::Release);
        Ok(())
    }
    async fn disconnect(&self) -> Result<()> {
        self.connected.store(false, Ordering::Release);
        Ok(())
    }
    async fn capabilities(&self) -> Result<Capabilities> {
        serde_json::from_value(serde_json::json!({
            "video":{"snapshot":true,"stream":false,"formats":["image/png"],"signal":"present"},
            "mouse":{"absolute":true,"relative":true,"buttons":["left","right","middle"],"horizontal_scroll":true},
            "keyboard":{"hid":true,"layouts":["us","de"],"text_method":"simulated","characters":"US / DE"},
            "audio":{"from_target":false,"to_target":false,"formats":[],"reason":"Simulator has no audio"},
            "timebase":{"source_timestamp":true,"uncertainty_ms":0},"extensions":{"simulated":true}
        })).map_err(|_|GatewayError::new("EXECUTION_UNKNOWN"))
    }
    async fn snapshot(&self, cancellation: CancellationToken) -> Result<DriverFrame> {
        if !self.connected.load(Ordering::Acquire) || cancellation.is_cancelled() {
            return Err(GatewayError::new("DEVICE_OFFLINE"));
        }
        let marker = self.marker.clone();
        let count = self.action_count();
        let data = tokio::task::spawn_blocking(move || render(&marker, count))
            .await
            .map_err(|_| GatewayError::new("FRAME_TIMEOUT"))??;
        if cancellation.is_cancelled() {
            return Err(GatewayError::new("FRAME_TIMEOUT"));
        }
        Ok(DriverFrame {
            data: Arc::from(data),
            width: 1280,
            height: 720,
            mime_type: Mime::Png,
            source_timestamp: Some(monotonic_ms()),
            signal: Signal::Present,
        })
    }
    fn validate(&self, actions: &[Action], width: u32, height: u32) -> Result<()> {
        validate_hid(actions, width, height, true, true, self.layout)
    }
    async fn execute(&self, _action: &Action, context: &ActionContext) -> Result<Delivery> {
        context.check()?;
        if !self.connected.load(Ordering::Acquire) {
            return Err(GatewayError::new("DEVICE_OFFLINE"));
        }
        self.count.fetch_add(1, Ordering::AcqRel);
        Ok(Delivery::Sent)
    }
    async fn release_all_inputs(&self) -> Result<()> {
        Ok(())
    } // No physical inputs exist on a simulator.
}

fn render(marker: &str, count: u64) -> Result<Vec<u8>> {
    let mut pixels = vec![0u8; 1280 * 720];
    rect(&mut pixels, 48, 48, 1184, 624, 1);
    text(&mut pixels, 90, 110, "KVMHELM SIMULATED TARGET", 5, 2);
    text(
        &mut pixels,
        90,
        210,
        &format!("VISUAL CODE: {marker}"),
        7,
        3,
    );
    text(
        &mut pixels,
        90,
        310,
        &format!("INPUT ACTIONS RECEIVED: {count}"),
        4,
        4,
    );
    text(
        &mut pixels,
        90,
        580,
        &format!("UTC MILLISECONDS: {}", now_ms()),
        3,
        4,
    );
    let mut data = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut data, 1280, 720);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        encoder.set_palette(vec![
            0x10, 0x1b, 0x2a, 0x18, 0x28, 0x3d, 0x55, 0xdf, 0xb0, 255, 255, 255, 0xa5, 0xb8, 0xce,
        ]);
        let mut writer = encoder
            .write_header()
            .map_err(|_| GatewayError::new("FRAME_TIMEOUT"))?;
        writer
            .write_image_data(&pixels)
            .map_err(|_| GatewayError::new("FRAME_TIMEOUT"))?;
    }
    Ok(data)
}
fn rect(p: &mut [u8], x: usize, y: usize, w: usize, h: usize, color: u8) {
    for row in y..(y + h).min(720) {
        p[row * 1280 + x.min(1280)..row * 1280 + (x + w).min(1280)].fill(color);
    }
}
fn text(p: &mut [u8], x: usize, y: usize, value: &str, scale: usize, color: u8) {
    for (index, c) in value.chars().enumerate() {
        for (row, bits) in glyph(c).into_iter().enumerate() {
            for column in 0..5 {
                if bits & (1 << (4 - column)) != 0 {
                    rect(
                        p,
                        x + index * 6 * scale + column * scale,
                        y + row * scale,
                        scale,
                        scale,
                        color,
                    );
                }
            }
        }
    }
}
fn glyph(c: char) -> [u8; 7] {
    match c {
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [30, 17, 17, 30, 17, 17, 30],
        'C' => [14, 17, 16, 16, 16, 17, 14],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [14, 17, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [14, 4, 4, 4, 4, 4, 14],
        'J' => [7, 2, 2, 2, 18, 18, 12],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 25, 21, 19, 17, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'Q' => [14, 17, 17, 17, 21, 18, 13],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 21, 10],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 1, 2, 4, 8, 16, 31],
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        ':' => [0, 4, 4, 0, 4, 4, 0],
        _ => [0; 7],
    }
}
