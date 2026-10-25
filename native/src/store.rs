//! SQLite schema v1, used unchanged by the Node implementation. One dedicated
//! thread owns the connection; async request handling never runs SQLite itself.
use crate::{GatewayError, Result, contracts::MAX_SAFE_INTEGER, private_fs};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicUsize, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread,
    time::Duration,
};
use tokio::sync::oneshot;

const READY: u8 = 1;
const FAILED: u8 = 2;
const CLOSED: u8 = 3;
const MAX_PENDING: usize = 256;
const DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Devices,
    Tokens,
    Preferences,
    Plugins,
}
impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Devices => "devices",
            Self::Tokens => "tokens",
            Self::Preferences => "preferences",
            Self::Plugins => "plugins",
        }
    }
}

enum Operation {
    Ping,
    List(Kind),
    Get(Kind, String),
    Put(Kind, String, Value, i64),
    Delete(Kind, String, i64),
    Backup(PathBuf),
}

struct Request {
    op: Operation,
    reply: oneshot::Sender<Result<Value>>,
    pending: Pending,
}

struct Pending(Arc<AtomicUsize>);
impl Drop for Pending {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
struct Inner {
    directory: PathBuf,
    sender: Mutex<Option<SyncSender<Request>>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    state: Arc<AtomicU8>,
    pending: Arc<AtomicUsize>,
    migration_backup: Option<PathBuf>,
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

impl Store {
    pub async fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let directory = dir.clone();
        let (sender, receiver) = mpsc::sync_channel::<Request>(MAX_PENDING);
        let (startup, ready) = oneshot::channel();
        let state = Arc::new(AtomicU8::new(0));
        let worker_state = state.clone();
        let worker = thread::Builder::new()
            .name("kvmhelm-sqlite".into())
            .spawn(move || {
                let initialized = initialize(&dir);
                let (mut db, backup) = match initialized {
                    Ok(value) => value,
                    Err(error) => {
                        worker_state.store(FAILED, Ordering::Release);
                        let _ = startup.send(Err(error));
                        return;
                    }
                };
                // Opening may have timed out; never revive that handle.
                if worker_state
                    .compare_exchange(0, READY, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    return;
                }
                if startup.send(Ok(backup)).is_err() {
                    return;
                }
                while let Ok(Request { op, reply, pending }) = receiver.recv() {
                    let result = if worker_state.load(Ordering::Acquire) == READY {
                        execute(&mut db, op)
                    } else {
                        Err(GatewayError::storage())
                    };
                    drop(pending);
                    let _ = reply.send(result);
                }
            })
            .map_err(|_| GatewayError::storage())?;
        let backup = match tokio::time::timeout(DEADLINE, ready).await {
            Ok(Ok(result)) => result?,
            _ => {
                state.store(FAILED, Ordering::Release);
                return Err(GatewayError::storage());
            }
        };
        Ok(Self {
            inner: Arc::new(Inner {
                directory,
                sender: Mutex::new(Some(sender)),
                worker: Mutex::new(Some(worker)),
                state,
                pending: Arc::new(AtomicUsize::new(0)),
                migration_backup: backup,
            }),
        })
    }

    pub fn state(&self) -> &'static str {
        match self.inner.state.load(Ordering::Acquire) {
            READY => "ready",
            FAILED => "failed",
            CLOSED => "closed",
            _ => "starting",
        }
    }
    pub fn directory(&self) -> &Path {
        &self.inner.directory
    }

    pub fn migration_backup(&self) -> Option<&Path> {
        self.inner.migration_backup.as_deref()
    }

    pub fn pending_requests(&self) -> usize {
        self.inner.pending.load(Ordering::Acquire)
    }

