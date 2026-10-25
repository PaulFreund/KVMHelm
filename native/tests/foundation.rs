use kvmhelm::{
    auth::{Auth, now_ms},
    contracts::{Action, Device, Layout, SCOPES, ToolRequest, Validate},
    keyboard::{key_name, text_keys, usb_key, validate_hid},
    network::*,
    secrets::Secrets,
    store::{Kind, Store},
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    time::{Duration, SystemTime},
};

#[test]
fn strict_contracts_preserve_defaults_and_zod_code_point_limits() {
    let device = Device::parse(json!({"name":"test"})).unwrap();
    assert_eq!(device.minimum_action_latency_ms, 30);
    assert_eq!(device.driver_id, "pikvm-v4");
    assert!(device.device_id.is_none());
    for invalid in [
        json!({"name":"test","extra":true}),
        json!({"name":"test","address":null}),
        json!({"name":"test","tls":{"insecure":null}}),
        json!({"name":"test","minimum_action_latency_ms":-1}),
        json!({"name":"test","minimum_action_latency_ms":0.5}),
    ] {
        assert_eq!(Device::parse(invalid).unwrap_err().code, "INVALID_ARGUMENT");
    }
    assert!(Device::parse(json!({"name":"🙂".repeat(50),"minimum_action_latency_ms":1.0})).is_ok());
    assert!(Device::parse(json!({"name":"🙂".repeat(100)})).is_ok());
    assert!(Device::parse(json!({"name":"🙂".repeat(101)})).is_err());
    assert!(Action::parse(json!({"type":"type","text":"🙂".repeat(4096)})).is_ok());
    assert!(Action::parse(json!({"type":"type","text":"🙂".repeat(4097)})).is_err());
    assert_eq!(
        serde_json::to_value(Action::parse(json!({"type":"wait"})).unwrap()).unwrap(),
        json!({"type":"wait","duration_ms":100})
    );
    assert!(Action::parse(json!({"type":"screenshot","before_ms":0})).is_err());
    assert!(Action::parse(json!({"type":"click","x":9007199254740992u64,"y":0})).is_err());
    assert!(Action::parse(json!({"type":"drag","path":[{"x":0,"y":0}]})).is_err());
    let screenshot =
        ToolRequest::parse_tool("computer_screenshot", json!({"session_id":"s"})).unwrap();
    assert_eq!(
        screenshot.normalized_args(),
        json!({"session_id":"s","max_age_ms":0,"timeout_ms":1500})
    );
    assert!(ToolRequest::parse_tool("list_computers", json!({"tags":null})).is_err());
    assert!(
        ToolRequest::parse_tool(
            "open_computer",
            json!({"computer_id":"d","mode":"control","profile":null})
        )
        .is_err()
    );
}

#[test]
fn keyboard_mapping_and_prevalidation_cover_the_complete_batch() {
    assert_eq!(
        text_keys("Yz\n", Layout::De).unwrap(),
        vec![vec!["ShiftLeft", "KeyZ"], vec!["KeyY"], vec!["Enter"]]
    );
    assert_eq!(
        text_keys("@ä€", Layout::De).unwrap(),
        vec![
            vec!["AltRight", "KeyQ"],
            vec!["Quote"],
            vec!["AltRight", "KeyE"]
        ]
    );
    assert_eq!(
        text_keys("´`^", Layout::De).unwrap(),
        vec![
            vec!["Equal"],
            vec!["Space"],
            vec!["ShiftLeft", "Equal"],
            vec!["Space"],
            vec!["Backquote"],
            vec!["Space"]
        ]
    );
    assert_eq!(
        text_keys("@", Layout::Us).unwrap(),
        vec![vec!["ShiftLeft", "Digit2"]]
    );
    for c in b' '..=b'~' {
        assert!(
            text_keys(&char::from(c).to_string(), Layout::Us).is_ok(),
            "ASCII {c}"
        );
    }
    assert_eq!(key_name("ALTGR").unwrap(), "AltRight");
    for (key, usage) in [
        ("a", 4),
        ("z", 29),
        ("1", 30),
        ("0", 39),
        ("F1", 58),
        ("F12", 69),
        ("F13", 104),
        ("F24", 115),
        ("ALTGR", 230),
        ("IntlBackslash", 100),
    ] {
        assert_eq!(usb_key(key).unwrap(), usage);
    }
    for key in ["F01", "F0", "F25", "keyA", "🙂", ""] {
        assert!(key_name(key).is_err());
    }
    let actions = vec![
        Action::parse(json!({"type":"click","x":1,"y":1})).unwrap(),
        Action::Type {
            text: "ok🙂".into(),
        },
    ];
    assert_eq!(
        validate_hid(&actions, 100, 100, true, true, Layout::De)
            .unwrap_err()
            .code,
        "UNSUPPORTED_TEXT"
    );
    assert_eq!(
        validate_hid(&actions[..1], 1, 1, true, true, Layout::Us)
            .unwrap_err()
            .code,
        "VIEW_CHANGED"
    );
    assert_eq!(
        validate_hid(&actions[..1], 100, 100, false, true, Layout::Us)
            .unwrap_err()
            .code,
        "UNSUPPORTED_ACTION"
    );
}

