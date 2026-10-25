//! Audio authorization and lifetime tests use only synthetic PCM and a native
//! in-memory driver. No browser capture device or physical KVM is accessed.
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use kvmhelm::{
    Result,
    audio::Source,
    auth::Auth,
    contracts::{Action, Capabilities, Layout, SCOPES},
    core::{Core, Limits},
    drivers::{ActionContext, AudioStream, Delivery, DriverEvent, KvmDriver, simulator::Simulator},
    media::DriverFrame,
    secrets::Secrets,
    server::{Options, Server},
    store::Store,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, broadcast};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};
use tokio_util::sync::CancellationToken;
type Socket = WebSocketStream<tokio::net::TcpStream>;

struct AudioDriver {
    simulator: Simulator,
    source: Arc<Source>,
    activations: Mutex<Vec<bool>>,
    packets: Mutex<Vec<Arc<[u8]>>>,
    delayed: std::sync::atomic::AtomicBool,
    entered: AtomicUsize,
    gate: Notify,
}
#[async_trait]
impl KvmDriver for AudioDriver {
    fn events(&self) -> broadcast::Receiver<DriverEvent> {
        self.simulator.events()
    }
    async fn connect(&self, c: CancellationToken) -> Result<()> {
        self.simulator.connect(c).await
    }
    async fn disconnect(&self) -> Result<()> {
        self.simulator.disconnect().await
    }
    async fn capabilities(&self) -> Result<Capabilities> {
        let mut caps = self.simulator.capabilities().await?;
        caps.audio.from_target = true;
        caps.audio.to_target = true;
        caps.audio.formats = vec!["pcm_s16le/48000/2".into()];
        Ok(caps)
    }
    async fn snapshot(&self, c: CancellationToken) -> Result<DriverFrame> {
        self.simulator.snapshot(c).await
    }
    fn validate(&self, a: &[Action], w: u32, h: u32) -> Result<()> {
        self.simulator.validate(a, w, h)
    }
    async fn execute(&self, a: &Action, c: &ActionContext) -> Result<Delivery> {
        self.simulator.execute(a, c).await
    }
    async fn release_all_inputs(&self) -> Result<()> {
        self.simulator.release_all_inputs().await
    }
    async fn subscribe_audio(&self, c: CancellationToken) -> Result<AudioStream> {
        self.source.subscribe(c)
    }
    async fn set_microphone(&self, enabled: bool, context: Option<ActionContext>) -> Result<()> {
        if enabled {
            self.entered.fetch_add(1, Ordering::AcqRel);
            if self.delayed.load(Ordering::Acquire) {
                self.gate.notified().await;
            }
            context.as_ref().unwrap().check()?;
        }
        self.activations.lock().unwrap().push(enabled);
        Ok(())
    }
    async fn send_audio(&self, data: Arc<[u8]>, context: &ActionContext) -> Result<()> {
        context.check()?;
        self.packets.lock().unwrap().push(data);
        Ok(())
    }
}
struct Fixture {
    core: Arc<Core>,
    store: Store,
    servers: Vec<Server>,
    driver: Arc<AudioDriver>,
    token: String,
    id: String,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}