    async fn call(&self, op: Operation) -> Result<Value> {
        if self.inner.state.load(Ordering::Acquire) != READY {
            return Err(GatewayError::storage());
        }
        self.inner
            .pending
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_PENDING).then_some(n + 1)
            })
            .map_err(|_| GatewayError::new("RESOURCE_LIMIT"))?;
        // The actor owns this reservation after admission. Dropping a caller's
        // future must neither cancel its accepted write nor free its queue slot.
        let pending = Pending(self.inner.pending.clone());
        let (reply, receiver) = oneshot::channel();
        {
            let sender = self
                .inner
                .sender
                .lock()
                .map_err(|_| GatewayError::storage())?;
            let sender = sender.as_ref().ok_or_else(GatewayError::storage)?;
            match sender.try_send(Request { op, reply, pending }) {
                Ok(()) => (),
                Err(TrySendError::Full(_)) => return Err(GatewayError::new("RESOURCE_LIMIT")),
                Err(TrySendError::Disconnected(_)) => {
                    self.fail();
                    return Err(GatewayError::storage());
                }
            }
        }
        match tokio::time::timeout(DEADLINE, receiver).await {
            Ok(Ok(result)) => result,
            _ => {
                // A write may already have committed. Reject future operations,
                // never retry it or report that its outcome is known to be absent.
                self.fail();
                Err(GatewayError::storage())
            }
        }
    }

    fn fail(&self) {
        let _ =
            self.inner
                .state
                .compare_exchange(READY, FAILED, Ordering::AcqRel, Ordering::Acquire);
    }

    pub async fn health(&self) -> Result<Value> {
        self.call(Operation::Ping).await?;
        Ok(json!({"state":self.state(),"pending":self.inner.pending.load(Ordering::Acquire)}))
    }

    pub async fn list<T: DeserializeOwned>(&self, kind: Kind) -> Result<Vec<T>> {
        serde_json::from_value(self.call(Operation::List(kind)).await?)
            .map_err(|_| GatewayError::storage())
    }

    pub async fn get<T: DeserializeOwned>(&self, kind: Kind, key: &str) -> Result<Option<T>> {
        serde_json::from_value(self.call(Operation::Get(kind, key.into())).await?)
            .map_err(|_| GatewayError::storage())
    }

    pub async fn put(&self, kind: Kind, key: &str, value: Value, revision: i64) -> Result<Value> {
        if !value.is_object() || !(0..MAX_SAFE_INTEGER).contains(&revision) {
            return Err(GatewayError::invalid());
        }
        self.call(Operation::Put(kind, key.into(), value, revision))
            .await
    }

    pub async fn delete(&self, kind: Kind, key: &str, revision: i64) -> Result<()> {
        self.call(Operation::Delete(kind, key.into(), revision))
            .await
            .map(|_| ())
    }

    /// SQLite's online backup API includes committed WAL content. It refuses to
    /// overwrite an existing destination and creates a private file first.
    pub async fn backup(&self, destination: impl AsRef<Path>) -> Result<()> {
        self.call(Operation::Backup(destination.as_ref().to_path_buf()))
            .await
            .map(|_| ())
    }

    pub async fn close(&self) -> Result<()> {
        self.inner.state.store(CLOSED, Ordering::Release);
        self.inner
            .sender
            .lock()
            .map_err(|_| GatewayError::storage())?
            .take();
        let worker = self
            .inner
            .worker
            .lock()
            .map_err(|_| GatewayError::storage())?
            .take();
        if let Some(worker) = worker {
            tokio::task::spawn_blocking(move || worker.join())
                .await
                .map_err(|_| GatewayError::storage())?
                .map_err(|_| GatewayError::storage())?;
        }
        Ok(())
    }
}

fn initialize(dir: &Path) -> Result<(Connection, Option<PathBuf>)> {
    private_fs::directory(dir).map_err(|_| GatewayError::storage())?;
    let path = dir.join("config.sqlite");
    if path.exists() {
        private_fs::reject_link(&path).map_err(|_| GatewayError::storage())?;
    } else {
        private_fs::create_new(&path).map_err(|_| GatewayError::storage())?;
    }
    private_fs::restrict(&path, false).map_err(|_| GatewayError::storage())?;
    let db = Connection::open(&path).map_err(|_| GatewayError::storage())?;
    db.busy_timeout(Duration::from_secs(3))
        .map_err(|_| GatewayError::storage())?;
    let version: i64 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|_| GatewayError::storage())?;
    if !(0..=1).contains(&version) {
        return Err(GatewayError::message(
            "STORAGE_ERROR",
            "Database schema is newer than or incompatible with this KVMHelm version",
        ));
    }
    let tables: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| GatewayError::storage())?;
    let backup = if version == 0 && tables != 0 {
        let target = dir.join(format!("config.sqlite.pre-v1-{}.bak", uuid::Uuid::new_v4()));
        backup_to(&db, &target)?;
        Some(target)
    } else {
        None
    };
    db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS documents (kind TEXT NOT NULL,id TEXT NOT NULL,json TEXT NOT NULL,revision INTEGER NOT NULL,PRIMARY KEY(kind,id)); PRAGMA user_version=1;")
        .map_err(|_| GatewayError::storage())?;
    // Check the stored shape before reporting readiness.
    db.prepare("SELECT kind,id,json,revision FROM documents LIMIT 0")
        .map_err(|_| GatewayError::storage())?;
    Ok((db, backup))
}