#[test]
fn lan_boundaries_and_ice_filtering_reject_public_destinations() {
    for address in [
        "10.0.0.1",
        "127.0.0.2",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.1.1",
        "169.254.0.1",
        "::1",
        "fd12::1",
        "fe80::1",
        "::ffff:192.168.1.1",
    ] {
        assert!(is_local_address(address), "{address}");
    }
    for address in [
        "0.0.0.0",
        "172.15.0.1",
        "172.32.0.1",
        "100.64.0.1",
        "192.167.1.1",
        "8.8.8.8",
        "2001:db8::1",
        "::",
        "ff02::1",
        "localhost",
        "127.0.0.1.evil",
    ] {
        assert!(!is_local_address(address), "{address}");
    }
    assert!(check_resolved_addresses(&[]).is_err());
    assert!(
        check_resolved_addresses(&["127.0.0.1".parse().unwrap(), "8.8.8.8".parse().unwrap()])
            .is_err()
    );
    assert!(check_resolved_addresses(&["192.168.1.1".parse().unwrap()]).is_ok());
    for origin in [
        "file:///tmp/x",
        "http://user:pass@127.0.0.1",
        "http://127.0.0.1/api",
        "http://8.8.8.8",
        "https://localhost/?q=x",
    ] {
        assert!(device_origin(origin).is_err(), "{origin}");
    }
    assert!(device_origin("https://kvm.lan:443").is_ok());
    let sdp = "v=0\na=candidate:1 1 udp 1 192.168.1.1 1234 typ host\na=candidate:2 1 udp 1 8.8.8.8 1234 typ srflx\n";
    assert!(check_ice_description(sdp).is_err());
    let local = local_ice_description(sdp);
    assert!(local.contains("192.168.1.1"));
    assert!(!local.contains("8.8.8.8"));
    assert!(check_ice_description(&local).is_ok());
}