impl Fixture {
    async fn new() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).await.unwrap();
        let auth = Arc::new(Auth::load(store.clone()).await.unwrap());
        let token = auth
            .create(
                json!({"name":"audio fixture","scopes":SCOPES,"devices":["*"]}),
                None,
            )
            .await
            .unwrap()
            .token
            .to_string();
        let identity = auth.authenticate(&token, "audio").unwrap();
        let core = Core::new(
            store.clone(),
            Secrets::open(dir.path().join("secrets")).await.unwrap(),
            auth,
            Limits::default(),
            120000,
        )
        .unwrap();
        let driver = Arc::new(AudioDriver {
            simulator: Simulator::new(Layout::Us),
            source: Source::new("fixture".into(), 2, 0),
            activations: Mutex::new(vec![]),
            packets: Mutex::new(vec![]),
            delayed: false.into(),
            entered: AtomicUsize::new(0),
            gate: Notify::new(),
        });
        let adapter = driver.clone();
        core.register_factory(
            "audio-fixture",
            Arc::new(move |_, _, _| Ok(adapter.clone())),
        );
        core.load().await.unwrap();
        let device=core.save_device(&identity,json!({"name":"Synthetic audio","driver_id":"audio-fixture","transport":"simulator","media_profile":"warm","minimum_action_latency_ms":0}),None,None).await.unwrap();
        let server = Server::bind(
            core.clone(),
            Options {
                port: 0,
                ..Options::default()
            },
            &[],
        )
        .await
        .unwrap();
        Self {
            core,
            store,
            servers: vec![server],
            driver,
            token,
            id: device["device_id"].as_str().unwrap().into(),
            http: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            _dir: dir,
        }
    }
    fn base(&self, index: usize) -> String {
        format!("http://{}", self.servers[index].address)
    }
    async fn request(
        &self,
        index: usize,
        path: &str,
        body: Value,
        token: &str,
    ) -> reqwest::Response {
        self.http
            .post(format!("{}/api/v1{path}", self.base(index)))
            .bearer_auth(token)
            .header("x-kvmhelm-client", "audio")
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    async fn open(&self, token: &str, mode: &str) -> String {
        let response=self.request(0,"/tools/open_computer",json!({"computer_id":self.id,"mode":mode,"request_id":uuid::Uuid::new_v4().to_string()}),token).await;
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["isError"], false, "{body}");
        body["structuredContent"]["session"]["session_id"]
            .as_str()
            .unwrap()
            .into()
    }
    async fn ticket(&self, index: usize, sid: &str, microphone: bool, token: &str) -> String {
        let response = self
            .request(
                index,
                &format!("/sessions/{sid}/audio"),
                json!({"microphone":microphone}),
                token,
            )
            .await;
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, 200, "{body}");
        body["ticket"].as_str().unwrap().into()
    }
    async fn socket(&self, index: usize, ticket: &str, cookie: Option<&str>) -> Socket {
        let mut request = format!("{}/api/v1/audio", self.base(index).replace("http:", "ws:"))
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("origin", self.base(index).parse().unwrap());
        if let Some(cookie) = cookie {
            request
                .headers_mut()
                .insert("cookie", cookie.parse().unwrap());
        }
        let tcp = tokio::net::TcpStream::connect(self.servers[index].address)
            .await
            .unwrap();
        let (mut ws, _) = tokio_tungstenite::client_async(request, tcp).await.unwrap();
        ws.send(Message::Text(json!({"ticket":ticket}).to_string().into()))
            .await
            .unwrap();
        ws
    }
    async fn connect(&self, index: usize, sid: &str, mic: bool) -> Socket {
        let ticket = self.ticket(index, sid, mic, &self.token).await;
        let mut ws = self.socket(index, &ticket, None).await;
        ready(&mut ws).await;
        ws
    }
    async fn close(self) {
        for server in self.servers {
            server.close().await;
        }
        self.core.shutdown().await;
        self.store.close().await.unwrap();
    }
}
async fn next(ws: &mut Socket) -> Message {
    tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}