fn read_document(text: String, revision: i64) -> Result<Value> {
    let mut doc: Value = serde_json::from_str(&text).map_err(|_| GatewayError::storage())?;
    let object = doc.as_object_mut().ok_or_else(GatewayError::storage)?;
    object.insert("revision".into(), revision.into());
    Ok(doc)
}

fn backup_to(db: &Connection, path: &Path) -> Result<()> {
    let file = private_fs::create_new(path).map_err(|_| GatewayError::storage())?;
    drop(file);
    if db.backup("main", path, None).is_err() {
        let _ = std::fs::remove_file(path);
        return Err(GatewayError::storage());
    }
    Ok(())
}

fn execute(db: &mut Connection, op: Operation) -> Result<Value> {
    match op {
        Operation::Ping => {
            db.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .map_err(|_| GatewayError::storage())?;
            Ok(json!({"ok":1}))
        }
        Operation::List(kind) => {
            let mut stmt = db
                .prepare("SELECT json,revision FROM documents WHERE kind=?")
                .map_err(|_| GatewayError::storage())?;
            let rows = stmt
                .query_map([kind.name()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(|_| GatewayError::storage())?;
            let mut docs = Vec::new();
            for row in rows {
                let (text, revision) = row.map_err(|_| GatewayError::storage())?;
                docs.push(read_document(text, revision)?);
            }
            Ok(docs.into())
        }
        Operation::Get(kind, key) => {
            let row: Option<(String, i64)> = db
                .query_row(
                    "SELECT json,revision FROM documents WHERE kind=? AND id=?",
                    params![kind.name(), key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|_| GatewayError::storage())?;
            match row {
                Some((text, revision)) => read_document(text, revision),
                None => Ok(Value::Null),
            }
        }
        Operation::Put(kind, key, mut value, revision) => {
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| GatewayError::storage())?;
            let current: Option<i64> = tx
                .query_row(
                    "SELECT revision FROM documents WHERE kind=? AND id=?",
                    params![kind.name(), key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| GatewayError::storage())?;
            if current.unwrap_or(0) != revision {
                return Err(GatewayError::new("REVISION_CONFLICT"));
            }
            let next = revision + 1;
            let text = serde_json::to_string(&value).map_err(|_| GatewayError::storage())?;
            tx.execute("INSERT INTO documents VALUES(?,?,?,?) ON CONFLICT(kind,id) DO UPDATE SET json=excluded.json,revision=excluded.revision",params![kind.name(),key,text,next]).map_err(|_| GatewayError::storage())?;
            tx.commit().map_err(|_| GatewayError::storage())?;
            value
                .as_object_mut()
                .ok_or_else(GatewayError::invalid)?
                .insert("revision".into(), next.into());
            Ok(value)
        }
        Operation::Delete(kind, key, revision) => {
            let changed = db
                .execute(
                    "DELETE FROM documents WHERE kind=? AND id=? AND revision=?",
                    params![kind.name(), key, revision],
                )
                .map_err(|_| GatewayError::storage())?;
            if changed == 0 {
                return Err(GatewayError::new("REVISION_CONFLICT"));
            }
            Ok(Value::Bool(true))
        }
        Operation::Backup(path) => {
            backup_to(db, &path)?;
            Ok(Value::Bool(true))
        }
    }
}
