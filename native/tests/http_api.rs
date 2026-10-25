use kvmhelm::{
    auth::{Auth, bearer_client},
    contracts::SCOPES,
    core::{Core, Limits},
    secrets::Secrets,
    server::{Options, Server},
    store::{Kind, Store},
};
use reqwest::{Client, Method, Response};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::io::AsyncWriteExt;

struct Fixture {
    server: Option<Server>,
    core: Arc<Core>,
    store: Store,
    token: String,
    id: String,
    base: String,
    http: Client,
    dir: tempfile::TempDir,
}
impl Fixture {
    async fn new() -> Self {
        Self::limited(Options {
            port: 0,
            ..Options::default()
        })
        .await
    }
    async fn limited(options: Options) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).await.unwrap();
        let auth = Arc::new(Auth::load(store.clone()).await.unwrap());
        let token = auth
            .create(
                json!({"name":"HTTP fixture","scopes":SCOPES,"devices":["*"]}),
                None,
            )
            .await
            .unwrap()
            .token
            .to_string();
        let owner = auth.authenticate(&token, "fixture").unwrap();
        let core = Core::new(
            store.clone(),
            Secrets::open(dir.path().join("secrets")).await.unwrap(),
            auth,
            Limits::default(),
            120000,
        )
        .unwrap();
        core.load().await.unwrap();
        let device=core.save_device(&owner,json!({"name":"HTTP simulator","driver_id":"simulator","transport":"simulator","media_profile":"warm","minimum_action_latency_ms":0}),None,None).await.unwrap();
        let server = Server::bind(core.clone(), options, &[]).await.unwrap();
        let base = format!("http://{}", server.address);
        let http = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        Self {
            server: Some(server),
            core,
            store,
            token,
            id: device["device_id"].as_str().unwrap().into(),
            base,
            http,
            dir,
        }
    }
    async fn request(&self, path: &str, method: Method, body: Option<Value>) -> Response {
        self.with_token(path, method, body, &self.token).await
    }
    async fn with_token(
        &self,
        path: &str,
        method: Method,
        body: Option<Value>,
        token: &str,
    ) -> Response {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token)
            .header("x-kvmhelm-client", "http");
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        request.send().await.unwrap()
    }
    async fn tool(&self, name: &str, body: Value) -> Value {
        let response = self
            .request(&format!("/api/v1/tools/{name}"), Method::POST, Some(body))
            .await;
        assert_eq!(response.status(), 200);
        response.json().await.unwrap()
    }
    async fn open(&self) -> Value {
        self.tool(
            "open_computer",
            json!({"computer_id":self.id,"mode":"control","request_id":"open"}),
        )
        .await
    }
    async fn close(mut self) {
        if let Some(server) = self.server.take() {
            server.close().await;
        }
        self.core.shutdown().await;
        let _ = self.store.close().await;
    }
}
async fn until(condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn native_http_tools_share_sessions_images_and_idempotent_input() {
    let f = Fixture::new().await;
    let opened = f.open().await;
    assert_eq!(opened["isError"], false);
    assert_eq!(opened["content"][1]["type"], "image");
    assert_eq!(opened["content"][1]["mimeType"], "image/png");
    let data = &opened["structuredContent"];
    let session = data["session"]["session_id"].as_str().unwrap();
    let info = &data["frames"][0];
    let input = json!({"session_id":session,"request_id":"click","reference":{"frame_id":info["frame_id"],"view_id":info["view_id"],"input_revision":info["input_revision"]},"actions":[{"type":"click","x":10,"y":10}],"observation":{"mode":"none"}});
    let result = f.tool("computer", input.clone()).await;
    assert_eq!(result["isError"], false);
    assert_eq!(result["structuredContent"]["actions"][0]["status"], "sent");
    assert_eq!(f.tool("computer", input).await, result);
    let frame = f
        .request(
            &format!("/api/v1/sessions/{session}/frame"),
            Method::GET,
            None,
        )
        .await;
    assert_eq!(frame.status(), 200);
    let metadata: Value =
        serde_json::from_str(frame.headers()["x-kvmhelm-frame"].to_str().unwrap()).unwrap();
    assert_eq!(
        metadata["input_revision"].as_i64().unwrap(),
        info["input_revision"].as_i64().unwrap() + 1
    );
    assert_eq!(frame.headers()["content-type"], "image/png");
    assert!(frame.bytes().await.unwrap().starts_with(b"\x89PNG"));
    let listed: Value = f
        .request("/api/v1/sessions", Method::GET, None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(
        f.tool("close_computer", json!({"session_id":session}))
            .await["isError"],
        false
    );
    assert_eq!(
        f.tool("computer_screenshot", json!({"session_id":session}))
            .await["structuredContent"]["error"]["code"],
        "SESSION_LOST"
    );
    f.close().await;
}
#[tokio::test]
async fn host_origin_csrf_and_cookie_identity_are_enforced() {
    let f = Fixture::new().await;
    for (name, value) in [
        ("host", "attacker.invalid"),
        ("origin", "http://attacker.invalid"),
    ] {
        let response = f
            .http
            .get(format!("{}/healthz", f.base))
            .header(name, value)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
    }
    let login = f
        .http
        .post(format!("{}/api/v1/login", f.base))
        .header("origin", &f.base)
        .header("content-type", "application/json")
        .body(json!({"pat":f.token}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 200);
    let cookie = login.headers()["set-cookie"].to_str().unwrap().to_owned();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    assert!(!cookie.contains("Secure"));
    let csrf = login.json::<Value>().await.unwrap()["csrf"]
        .as_str()
        .unwrap()
        .to_owned();
    let cookie = cookie.split(';').next().unwrap();
    let me = f
        .http
        .get(format!("{}/api/v1/me", f.base))
        .header("cookie", cookie)
        .header("x-kvmhelm-client", "browser")
        .send()
        .await
        .unwrap();
    assert_eq!(me.status(), 200);
    assert_eq!(me.json::<Value>().await.unwrap()["csrf"], csrf);
    for (origin, token, status) in [
        (None, None, 403),
        (Some(f.base.as_str()), Some("bad"), 403),
        (Some(f.base.as_str()), Some(csrf.as_str()), 200),
    ] {
        let mut request = f
            .http
            .post(format!("{}/api/v1/devices/{}/stop", f.base, f.id))
            .header("cookie", cookie)
            .header("x-kvmhelm-client", "browser");
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        if let Some(token) = token {
            request = request.header("x-csrf-token", token);
        }
        assert_eq!(request.send().await.unwrap().status(), status);
    }
    let logout = f
        .http
        .post(format!("{}/api/v1/logout", f.base))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .header("origin", &f.base)
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), 200);
    assert!(
        logout.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    assert_eq!(
        f.http
            .get(format!("{}/api/v1/me", f.base))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    f.close().await;
}
#[tokio::test]
async fn body_limits_strict_schemas_and_preferences_cas_are_preserved() {
    let f = Fixture::new().await;
    let response = f
        .http
        .post(format!("{}/api/v1/devices", f.base))
        .bearer_auth(&f.token)
        .header("content-type", "application/json")
        .body(" ".repeat(256 * 1024 + 1))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    let response = f
        .request(
            "/api/v1/devices",
            Method::POST,
            Some(json!({"device":{},"unexpected":"secret"})),
        )
        .await;
    assert_eq!(response.status(), 400);
    assert!(!response.text().await.unwrap().contains("secret"));
    let mut prefs: Value = f
        .request("/api/v1/preferences", Method::GET, None)
        .await
        .json()
        .await
        .unwrap();
    prefs["theme"] = "light".into();
    let saved: Value = f
        .request("/api/v1/preferences", Method::PUT, Some(prefs.clone()))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(saved["revision"], 1);
    assert_eq!(
        f.request("/api/v1/preferences", Method::PUT, Some(prefs))
            .await
            .status(),
        409
    );
    assert_eq!(
        f.request("/api/v1/tools/not_a_tool", Method::POST, Some(json!({})))
            .await
            .status(),
        404
    );
    f.close().await;
}
#[tokio::test]
async fn media_or_control_exhaustion_cannot_block_safety_health_or_a_second_token() {
    let f = Fixture::limited(Options {
        port: 0,
        media_per_minute: 2,
        control_per_minute: 8,
        ..Options::default()
    })
    .await;
    let opened = f.open().await;
    let session = opened["structuredContent"]["session"]["session_id"]
        .as_str()
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            f.request(
                &format!("/api/v1/sessions/{session}/frame"),
                Method::GET,
                None
            )
            .await
            .status(),
            200
        );
    }
    let limited = f
        .request(
            &format!("/api/v1/sessions/{session}/frame"),
            Method::GET,
            None,
        )
        .await;
    assert_eq!(limited.status(), 429);
    assert!(limited.headers().contains_key("retry-after"));
    for _ in 0..8 {
        let _ = f.request("/api/v1/devices", Method::GET, None).await;
    }
    assert_eq!(
        f.request("/api/v1/devices", Method::GET, None)
            .await
            .status(),
        429
    );
    assert_eq!(
        f.request(
            &format!("/api/v1/sessions/{session}/release-inputs"),
            Method::POST,
            Some(json!({}))
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        f.tool(
            "computer_control",
            json!({"session_id":session,"request_id":"release","operation":"release"})
        )
        .await["isError"],
        false
    );
    assert_eq!(
        f.request(
            &format!("/api/v1/devices/{}/stop", f.id),
            Method::POST,
            Some(json!({}))
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        f.tool("close_computer", json!({"session_id":session}))
            .await["isError"],
        false
    );
    let other = f
        .core
        .auth
        .create(
            json!({"name":"reader","scopes":["devices:read"],"devices":["*"]}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        f.with_token("/api/v1/devices", Method::GET, None, &other.token)
            .await
            .status(),
        200
    );
    assert_eq!(
        f.with_token(
            &format!("/api/v1/devices/{}/stop", f.id),
            Method::POST,
            Some(json!({})),
            &other.token
        )
        .await
        .status(),
        403
    );
    for path in ["/healthz", "/readyz"] {
        assert_eq!(
            f.http
                .get(format!("{}{path}", f.base))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    f.store.close().await.unwrap();
    assert_eq!(
        f.http
            .get(format!("{}/readyz", f.base))
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    assert_eq!(
        f.http
            .get(format!("{}/healthz", f.base))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    f.close().await;
}
#[tokio::test]
async fn accepted_token_revocation_finishes_after_the_http_socket_disappears() {
    let f = Fixture::new().await;
    let _ = f.open().await;
    assert!(f.core.get(&f.id).unwrap().lease().is_some());
    let identity = f
        .core
        .auth
        .authenticate(&f.token, &bearer_client(&f.token, "http"))
        .unwrap();
    let database = rusqlite::Connection::open(f.dir.path().join("data/config.sqlite")).unwrap();
    database.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut socket = tokio::net::TcpStream::connect(f.server.as_ref().unwrap().address)
        .await
        .unwrap();
    socket.write_all(format!("DELETE /api/v1/tokens/{} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",identity.token_id,f.server.as_ref().unwrap().address,f.token).as_bytes()).await.unwrap();
    until(|| f.store.pending_requests() > 0).await;
    drop(socket);
    database.execute_batch("COMMIT").unwrap();
    drop(database);
    until(|| f.core.auth.check(&identity, None, None).is_err()).await;
    until(|| f.core.get(&f.id).unwrap().lease().is_none()).await;
    assert_eq!(
        f.store
            .get::<Value>(Kind::Tokens, &identity.token_id)
            .await
            .unwrap()
            .unwrap()["revoked"],
        true
    );
    f.close().await;
}

#[tokio::test]
async fn sse_restarts_ignore_old_cursors_and_revocation_closes_the_live_socket() {
    let f = Fixture::new().await;
    let token = f
        .core
        .auth
        .create(
            json!({"name":"events","scopes":["devices:read"],"devices":["*"]}),
            None,
        )
        .await
        .unwrap();
    let mut response = f
        .http
        .get(format!("{}/api/v1/events", f.base))
        .bearer_auth(token.token.as_str())
        .header("last-event-id", "999999")
        .header("x-kvmhelm-instance", "previous-daemon")
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["x-kvmhelm-instance"], f.core.instance_id);
    let first = response.chunk().await.unwrap().unwrap();
    let first = String::from_utf8_lossy(&first);
    assert!(first.contains("device.updated") && first.contains("historical"));
    let id = token.metadata["id"].as_str().unwrap();
    f.core.auth.revoke(id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Ok(Some(_)) = response.chunk().await {}
    })
    .await
    .unwrap();
    f.close().await;
}

#[tokio::test]
async fn video_subscribers_release_their_consumer_on_disconnect() {
    let f = Fixture::new().await;
    let opened = f.open().await;
    let id = opened["structuredContent"]["session"]["session_id"]
        .as_str()
        .unwrap();
    let media = f.core.get(&f.id).unwrap().media.clone();
    assert_eq!(media.consumer_count(), 1);
    let mut response = f
        .request(&format!("/api/v1/sessions/{id}/video"), Method::GET, None)
        .await;
    assert_eq!(
        response.headers()["content-type"],
        "multipart/x-mixed-replace; boundary=kvmhelm"
    );
    let chunk = response.chunk().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&chunk).contains("--kvmhelm"));
    assert_eq!(media.consumer_count(), 2);
    drop(response);
    until(|| media.consumer_count() == 1).await;
    f.close().await;
}

#[tokio::test]
async fn tls_and_plain_listeners_use_one_core_and_lan_plaintext_is_refused() {
    let f = Fixture::new().await;
    assert!(
        Server::bind(
            f.core.clone(),
            Options {
                host: "0.0.0.0".into(),
                port: 0,
                ..Options::default()
            },
            &[]
        )
        .await
        .is_err()
    );
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = f.dir.path().join("cert.pem");
    let key = f.dir.path().join("key.pem");
    std::fs::write(&cert, generated.cert.pem()).unwrap();
    std::fs::write(&key, generated.signing_key.serialize_pem()).unwrap();
    let tls = Server::bind(
        f.core.clone(),
        Options {
            port: 0,
            certificate: Some(cert),
            key: Some(key),
            ..Options::default()
        },
        &[],
    )
    .await
    .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(generated.cert.der().clone()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let client = Client::builder()
        .no_proxy()
        .tls_backend_preconfigured(config)
        .build()
        .unwrap();
    let base = format!("https://localhost:{}", tls.address.port());
    let login = client
        .post(format!("{base}/api/v1/login"))
        .header("origin", &base)
        .header("content-type", "application/json")
        .body(json!({"pat":f.token}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 200);
    assert!(
        login.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("; Secure")
    );
    f.open().await;
    let other: Value = client
        .post(format!("{base}/api/v1/tools/open_computer"))
        .bearer_auth(&f.token)
        .header("x-kvmhelm-client", "tls")
        .header("content-type", "application/json")
        .body(json!({"computer_id":f.id,"mode":"control","request_id":"other"}).to_string())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(other["structuredContent"]["error"]["code"], "CONTROL_BUSY");
    tls.close().await;
    assert_eq!(
        f.http
            .get(format!("{}/healthz", f.base))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    f.close().await;
}

#[tokio::test]
async fn token_scopes_and_device_configurations_reject_privilege_expansion_and_stale_revisions() {
    let f = Fixture::new().await;
    let created: Value = f
        .request(
            "/api/v1/tokens",
            Method::POST,
            Some(
                json!({"name":"reader","scopes":["tokens:manage","devices:read"],"devices":[f.id]}),
            ),
        )
        .await
        .json()
        .await
        .unwrap();
    let token = created["token"].as_str().unwrap();
    assert_eq!(
        f.with_token(
            "/api/v1/tokens",
            Method::POST,
            Some(json!({"name":"expanded","scopes":["input:write"],"devices":["*"]})),
            token
        )
        .await
        .status(),
        403
    );
    let devices: Value = f
        .with_token("/api/v1/devices", Method::GET, None, token)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(devices.as_array().unwrap().len(), 1);
    let original = serde_json::to_value(f.core.get(&f.id).unwrap().device.clone()).unwrap();
    let update=f.request("/api/v1/devices",Method::POST,Some(json!({"device":original,"secret":{"username":"fixture","password":"fixture-only"}}))).await;
    assert_eq!(update.status(), 200);
    assert_eq!(
        f.request(
            "/api/v1/devices",
            Method::POST,
            Some(json!({"device":original}))
        )
        .await
        .status(),
        409
    );
    assert_eq!(
        f.with_token(
            "/api/v1/devices",
            Method::POST,
            Some(json!({"device":original})),
            token
        )
        .await
        .status(),
        403
    );
    f.close().await;
}

#[tokio::test]
async fn configured_plugins_are_reported_as_unavailable_instead_of_an_empty_runtime() {
    let f = Fixture::new().await;
    for path in [
        "/api/v1/plugins",
        "/api/v1/ui-extensions",
        "/api/v1/diagnostics",
    ] {
        assert_eq!(f.request(path, Method::GET, None).await.status(), 200);
    }
    f.store
        .put(
            Kind::Plugins,
            "fixture",
            json!({"id":"fixture","revision":0}),
            0,
        )
        .await
        .unwrap();
    for path in [
        "/api/v1/plugins",
        "/api/v1/ui-extensions",
        "/api/v1/diagnostics",
    ] {
        let response: Value = f
            .request(path, Method::GET, None)
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], "UNSUPPORTED_RUNTIME");
    }
    f.close().await;
}

#[cfg(feature = "embedded-web")]
struct NativeCli {
    executable: std::path::PathBuf,
    dir: tempfile::TempDir,
}
#[cfg(feature = "embedded-web")]
impl NativeCli {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join(if cfg!(windows) {
            "kvmhelm.exe"
        } else {
            "kvmhelm"
        });
        std::fs::copy(env!("CARGO_BIN_EXE_kvmhelm"), &executable).unwrap();
        Self { executable, dir }
    }
    async fn run(&self, args: Vec<String>) -> std::process::Output {
        let mut command = std::process::Command::new(&self.executable);
        let environment = std::env::vars_os().filter(|(key, _)| {
            !key.to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("KVMHELM_")
        });
        command
            .env_clear()
            .envs(environment)
            .env("PATH", "")
            .current_dir(self.dir.path())
            .args(args);
        tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap()
    }
}

#[cfg(feature = "embedded-web")]
#[tokio::test]
async fn copied_cli_bootstrap_needs_no_node_and_never_overwrites_an_existing_pat() {
    let cli = NativeCli::new();
    let data = cli.dir.path().join("data");
    let secrets = cli.dir.path().join("secrets");
    let args = vec![
        "token".into(),
        "bootstrap".into(),
        "--data".into(),
        data.to_string_lossy().into(),
        "--secrets".into(),
        secrets.to_string_lossy().into(),
    ];
    let first = cli.run(args.clone()).await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let pat = std::fs::read_to_string(secrets.join("cli.pat")).unwrap();
    assert!(!String::from_utf8_lossy(&first.stdout).contains(pat.trim()));
    assert!(!String::from_utf8_lossy(&first.stderr).contains(pat.trim()));
    assert!(!cli.run(args).await.status.success());
    assert_eq!(
        std::fs::read_to_string(secrets.join("cli.pat")).unwrap(),
        pat
    );
    let store = Store::open(data).await.unwrap();
    let auth = Auth::load(store.clone()).await.unwrap();
    assert!(auth.authenticate(pat.trim(), "fixture").is_ok());
    assert_eq!(store.list::<Value>(Kind::Tokens).await.unwrap().len(), 1);
    assert!(store.list::<Value>(Kind::Devices).await.unwrap().is_empty());
    store.close().await.unwrap();
}

#[cfg(feature = "embedded-web")]
#[tokio::test]
async fn copied_cli_uses_the_running_daemon_and_reserves_token_output_before_issuance() {
    let f = Fixture::new().await;
    let cli = NativeCli::new();
    let pat_file = cli.dir.path().join("owner.pat");
    std::fs::write(&pat_file, &f.token).unwrap();
    let blocked_data = cli.dir.path().join("this-is-a-file");
    std::fs::write(&blocked_data, "Remote administration must not open a Core").unwrap();
    let common = vec![
        "--url".into(),
        f.base.clone(),
        "--pat-file".into(),
        pat_file.to_string_lossy().into(),
        "--data".into(),
        blocked_data.to_string_lossy().into(),
    ];
    let command = |parts: &[&str]| {
        parts
            .iter()
            .map(|s| (*s).into())
            .chain(common.clone())
            .collect::<Vec<String>>()
    };
    let doctor = cli.run(command(&["doctor"])).await;
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let diagnostics: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(diagnostics["health"]["ok"], true);
    assert_eq!(diagnostics["diagnostics"]["devices"][0]["device_id"], f.id);
    #[cfg(any(windows, target_os = "linux"))]
    assert!(
        diagnostics["diagnostics"]["memory"]["rss"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(diagnostics["diagnostics"]["cpu_time_ms"].as_f64().unwrap() >= 0.0);
    let output = cli.dir.path().join("reader.pat");
    let mut args: Vec<String> = [
        "token",
        "create",
        "--name",
        "CLI reader",
        "--scope",
        "devices:read",
        "--out",
    ]
    .map(String::from)
    .into();
    args.push(output.to_string_lossy().into());
    args.extend(common.clone());
    let created = cli.run(args.clone()).await;
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let token = std::fs::read_to_string(&output).unwrap();
    assert!(f.core.auth.authenticate(token.trim(), "fixture").is_ok());
    let public: Value = serde_json::from_slice(&created.stdout).unwrap();
    assert!(public.get("token").is_none());
    assert!(!String::from_utf8_lossy(&created.stdout).contains(token.trim()));
    let listed = cli.run(command(&["token", "list"])).await;
    assert!(listed.status.success());
    let tokens: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(tokens[0]["name"], "HTTP fixture");
    assert_eq!(tokens[1]["name"], "CLI reader");
    assert!(!cli.run(args).await.status.success());
    assert_eq!(std::fs::read_to_string(output).unwrap(), token);
    assert_eq!(f.store.list::<Value>(Kind::Tokens).await.unwrap().len(), 2);
    f.close().await;
}
