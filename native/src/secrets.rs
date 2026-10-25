//! Byte-compatible with server/store.ts: IV[12] || GCM tag[16] || ciphertext.
use crate::{GatewayError, Result, private_fs};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, Tag, aead::AeadInPlace};
use rand::{RngCore, rngs::OsRng};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use zeroize::Zeroizing;

const MAX_SECRET_BYTES: u64 = 1024 * 1024;
struct Inner {
    dir: PathBuf,
    key: Zeroizing<[u8; 32]>,
    writing: Mutex<()>,
}
#[derive(Clone)]
pub struct Secrets {
    inner: Arc<Inner>,
}

fn storage_error() -> GatewayError {
    GatewayError::message(
        "STORAGE_ERROR",
        "Secret storage unavailable or authentication failed",
    )
}

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut data = [0u8; N];
    OsRng
        .try_fill_bytes(&mut data)
        .map_err(|_| storage_error())?;
    Ok(data)
}

impl Secrets {
    pub async fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || {
            private_fs::directory(&dir).map_err(|_| storage_error())?;
            let master = dir.join("master.key");
            if !master.exists() {
                // Publish only a complete key. create_new alone exposes an empty
                // file to a concurrent opener before the first writer finishes.
                let temporary = dir.join(format!(".{}.key", uuid::Uuid::new_v4()));
                let key = Zeroizing::new(random::<32>()?);
                let publish = (|| -> std::io::Result<()> {
                    let mut file = private_fs::create_new(&temporary)?;
                    file.write_all(key.as_ref())?;
                    file.sync_all()?;
                    drop(file);
                    match fs::hard_link(&temporary, &master) {
                        Ok(()) => Ok(()),
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                        Err(error) => Err(error),
                    }
                })();
                let _ = fs::remove_file(&temporary);
                publish.map_err(|_| storage_error())?;
            }
            private_fs::reject_link(&master).map_err(|_| storage_error())?;
            private_fs::restrict(&master, false).map_err(|_| storage_error())?;
            let mut file = fs::File::open(master).map_err(|_| storage_error())?;
            if file.metadata().map_err(|_| storage_error())?.len() != 32 {
                return Err(storage_error());
            }
            let mut key = Zeroizing::new([0u8; 32]);
            file.read_exact(key.as_mut()).map_err(|_| storage_error())?;
            Ok(Self {
                inner: Arc::new(Inner {
                    dir,
                    key,
                    writing: Mutex::new(()),
                }),
            })
        })
        .await
        .map_err(|_| storage_error())?
    }

    pub fn path(&self, reference: &str) -> Result<PathBuf> {
        if reference.is_empty()
            || reference.len() > 100
            || !reference
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(GatewayError::new("INVALID_SECRET_REF"));
        }
        Ok(self.inner.dir.join(format!("{reference}.secret")))
    }

    pub async fn set(&self, reference: &str, value: Value) -> Result<()> {
        let path = self.path(reference)?;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _writing = inner.writing.lock().map_err(|_| storage_error())?;
            let mut bytes =
                Zeroizing::new(serde_json::to_vec(&value).map_err(|_| storage_error())?);
            if bytes.len() as u64 > MAX_SECRET_BYTES - 28 {
                return Err(GatewayError::new("RESOURCE_LIMIT"));
            }
            let iv = random::<12>()?;
            let cipher =
                Aes256Gcm::new_from_slice(inner.key.as_ref()).map_err(|_| storage_error())?;
            let tag = cipher
                .encrypt_in_place_detached(Nonce::from_slice(&iv), b"", bytes.as_mut_slice())
                .map_err(|_| storage_error())?;
            let mut raw = Vec::with_capacity(28 + bytes.len());
            raw.extend_from_slice(&iv);
            raw.extend_from_slice(&tag);
            raw.extend_from_slice(&bytes);
            private_fs::atomic_write(&path, &raw).map_err(|_| storage_error())
        })
        .await
        .map_err(|_| storage_error())?
    }

    pub async fn get(&self, reference: Option<&str>) -> Result<Value> {
        let Some(reference) = reference else {
            return Ok(serde_json::json!({}));
        };
        let path = self.path(reference)?;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            private_fs::reject_link(&path).map_err(|_| storage_error())?;
            private_fs::restrict(&path, false).map_err(|_| storage_error())?;
            let file = fs::File::open(path).map_err(|_| storage_error())?;
            let mut raw = Vec::new();
            file.take(MAX_SECRET_BYTES + 1)
                .read_to_end(&mut raw)
                .map_err(|_| storage_error())?;
            if raw.len() < 28 || raw.len() as u64 > MAX_SECRET_BYTES {
                return Err(storage_error());
            }
            let cipher =
                Aes256Gcm::new_from_slice(inner.key.as_ref()).map_err(|_| storage_error())?;
            let mut data = Zeroizing::new(raw[28..].to_vec());
            cipher
                .decrypt_in_place_detached(
                    Nonce::from_slice(&raw[..12]),
                    b"",
                    data.as_mut_slice(),
                    Tag::from_slice(&raw[12..28]),
                )
                .map_err(|_| storage_error())?;
            serde_json::from_slice(&data).map_err(|_| storage_error())
        })
        .await
        .map_err(|_| storage_error())?
    }

    pub async fn delete(&self, reference: &str) -> Result<()> {
        let path = self.path(reference)?;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _writing = inner.writing.lock().map_err(|_| storage_error())?;
            match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(_) => Err(storage_error()),
            }
        })
        .await
        .map_err(|_| storage_error())?
    }

    /// Offline maintenance only. Preserve every referenced, recent, foreign or
    /// unrecognized file, including files left by an ambiguous config commit.
    pub async fn prune(&self, references: HashSet<String>) -> Result<Vec<String>> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _writing = inner.writing.lock().map_err(|_| storage_error())?;
            let mut deleted = Vec::new();
            for entry in fs::read_dir(&inner.dir).map_err(|_| storage_error())? {
                let entry = entry.map_err(|_| storage_error())?;
                let name = entry.file_name();
                let Some(reference) = name.to_str().and_then(|s| s.strip_suffix(".secret")) else {
                    continue;
                };
                if !generated_ref(reference) || references.contains(reference) {
                    continue;
                }
                let metadata = fs::symlink_metadata(entry.path()).map_err(|_| storage_error())?;
                if !metadata.is_file() || private_fs::reject_link(&entry.path()).is_err() {
                    continue;
                }
                let modified = metadata.modified().map_err(|_| storage_error())?;
                if SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or_default()
                    < Duration::from_secs(86400)
                {
                    continue;
                }
                fs::remove_file(entry.path()).map_err(|_| storage_error())?;
                deleted.push(reference.to_owned());
            }
            deleted.sort();
            Ok(deleted)
        })
        .await
        .map_err(|_| storage_error())?
    }
}

fn generated_ref(reference: &str) -> bool {
    fn uuid_chars(s: &str) -> bool {
        s.len() == 36
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
    }
    if let Some(uuid) = reference.strip_prefix("ca-") {
        uuid_chars(uuid)
    } else {
        reference.len() == 73
            && reference.as_bytes()[36] == b'-'
            && uuid_chars(&reference[..36])
            && uuid_chars(&reference[37..])
    }
}
