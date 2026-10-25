use crate::shared::{Flight, unknown};
use crate::{GatewayError, Result};
use futures_util::FutureExt;
use std::{
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

type Work = Pin<Box<dyn Future<Output = ()> + Send>>;
struct Completion<T: Clone>(Arc<Flight<T>>);
impl<T: Clone> Drop for Completion<T> {
    fn drop(&mut self) {
        self.0.finish(Err(unknown()));
    }
}
struct Slot(Arc<AtomicUsize>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
struct Job {
    future: Work,
    _slot: Option<Slot>,
}

/// Admission is synchronous and FIFO; the worker, rather than the HTTP caller,
/// owns every accepted job. The limit includes the currently executing job.
pub(crate) struct DeviceQueue {
    sender: Mutex<Option<mpsc::Sender<Job>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    pending: Arc<AtomicUsize>,
}
impl DeviceQueue {
    pub fn new() -> Self {
        let (sender, mut receiver) = mpsc::channel::<Job>(64);
        let worker = tokio::spawn(async move {
            while let Some(job) = receiver.recv().await {
                // Core jobs handle driver panics and finish their journal. This
                // boundary also prevents an unrelated callback killing the FIFO.
                let _ = AssertUnwindSafe(job.future).catch_unwind().await;
            }
        });
        Self {
            sender: Mutex::new(Some(sender)),
            worker: Mutex::new(Some(worker)),
            pending: Arc::new(AtomicUsize::new(0)),
        }
    }
    pub fn submit(&self, future: impl Future<Output = ()> + Send + 'static) -> Result<()> {
        self.pending
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 64).then_some(n + 1)
            })
            .map_err(|_| GatewayError::message("RESOURCE_LIMIT", "Device command queue full"))?;
        let job = Job {
            future: Box::pin(future),
            _slot: Some(Slot(self.pending.clone())),
        };
        self.sender
            .lock()
            .unwrap()
            .as_ref()
            .ok_or_else(|| GatewayError::new("DEVICE_OFFLINE"))?
            .try_send(job)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => GatewayError::new("RESOURCE_LIMIT"),
                mpsc::error::TrySendError::Closed(_) => GatewayError::new("DEVICE_OFFLINE"),
            })
    }
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }
    pub async fn run_owned<T, F>(&self, work: F) -> Result<T>
    where
        T: Clone + Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let flight = Arc::new(Flight::new());
        let done = flight.clone();
        self.submit(async move {
            let _complete = Completion(done.clone());
            let result = AssertUnwindSafe(work)
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err(unknown()));
            done.finish(result);
        })?;
        flight.wait().await
    }
    pub async fn barrier(&self) -> Result<()> {
        let (done, wait) = oneshot::channel();
        let sender = self
            .sender
            .lock()
            .unwrap()
            .as_ref()
            .cloned()
            .ok_or_else(|| GatewayError::new("DEVICE_OFFLINE"))?;
        sender
            .send(Job {
                future: Box::pin(async move {
                    let _ = done.send(());
                }),
                _slot: None,
            })
            .await
            .map_err(|_| GatewayError::new("DEVICE_OFFLINE"))?;
        wait.await.map_err(|_| GatewayError::new("DEVICE_OFFLINE"))
    }
    pub async fn close(&self) {
        self.sender.lock().unwrap().take();
        let worker = self.worker.lock().unwrap().take();
        if let Some(worker) = worker {
            let _ = worker.await;
        }
    }
}