async fn ready(ws: &mut Socket) {
    let value = next(ws).await;
    assert_eq!(value, Message::Text("{\"ready\":true}".into()));
}
async fn closed(ws: &mut Socket) -> u16 {
    loop {
        if let Message::Close(frame) = next(ws).await {
            return frame.unwrap().code.into();
        }
    }
}
async fn until(condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn shared_audio_and_one_microphone_across_listeners_release_every_consumer() {
    let mut f = Fixture::new().await;
    f.servers.push(
        Server::bind(
            f.core.clone(),
            Options {
                port: 0,
                ..Options::default()
            },
            &[],
        )
        .await
        .unwrap(),
    );
    let sid = f.open(&f.token, "control").await;
    let before = f.core.get(&f.id).unwrap().media.consumer_count();
    let mut first = f.connect(0, &sid, true).await;
    let mut listener = f.connect(1, &sid, false).await;
    assert_eq!(f.driver.source.users(), 2);
    assert_eq!(
        f.core.get(&f.id).unwrap().media.consumer_count(),
        before + 2
    );
    let ticket = f.ticket(1, &sid, true, &f.token).await;
    let mut second = f.socket(1, &ticket, None).await;
    assert_eq!(closed(&mut second).await, 1008);
    assert_eq!(*f.driver.activations.lock().unwrap(), [true]);
    first
        .send(Message::Binary(vec![1, 0, 2, 0].into()))
        .await
        .unwrap();
    until(|| f.driver.packets.lock().unwrap().len() == 1).await;
    assert_eq!(&*f.driver.packets.lock().unwrap()[0], &[1, 0, 2, 0]);
    f.driver.source.publish(vec![3, 0, 4, 0].into(), 1, true);
    for socket in [&mut first, &mut listener] {
        assert_eq!(next(socket).await, Message::Text("{\"gap\":true}".into()));
        assert_eq!(next(socket).await, Message::Binary(vec![3, 0, 4, 0].into()));
    }
    first.close(None).await.unwrap();
    until(|| f.driver.activations.lock().unwrap().last() == Some(&false)).await;
    assert_eq!(f.driver.source.users(), 1);
    f.driver.source.publish(vec![5, 0, 6, 0].into(), 1, false);
    assert_eq!(
        next(&mut listener).await,
        Message::Binary(vec![5, 0, 6, 0].into())
    );
    listener.close(None).await.unwrap();
    until(|| f.driver.source.users() == 0).await;
    until(|| f.core.get(&f.id).unwrap().media.consumer_count() == before).await;
    f.close().await;
}

#[tokio::test]
async fn audio_tickets_require_direction_scopes_control_origin_and_single_use() {
    let f = Fixture::new().await;
    let token=f.core.auth.create(json!({"name":"listener","scopes":["devices:read","video:read","audio:read"],"devices":[f.id]}),None).await.unwrap().token.to_string();
    let sid = f.open(&token, "observe").await;
    let denied = f
        .request(
            0,
            &format!("/sessions/{sid}/audio"),
            json!({"microphone":true}),
            &token,
        )
        .await;
    assert_eq!(denied.status(), 403);
    for body in [json!({}), json!({"microphone":false,"unknown":true})] {
        assert_eq!(
            f.request(0, &format!("/sessions/{sid}/audio"), body, &token)
                .await
                .status(),
            400
        );
    }
    let ticket = f.ticket(0, &sid, false, &token).await;
    for origin in [None, Some("http://untrusted.invalid")] {
        let mut req = format!("{}/api/v1/audio", f.base(0).replace("http:", "ws:"))
            .into_client_request()
            .unwrap();
        if let Some(origin) = origin {
            req.headers_mut().insert("origin", origin.parse().unwrap());
        }
        let tcp = tokio::net::TcpStream::connect(f.servers[0].address)
            .await
            .unwrap();
        assert!(tokio_tungstenite::client_async(req, tcp).await.is_err());
    }
    let mut listener = f.socket(0, &ticket, None).await;
    ready(&mut listener).await;
    let mut replay = f.socket(0, &ticket, None).await;
    assert_eq!(closed(&mut replay).await, 1008);
    listener
        .send(Message::Binary(vec![0; 4].into()))
        .await
        .unwrap();
    assert_eq!(closed(&mut listener).await, 1008);
    assert!(f.driver.packets.lock().unwrap().is_empty());
    let no_audio = f
        .core
        .auth
        .create(
            json!({"name":"no audio","scopes":["devices:read","video:read"],"devices":[f.id]}),
            None,
        )
        .await
        .unwrap()
        .token
        .to_string();
    let sid = f.open(&no_audio, "observe").await;
    assert_eq!(
        f.request(
            0,
            &format!("/sessions/{sid}/audio"),
            json!({"microphone":false}),
            &no_audio
        )
        .await
        .status(),
        403
    );
    f.close().await;
}

#[tokio::test]
async fn release_and_revoke_close_audio_even_when_the_source_is_idle() {
    let f = Fixture::new().await;
    let sid = f.open(&f.token, "control").await;
    let mut mic = f.connect(0, &sid, true).await;
    let identity = f.core.auth.authenticate(&f.token, "http:audio").unwrap();
    let release = f
        .request(
            0,
            "/tools/computer_control",
            json!({"session_id":sid,"operation":"release","request_id":"release"}),
            &f.token,
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(release["isError"], false, "{release}");
    assert_eq!(closed(&mut mic).await, 1008);
    until(|| f.driver.source.users() == 0).await;
    assert_eq!(f.driver.activations.lock().unwrap().last(), Some(&false));
    let mut listen = f.connect(0, &sid, false).await;
    f.core.auth.revoke(&identity.token_id).await.unwrap();
    assert_eq!(closed(&mut listen).await, 1008);
    until(|| f.driver.source.users() == 0).await;
    f.close().await;
}

#[tokio::test]
async fn a_late_microphone_activation_after_socket_eof_is_cancelled_before_enable() {
    let f = Fixture::new().await;
    let sid = f.open(&f.token, "control").await;
    f.driver.delayed.store(true, Ordering::Release);
    let ticket = f.ticket(0, &sid, true, &f.token).await;
    let mut ws = f.socket(0, &ticket, None).await;
    until(|| f.driver.entered.load(Ordering::Acquire) > 0).await;
    ws.close(None).await.unwrap();
    drop(ws);
    tokio::time::sleep(Duration::from_millis(25)).await;
    f.driver.gate.notify_one();
    until(|| !f.driver.activations.lock().unwrap().is_empty()).await;
    assert_eq!(*f.driver.activations.lock().unwrap(), [false]);
    f.driver.delayed.store(false, Ordering::Release);
    let mut replacement = f.connect(0, &sid, true).await;
    replacement.close(None).await.unwrap();
    until(|| f.driver.activations.lock().unwrap().len() == 3).await;
    f.close().await;
}

#[tokio::test]
async fn source_failure_and_microphone_rate_and_frame_limits_close_and_clean_up() {
    let f = Fixture::new().await;
    let sid = f.open(&f.token, "control").await;
    let mut listen = f.connect(0, &sid, false).await;
    f.driver.source.fault("AUDIO_DISCONNECTED");
    assert_eq!(closed(&mut listen).await, 1011);
    until(|| f.driver.source.users() == 0).await;
    let mut mic = f.connect(0, &sid, true).await;
    mic.send(Message::Binary(vec![0; 16385].into()))
        .await
        .unwrap();
    // Tungstenite rejects oversized frames before exposing them to the driver.
    let _ = tokio::time::timeout(Duration::from_secs(5), mic.next())
        .await
        .unwrap();
    until(|| f.driver.activations.lock().unwrap().last() == Some(&false)).await;
    assert!(f.driver.packets.lock().unwrap().is_empty());
    let mut mic = f.connect(0, &sid, true).await;
    for _ in 0..9 {
        let _ = mic.send(Message::Binary(vec![0; 16000].into())).await;
    }
    assert_eq!(closed(&mut mic).await, 1008);
    assert!(f.driver.packets.lock().unwrap().len() <= 8);
    until(|| f.driver.source.users() == 0).await;
    f.close().await;
}

#[tokio::test]
async fn ui_audio_ticket_requires_its_cookie_and_logout_invalidates_an_idle_stream() {
    let f = Fixture::new().await;
    let login = f.core.auth.login(&f.token).unwrap();
    let cookie = format!("kvmhelm={}", login.id);
    let (identity, _) = f.core.auth.cookie(&login.id, "browser").unwrap();
    let opened = f
        .core
        .tool(
            &identity,
            "open_computer",
            json!({"computer_id":f.id,"mode":"observe"}),
        )
        .await;
    assert!(opened.success(), "{:?}", opened.body);
    let sid = opened.body["session"]["session_id"].as_str().unwrap();
    let issue = || async {
        let response = f
            .http
            .post(format!("{}/api/v1/sessions/{sid}/audio", f.base(0)))
            .header("cookie", &cookie)
            .header("origin", f.base(0))
            .header("x-csrf-token", &login.csrf)
            .header("x-kvmhelm-client", "browser")
            .json(&json!({"microphone":false}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.json::<Value>().await.unwrap()["ticket"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let ticket = issue().await;
    let mut wrong = f.socket(0, &ticket, None).await;
    assert_eq!(closed(&mut wrong).await, 1008);
    let mut used = f.socket(0, &ticket, Some(&cookie)).await;
    assert_eq!(closed(&mut used).await, 1008);
    let ticket = issue().await;
    let mut valid = f.socket(0, &ticket, Some(&cookie)).await;
    ready(&mut valid).await;
    f.core.auth.logout(&login.id).unwrap();
    assert_eq!(closed(&mut valid).await, 1008);
    until(|| f.driver.source.users() == 0).await;
    f.close().await;
}

#[tokio::test]
async fn a_new_control_generation_cannot_reauthorize_an_existing_microphone_socket() {
    let f = Fixture::new().await;
    let sid = f.open(&f.token, "control").await;
    let mut mic = f.connect(0, &sid, true).await;
    for (operation, request) in [("release", "release"), ("acquire", "acquire")] {
        let response = f
            .request(
                0,
                "/tools/computer_control",
                json!({"session_id":sid,"operation":operation,"request_id":request}),
                &f.token,
            )
            .await
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(response["isError"], false, "{response}");
    }
    let _ = mic.send(Message::Binary(vec![1; 1920].into())).await;
    assert_eq!(closed(&mut mic).await, 1008);
    assert!(f.driver.packets.lock().unwrap().is_empty());
    until(|| f.driver.source.users() == 0).await;
    f.close().await;
}