#[tokio::test]
async fn store_transactions_cas_persistence_and_fail_closed_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    assert_eq!(store.state(), "ready");
    assert!(store.list::<Value>(Kind::Devices).await.unwrap().is_empty());
    let key = "id' OR 1=1; --";
    let saved = store
        .put(Kind::Devices, key, json!({"name":"kept","revision":999}), 0)
        .await
        .unwrap();
    assert_eq!(saved["revision"], 1);
    let (a, b) = tokio::join!(
        store.put(Kind::Devices, key, json!({"name":"A"}), 1),
        store.put(Kind::Devices, key, json!({"name":"B"}), 1)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(a.err().or(b.err()).unwrap().code, "REVISION_CONFLICT");
    let stored: Value = store.get(Kind::Devices, key).await.unwrap().unwrap();
    assert_eq!(stored["revision"], 2);
    assert_eq!(
        store.delete(Kind::Devices, key, 1).await.unwrap_err().code,
        "REVISION_CONFLICT"
    );
    store.close().await.unwrap();
    assert!(store.health().await.is_err());
    assert!(
        store
            .put(Kind::Preferences, "owner", json!({}), 0)
            .await
            .is_err()
    );
    let reopened = Store::open(dir.path()).await.unwrap();
    assert_eq!(
        reopened.get::<Value>(Kind::Devices, key).await.unwrap(),
        Some(stored)
    );
    reopened.delete(Kind::Devices, key, 2).await.unwrap();
    assert_eq!(
        reopened.get::<Value>(Kind::Devices, key).await.unwrap(),
        None
    );
    reopened.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_store_queue_survives_cancelled_callers_without_losing_accepted_writes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let blocker = Connection::open(dir.path().join("config.sqlite")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    let mut cancel = None;
    for n in 0..256 {
        let store = store.clone();
        let task = tasks.spawn(async move {
            store
                .put(Kind::Preferences, &format!("{n}"), json!({"n":n}), 0)
                .await
        });
        if n == 0 {
            cancel = Some(task);
        }
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while store.pending_requests() != 256 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("requests must reserve their bounded queue slots");
    cancel.unwrap().abort();
    // Await the cancellation to prove its future and local resources have gone.
    assert!(tasks.join_next().await.unwrap().unwrap_err().is_cancelled());
    assert_eq!(store.pending_requests(), 256);
    assert_eq!(store.health().await.unwrap_err().code, "RESOURCE_LIMIT");
    blocker.execute_batch("COMMIT").unwrap();
    while let Some(result) = tasks.join_next().await {
        result.unwrap().unwrap();
    }
    assert_eq!(
        store.list::<Value>(Kind::Preferences).await.unwrap().len(),
        256
    );
    assert_eq!(store.pending_requests(), 0);
    store.close().await.unwrap();
}

#[tokio::test]
async fn consistent_backup_includes_wal_and_refuses_to_overwrite_an_old_backup() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("original")).await.unwrap();
    store
        .put(Kind::Preferences, "owner", json!({"theme":"dark"}), 0)
        .await
        .unwrap();
    let backup = dir.path().join("backup.sqlite");
    store.backup(&backup).await.unwrap();
    let bytes = fs::read(&backup).unwrap();
    assert!(store.backup(&backup).await.is_err());
    assert_eq!(fs::read(&backup).unwrap(), bytes);
    let restore = dir.path().join("restored");
    fs::create_dir(&restore).unwrap();
    fs::copy(&backup, restore.join("config.sqlite")).unwrap();
    let restored = Store::open(&restore).await.unwrap();
    assert_eq!(
        restored
            .get::<Value>(Kind::Preferences, "owner")
            .await
            .unwrap()
            .unwrap()["theme"],
        "dark"
    );
    restored.close().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn unknown_future_schema_is_rejected_before_mutating_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.sqlite");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(
        "CREATE TABLE future(value); INSERT INTO future VALUES('keep'); PRAGMA user_version=2;",
    )
    .unwrap();
    drop(db);
    let before = fs::read(&path).unwrap();
    assert!(Store::open(dir.path()).await.is_err());
    assert_eq!(fs::read(path).unwrap(), before);
}

#[tokio::test]
async fn schema_zero_is_backed_up_before_upgrading_and_schema_one_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let db = Connection::open(dir.path().join("config.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE documents(kind TEXT NOT NULL,id TEXT NOT NULL,json TEXT NOT NULL,revision INTEGER NOT NULL,PRIMARY KEY(kind,id)); INSERT INTO documents VALUES('preferences','owner','{\"theme\":\"light\"}',7);").unwrap();
    drop(db);
    let store = Store::open(dir.path()).await.unwrap();
    let path = store
        .migration_backup()
        .expect("nonempty old schema needs a backup")
        .to_path_buf();
    let backup = Connection::open(path).unwrap();
    assert_eq!(
        backup
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        0
    );
    assert_eq!(
        backup
            .query_row("SELECT revision FROM documents", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        7
    );
    drop(backup);
    assert_eq!(
        store
            .get::<Value>(Kind::Preferences, "owner")
            .await
            .unwrap()
            .unwrap()["revision"],
        7
    );
    store.close().await.unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    assert!(store.migration_backup().is_none());
    store.close().await.unwrap();
}

#[tokio::test]
async fn malformed_documents_fail_with_no_sql_or_contents_in_the_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let db = Connection::open(dir.path().join("config.sqlite")).unwrap();
    db.execute(
        "INSERT INTO documents VALUES('preferences','broken','private-invalid-json',1)",
        [],
    )
    .unwrap();
    let error = store
        .get::<Value>(Kind::Preferences, "broken")
        .await
        .unwrap_err();
    assert_eq!(error.code, "STORAGE_ERROR");
    assert!(!error.to_string().contains("private-invalid-json"));
    assert!(store.health().await.is_ok());
    drop(db);
    store.close().await.unwrap();
}

