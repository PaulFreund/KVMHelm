use crate::{
    GatewayError, Result,
    contracts::Signal,
    media::{DriverFrame, Mime},
};
use std::{io::Cursor, sync::Arc};

pub const FRAME_BYTES: usize = 8 * 1024 * 1024;
const PIXELS: u64 = 16 * 1024 * 1024;

/// Read only metadata, as the original sharp.metadata path does. Parsing is off
/// the Tokio reactor and both encoded input and decoder allocation are bounded.
pub async fn inspect(data: Arc<[u8]>, signal: Signal) -> Result<DriverFrame> {
    if data.is_empty() {
        return Err(GatewayError::new("FRAME_TIMEOUT"));
    }
    if data.len() > FRAME_BYTES {
        return Err(GatewayError::new("RESOURCE_LIMIT"));
    }
    tokio::task::spawn_blocking(move || {
        let invalid = || GatewayError::message("FRAME_TIMEOUT", "Invalid device image");
        let format = image::guess_format(&data).map_err(|_| invalid())?;
        let mime_type = match format {
            image::ImageFormat::Jpeg => Mime::Jpeg,
            image::ImageFormat::Png => Mime::Png,
            _ => return Err(invalid()),
        };
        let mut reader = image::ImageReader::with_format(Cursor::new(data.as_ref()), format);
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(64 * 1024 * 1024);
        reader.limits(limits);
        let (width, height) = reader.into_dimensions().map_err(|_| invalid())?;
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > PIXELS {
            return Err(GatewayError::new("RESOURCE_LIMIT"));
        }
        Ok(DriverFrame {
            data,
            width,
            height,
            mime_type,
            signal,
            source_timestamp: None,
        })
    })
    .await
    .map_err(|_| GatewayError::new("FRAME_TIMEOUT"))?
}

/// Incremental marker parser for KVMD MJPEG. Keeping a scan offset avoids
/// repeatedly scanning the whole unfinished frame for each network fragment.
#[derive(Default)]
pub struct Mjpeg {
    buffer: Vec<u8>,
    frame: bool,
    scan: usize,
}
impl Mjpeg {
    pub fn push(&mut self, data: &[u8]) -> Result<()> {
        if data.len() > (12 * 1024 * 1024usize).saturating_sub(self.buffer.len()) {
            return Err(GatewayError::message(
                "RESOURCE_LIMIT",
                "MJPEG buffer limit",
            ));
        }
        self.buffer.extend_from_slice(data);
        Ok(())
    }
    pub fn next_frame(&mut self) -> Result<Option<Arc<[u8]>>> {
        if !self.frame {
            let Some(start) = self.buffer.windows(2).position(|p| p == [255, 216]) else {
                if self.buffer.len() > 65536 {
                    let tail = self.buffer.split_off(self.buffer.len() - 2);
                    self.buffer = tail;
                }
                return Ok(None);
            };
            self.buffer.drain(..start);
            self.frame = true;
            self.scan = 2;
        }
        let start = self.scan.min(self.buffer.len().saturating_sub(1));
        let Some(end) = self.buffer[start..]
            .windows(2)
            .position(|p| p == [255, 217])
            .map(|p| start + p + 2)
        else {
            if self.buffer.len() > FRAME_BYTES {
                return Err(GatewayError::new("RESOURCE_LIMIT"));
            }
            self.scan = self.buffer.len().saturating_sub(1);
            return Ok(None);
        };
        if end > FRAME_BYTES {
            return Err(GatewayError::new("RESOURCE_LIMIT"));
        }
        let frame = self.buffer[..end].to_vec().into();
        self.buffer.drain(..end);
        self.frame = false;
        self.scan = 0;
        Ok(Some(frame))
    }
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }
}
