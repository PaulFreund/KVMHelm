pub mod simulator;

use crate::{
    GatewayError, Result,
    contracts::{Action, Capabilities},
    media::DriverFrame,
};
use async_trait::async_trait;
use futures_util::Stream;
use std::{pin::Pin, sync::Arc, time::Duration};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

pub type VideoStream = Pin<Box<dyn Stream<Item = Result<DriverFrame>> + Send>>;

#[derive(Clone, Debug)]
pub enum DriverEvent {
    Disconnected,
    CapabilitiesChanged,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Sent,
    Acknowledged,
}

#[derive(Clone)]
pub struct ActionContext {
    pub cancellation: CancellationToken,
    pub width: u32,
    pub height: u32,
    pub assert_control: Arc<dyn Fn() -> Result<()> + Send + Sync>,
}
impl ActionContext {
    pub fn check(&self) -> Result<()> {
        (self.assert_control)()?;
        if self.cancellation.is_cancelled() {
            return Err(GatewayError::new("LEASE_REVOKED"));
        }
        Ok(())
    }
    pub async fn delay(&self, duration: Duration) -> Result<()> {
        self.check()?;
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled()=>Err(GatewayError::new("LEASE_REVOKED")),
            _ = tokio::time::sleep(duration)=>self.check(),
        }
    }
}

#[async_trait]
pub trait KvmDriver: Send + Sync {
    fn events(&self) -> broadcast::Receiver<DriverEvent>;
    async fn connect(&self, cancellation: CancellationToken) -> Result<()>;
    async fn disconnect(&self) -> Result<()>;
    async fn capabilities(&self) -> Result<Capabilities>;
    async fn snapshot(&self, cancellation: CancellationToken) -> Result<DriverFrame>;
    /// None is an explicitly unsupported optional streaming capability.
    async fn subscribe_video(
        &self,
        _cancellation: CancellationToken,
    ) -> Result<Option<VideoStream>> {
        Ok(None)
    }
    fn validate(&self, actions: &[Action], width: u32, height: u32) -> Result<()>;
    async fn execute(&self, action: &Action, context: &ActionContext) -> Result<Delivery>;
    async fn release_all_inputs(&self) -> Result<()>;
}