#[tokio::test]
async fn encrypted_secrets_survive_restart_and_detect_tampering_and_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Secrets::open(dir.path()).await.unwrap();
    let value = json!({"username":"admin","password":"fixture-ä🙂","nested":[1,true]});
    assert_eq!(secrets.get(None).await.unwrap(), json!({}));
    secrets.set("device", value.clone()).await.unwrap();
    let original = fs::read(secrets.path("device").unwrap()).unwrap();
    assert!(!original.windows(7).any(|w| w == b"fixture"));
    secrets.set("device", value.clone()).await.unwrap();
    assert_ne!(original, fs::read(secrets.path("device").unwrap()).unwrap());
    drop(secrets);
    let secrets = Secrets::open(dir.path()).await.unwrap();
    assert_eq!(secrets.get(Some("device")).await.unwrap(), value);
    for index in [0, 12, 28] {
        let mut corrupt = original.clone();
        corrupt[index] ^= 1;
        fs::write(secrets.path("bad").unwrap(), corrupt).unwrap();
        assert!(secrets.get(Some("bad")).await.is_err());
    }
    for size in [0, 1, 12, 27, 28] {
        fs::write(secrets.path("bad").unwrap(), vec![0u8; size]).unwrap();
        assert!(secrets.get(Some("bad")).await.is_err());
    }
    for reference in [
        "",
        "../master",
        "a/b",
        "a\\b",
        "CON:stream",
        "ä",
        &"x".repeat(101),
    ] {
        assert_eq!(
            secrets.path(reference).unwrap_err().code,
            "INVALID_SECRET_REF"
        );
    }
    secrets.delete("missing").await.unwrap();
    secrets.delete("device").await.unwrap();
    assert!(secrets.get(Some("device")).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_secret_openers_publish_only_one_complete_master_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for n in 0..8 {
        let dir = dir.path().to_path_buf();
        tasks.spawn(async move {
            let secrets = Secrets::open(dir).await.unwrap();
            secrets
                .set(&format!("ref{n}"), json!({"n":n}))
                .await
                .unwrap();
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    let secrets = Secrets::open(dir.path()).await.unwrap();
    for n in 0..8 {
        assert_eq!(
            secrets.get(Some(&format!("ref{n}"))).await.unwrap(),
            json!({"n":n})
        );
    }
    assert_eq!(fs::read(dir.path().join("master.key")).unwrap().len(), 32);
}

#[tokio::test]
async fn invalid_master_key_is_never_replaced() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("master.key"), b"bad key").unwrap();
    assert!(Secrets::open(dir.path()).await.is_err());
    assert_eq!(fs::read(dir.path().join("master.key")).unwrap(), b"bad key");
}

#[tokio::test]
async fn cleanup_preserves_referenced_recent_and_foreign_files() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Secrets::open(dir.path()).await.unwrap();
    let orphan = format!("ca-{}", uuid::Uuid::new_v4());
    let referenced = format!("{}-{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let recent = format!("ca-{}", uuid::Uuid::new_v4());
    for reference in [&orphan, &referenced, &recent, "foreign"] {
        secrets.set(reference, json!({"test":true})).await.unwrap();
    }
    let old = SystemTime::now() - Duration::from_secs(86410);
    for reference in [&orphan, &referenced, "foreign"] {
        fs::File::options()
            .write(true)
            .open(secrets.path(reference).unwrap())
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
    }
    assert_eq!(
        secrets
            .prune(HashSet::from([referenced.clone()]))
            .await
            .unwrap(),
        vec![orphan.clone()]
    );
    assert!(!secrets.path(&orphan).unwrap().exists());
    for reference in [&referenced, &recent, "foreign"] {
        assert!(secrets.path(reference).unwrap().exists());
    }
    assert!(dir.path().join("master.key").exists());
}

#[tokio::test]
async fn pat_hashes_ui_csrf_and_revocation_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let auth = Auth::load(store.clone()).await.unwrap();
    let created = auth
        .create(
            json!({"name":"owner","scopes":SCOPES,"devices":["*"]}),
            None,
        )
        .await
        .unwrap();
    assert!(created.token.starts_with("kh_"));
    assert_eq!(created.token.len(), 83);
    assert!(created.metadata.get("hash").is_none());
    let identity = auth.authenticate(&created.token, "client-a").unwrap();
    auth.check(&identity, Some("input:write"), Some("device"))
        .unwrap();
    assert!(
        auth.list()
            .unwrap()
            .iter()
            .all(|t| t.get("hash").is_none() && t.get("token").is_none())
    );
    for invalid in [
        "",
        "kh_",
        "kh_🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂",
        "kh_11111111-1111-4111-8111-111111111111_wrong",
    ] {
        assert!(auth.authenticate(invalid, "client").is_err());
    }
    let login = auth.login(&created.token).unwrap();
    let (cookie, csrf) = auth.cookie(&login.id, "browser").unwrap();
    assert_eq!(csrf, login.csrf);
    assert!(auth.valid_csrf(&cookie, &csrf).unwrap());
    assert!(!auth.valid_csrf(&cookie, "wrong").unwrap());
    let other_client = auth.cookie(&login.id, "other-browser").unwrap().0;
    assert_ne!(cookie.client_id, other_client.client_id);
    auth.logout(&login.id).unwrap();
    assert_eq!(
        auth.check(&cookie, None, None).err().unwrap().code,
        "AUTH_FAILED"
    );
    let login = auth.login(&created.token).unwrap();
    auth.revoke(&identity.token_id).await.unwrap();
    assert_eq!(
        auth.check(&identity, None, None).err().unwrap().code,
        "AUTH_FAILED"
    );
    assert!(auth.cookie(&login.id, "browser").is_err());
    assert!(auth.authenticate(&created.token, "client-a").is_err());
    let stored: Value = store
        .get(Kind::Tokens, &identity.token_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored["revoked"], true);
    assert_eq!(stored["revision"], 2);
    store.close().await.unwrap();
}

