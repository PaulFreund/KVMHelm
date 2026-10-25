use super::process_tree::Tree;
use crate::{GatewayError, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
const BYTES: usize = 16 * 1024 * 1024;
type Check = Arc<dyn Fn() -> Result<()> + Send + Sync>;
type Callback = Arc<dyn Fn(Value) -> bool + Send + Sync>;
struct Waiting {
    run: String,
    deadline: Instant,
    done: oneshot::Sender<Result<bool>>,
    _bytes: Arc<OwnedSemaphorePermit>,
}
struct Job {
    bytes: Vec<u8>,
    request: String,
    check: Option<Check>,
    // A process may exit before its queued frame reaches the writer. Keep the
    // byte reservation until both the queue and delivery bookkeeping release it.
    _bytes: Arc<OwnedSemaphorePermit>,
}
pub(super) struct Bridge {
    pub id: String,
    control: mpsc::Sender<Vec<u8>>,
    sender: mpsc::Sender<Job>,
    bytes: Arc<Semaphore>,
    pending: Mutex<HashMap<String, Waiting>>,
    cancel: CancellationToken,
    worker: Mutex<Option<JoinHandle<()>>>,
    exits: Mutex<HashMap<String, CancellationToken>>,
}
fn failure() -> GatewayError {
    GatewayError::new("PLUGIN_ERROR")
}
impl Bridge {
    pub fn alive(&self) -> bool {
        !self.cancel.is_cancelled()
    }
    pub async fn start(callback: Callback) -> Result<Arc<Self>> {
        let program = if let Some(path) = std::env::var_os("KVMHELM_NODE") {
            std::path::PathBuf::from(path)
        } else {
            let beside = std::env::current_exe().ok().and_then(|p| {
                p.parent()
                    .map(|p| p.join(if cfg!(windows) { "node.exe" } else { "node" }))
            });
            beside
                .filter(|p| p.is_file())
                .unwrap_or_else(|| "node".into())
        };
        let mut command = tokio::process::Command::new(program);
        command
            .arg("-e")
            .arg(include_str!("bridge.cjs"))
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for name in ["PATH", "SystemRoot", "TEMP", "HOME"] {
            if let Some(v) = std::env::var_os(name) {
                command.env(name, v);
            }
        }
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|_| {
            GatewayError::message(
                "DEPENDENCY_MISSING",
                "Plugin runtime requires Node (KVMHELM_NODE, beside the executable, or PATH)",
            )
        })?;
        let tree = match Tree::attach(&child) {
            Ok(tree) => tree,
            Err(error) => {
                let _ = child.kill().await;
                return Err(error);
            }
        };
        let mut input = child.stdin.take().unwrap();
        let mut output = child.stdout.take().unwrap();
        let (sender, mut jobs) = mpsc::channel::<Job>(128);
        let (control, mut commands) = mpsc::channel::<Vec<u8>>(16);
        let cancel = CancellationToken::new();
        let lifetime = cancel.clone();
        let bridge = Arc::new(Self {
            id: uuid::Uuid::new_v4().to_string(),
            sender,
            control,
            bytes: Arc::new(Semaphore::new(BYTES)),
            pending: Mutex::new(HashMap::new()),
            cancel,
            worker: Mutex::new(None),
            exits: Mutex::new(HashMap::new()),
        });
        let weak = Arc::downgrade(&bridge);
        let read_weak = weak.clone();
        let write_weak = weak.clone();
        let write_cancel = lifetime.clone();
        let read_cancel = lifetime.clone();
        let events = callback.clone();
        let writer = tokio::spawn(async move {
            loop {
                let (bytes, _admission) = tokio::select! {biased;_=write_cancel.cancelled()=>return,cmd=commands.recv()=>match cmd{Some(v)=>(v,None),None=>return},job=jobs.recv()=>{
                    let Some(job)=job else{return;};
                    if let Some(check)=job.check && let Err(error)=check(){if let Some(b)=write_weak.upgrade(){b.finish(&job.request,Err(error));}continue;}
                    (job.bytes, Some(job._bytes))
                }};
                let result = tokio::time::timeout(Duration::from_secs(2), async {
                    input.write_u32_le(bytes.len() as u32).await?;
                    input.write_all(&bytes).await
                })
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    write_cancel.cancel();
                    return;
                }
            }
        });
        let mut reader = tokio::spawn(async move {
            let run = async {
                loop {
                    let n = output.read_u32_le().await.map_err(|_| failure())? as usize;
                    if n > 1024 * 1024 {
                        return Err(failure());
                    }
                    let mut bytes = vec![0; n];
                    output.read_exact(&mut bytes).await.map_err(|_| failure())?;
                    let value: Value = serde_json::from_slice(&bytes).map_err(|_| failure())?;
                    let Some(b) = read_weak.upgrade() else {
                        return Ok(());
                    };
                    if value["type"] == "delivered" {
                        if let Some(id) = value["request"].as_str() {
                            b.finish(id, Ok(value["ok"] == true));
                        }
                    } else {
                        if value["type"] == "exit"
                            && let Some(run) = value["id"].as_str()
                        {
                            b.fail_run(run);
                        }
                        if !events(value) {
                            return Err(failure());
                        }
                    }
                }
            };
            tokio::select! {biased;_=read_cancel.cancelled()=>(),_=run=>()};
        });
        let bridge_id = bridge.id.clone();
        let worker = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            let mut reader_done = false;
            loop {
                tokio::select! {biased;_=lifetime.cancelled()=>break,_=&mut reader=>{reader_done=true;break},_=tick.tick()=>{
                    let Some(b)=weak.upgrade() else{break;};
                    let expired=b.pending.lock().unwrap().values().filter(|v|v.deadline<=Instant::now()).map(|v|v.run.clone()).collect::<std::collections::HashSet<_>>();
                    for run in expired{if b.command(json!({"type":"kill","id":run})).is_err(){lifetime.cancel();break;}}
                }}
            }
            lifetime.cancel();
            tree.kill();
            let _ = child.start_kill();
            let _ = child.wait().await;
            writer.abort();
            reader.abort();
            let _ = writer.await;
            if !reader_done {
                let _ = reader.await;
            }
            if let Some(b) = weak.upgrade() {
                for (_, waiting) in b.pending.lock().unwrap().drain() {
                    let _ = waiting.done.send(Err(failure()));
                }
                for (_, exit) in b.exits.lock().unwrap().drain() {
                    exit.cancel();
                }
            }
            callback(json!({"type":"bridge.exit","bridge":bridge_id}));
        });
        *bridge.worker.lock().unwrap() = Some(worker);
        Ok(bridge)
    }
    pub fn command(&self, value: Value) -> Result<()> {
        if !self.alive() {
            return Err(failure());
        }
        let bytes = serde_json::to_vec(&value).map_err(|_| failure())?;
        if bytes.len() > 256 * 1024 {
            return Err(GatewayError::new("RESOURCE_LIMIT"));
        }
        self.control
            .try_send(bytes)
            .map_err(|_| GatewayError::new("RESOURCE_LIMIT"))
    }
    pub fn enqueue(
        &self,
        run: &str,
        message: Value,
        data: Option<Arc<[u8]>>,
        check: Option<Check>,
    ) -> Result<oneshot::Receiver<Result<bool>>> {
        if !self.alive() {
            return Err(failure());
        }
        let base = serde_json::to_string(&message).map_err(|_| failure())?;
        let cost = base
            .len()
            .saturating_add(data.as_ref().map_or(0, |d| d.len().div_ceil(3) * 4))
            .saturating_add(512);
        if cost > BYTES {
            return Err(GatewayError::new("RESOURCE_LIMIT"));
        }
        let permit = self
            .bytes
            .clone()
            .try_acquire_many_owned(cost as u32)
            .map_err(|_| GatewayError::new("RESOURCE_LIMIT"))?;
        let permit = Arc::new(permit);
        let request = uuid::Uuid::new_v4().to_string();
        let mut value = json!({"type":"send","id":run,"request":request,"message":message});
        if let Some(data) = data {
            value["bytes"] = STANDARD.encode(&data).into();
        }
        let bytes = serde_json::to_vec(&value).map_err(|_| failure())?;
        if bytes.len() > BYTES {
            return Err(GatewayError::new("RESOURCE_LIMIT"));
        }
        let (done, wait) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.len() >= 128 {
                return Err(GatewayError::new("RESOURCE_LIMIT"));
            }
            pending.insert(
                request.clone(),
                Waiting {
                    run: run.into(),
                    deadline: Instant::now() + Duration::from_secs(2),
                    done,
                    _bytes: permit.clone(),
                },
            );
        }
        if self
            .sender
            .try_send(Job {
                bytes,
                request: request.clone(),
                check,
                _bytes: permit,
            })
            .is_err()
        {
            self.finish(&request, Err(GatewayError::new("RESOURCE_LIMIT")));
            return Err(GatewayError::new("RESOURCE_LIMIT"));
        }
        Ok(wait)
    }
    pub async fn send(
        &self,
        run: &str,
        message: Value,
        data: Option<Arc<[u8]>>,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        let cancel = cancel.clone();
        self.enqueue(
            run,
            message,
            data,
            Some(Arc::new(move || {
                if cancel.is_cancelled() {
                    Err(GatewayError::new("SOURCE_UNAVAILABLE"))
                } else {
                    Ok(())
                }
            })),
        )?
        .await
        .map_err(|_| failure())?
    }
    fn finish(&self, id: &str, result: Result<bool>) {
        if let Some(wait) = self.pending.lock().unwrap().remove(id) {
            let _ = wait.done.send(result);
        }
    }
    fn fail_run(&self, run: &str) {
        self.pending.lock().unwrap().retain(|_, v| v.run != run);
        if let Some(exit) = self.exits.lock().unwrap().remove(run) {
            exit.cancel();
        }
    }
    pub fn launch(&self, id: &str, path: &str, entry: &str) -> Result<CancellationToken> {
        let exit = CancellationToken::new();
        self.exits.lock().unwrap().insert(id.into(), exit.clone());
        if let Err(error) = self.command(json!({"type":"launch","id":id,"path":path,"entry":entry}))
        {
            self.fail_run(id);
            return Err(error);
        }
        Ok(exit)
    }
    pub fn abort(&self) {
        self.cancel.cancel();
    }
    pub async fn close(&self) {
        self.cancel.cancel();
        let worker = self.worker.lock().unwrap().take();
        if let Some(worker) = worker {
            let _ = worker.await;
        }
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