#[tokio::test]
async fn token_issuance_cannot_expand_scopes_devices_or_lifetime() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let auth = Auth::load(store.clone()).await.unwrap();
    let expiry = now_ms() + 60_000;
    let issuer = auth.create(json!({"name":"restricted","scopes":["tokens:manage","devices:read"],"devices":["allowed"],"expires_at":expiry}),None).await.unwrap();
    let identity = auth.authenticate(&issuer.token, "issuer").unwrap();
    for input in [
        json!({"name":"scope","scopes":["input:write"],"devices":["allowed"],"expires_at":expiry}),
        json!({"name":"device","scopes":["devices:read"],"devices":["*"] ,"expires_at":expiry}),
        json!({"name":"lifetime","scopes":["devices:read"],"devices":["allowed"]}),
        json!({"name":"later","scopes":["devices:read"],"devices":["allowed"],"expires_at":expiry+1}),
    ] {
        assert_eq!(
            auth.create(input, Some(&identity))
                .await
                .err()
                .unwrap()
                .code,
            "FORBIDDEN"
        );
    }
    let child = auth.create(json!({"name":"child","scopes":["devices:read"],"devices":["allowed"],"expires_at":expiry-1}),Some(&identity)).await.unwrap();
    let child_id = auth.authenticate(&child.token, "child").unwrap();
    assert!(
        auth.check(&child_id, Some("devices:read"), Some("allowed"))
            .is_ok()
    );
    assert_eq!(
        auth.check(&child_id, Some("devices:read"), Some("other"))
            .err()
            .unwrap()
            .code,
        "FORBIDDEN"
    );
    assert_eq!(
        auth.check(&child_id, Some("video:read"), Some("allowed"))
            .err()
            .unwrap()
            .code,
        "FORBIDDEN"
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn expired_tokens_stay_unusable_and_ui_sessions_do_not_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let auth = Auth::load(store.clone()).await.unwrap();
    let expired = auth
        .create(
            json!({"name":"expired","scopes":["devices:read"],"devices":["*"],"expires_at":0}),
            None,
        )
        .await
        .unwrap();
    assert!(auth.authenticate(&expired.token, "client").is_err());
    let live = auth
        .create(
            json!({"name":"live","scopes":["devices:read"],"devices":["*"]}),
            None,
        )
        .await
        .unwrap();
    let login = auth.login(&live.token).unwrap();
    let auth = Auth::load(store.clone()).await.unwrap();
    assert!(auth.cookie(&login.id, "client").is_err());
    assert!(auth.authenticate(&live.token, "client").is_ok());
    let mut stored: Value = store
        .get(Kind::Tokens, live.metadata["id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    stored["hash"] = stored["hash"].as_str().unwrap().to_uppercase().into();
    store
        .put(
            Kind::Tokens,
            live.metadata["id"].as_str().unwrap(),
            stored,
            1,
        )
        .await
        .unwrap();
    let auth = Auth::load(store.clone()).await.unwrap();
    assert!(
        auth.authenticate(&live.token, "client").is_ok(),
        "hex hash case is immaterial in the existing format"
    );
    store.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn posix_secret_permissions_are_private_and_symlinks_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let secrets = Secrets::open(dir.path().join("secrets")).await.unwrap();
    secrets.set("key", json!({"fixture":true})).await.unwrap();
    assert_eq!(
        fs::metadata(dir.path().join("secrets"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for path in [
        dir.path().join("secrets/master.key"),
        secrets.path("key").unwrap(),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    symlink(
        secrets.path("key").unwrap(),
        secrets.path("linked").unwrap(),
    )
    .unwrap();
    assert!(secrets.get(Some("linked")).await.is_err());
}
