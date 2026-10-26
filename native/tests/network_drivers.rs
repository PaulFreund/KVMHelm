use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt, stream};
use http_body_util::{BodyExt, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Request, Response, StatusCode,
    body::{Frame, Incoming},
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use kvmhelm::{
    GatewayError, Result,
    contracts::{Action, Device, Layout, Signal, Tls, Validate},
    drivers::{
        ActionContext, DriverEvent, KvmDriver,
        frames::{self, Mjpeg},
        network_hid::{Hid, HidTransport},
        pikvm::{CometDriver, PiKvmDriver},
    },
    transport::{AddressResolver, LanTransport},
};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
    sync::{Notify, broadcast},
    task::JoinHandle,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, handshake::derive_accept_key, protocol::Role},
};
use tokio_util::sync::CancellationToken;

type Body = UnsyncBoxBody<Bytes, Infallible>;
fn full(value: impl Into<Bytes>) -> Body {
    Full::new(value.into()).boxed_unsync()
}
fn jpeg() -> Vec<u8> {
    let mut data = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut data)
        .encode(
            &vec![127u8; 640 * 480 * 3],
            640,
            480,
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    data
}
#[derive(Clone, Debug)]
enum Command {
    Message(Value),
    Close,
}
struct State {
    requests: Mutex<Vec<(String, HeaderMap)>>,
    events: Mutex<Vec<Value>>,
    changed: Notify,
    commands: broadcast::Sender<Command>,
    pongs: AtomicBool,
    sockets: AtomicUsize,
    snapshot_calls: AtomicUsize,
    warmup: AtomicUsize,
    source_offline: AtomicBool,
    hid_online: AtomicBool,
    status: AtomicUsize,
    auth_status: AtomicUsize,
    body: Mutex<Vec<u8>>,
    media_length: AtomicUsize,
    stream_parts: Mutex<Vec<Vec<u8>>>,
    cancel: CancellationToken,
    tls: Mutex<Option<Arc<rustls::ServerConfig>>>,
}
struct Fixture {
    address: String,
    state: Arc<State>,
    worker: JoinHandle<()>,
    certificate: Option<rcgen::Certificate>,
}
fn certificate(name: &str) -> (rcgen::Certificate, Arc<rustls::ServerConfig>) {
    let generated = rcgen::generate_simple_self_signed(vec![name.into()]).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![generated.cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der()).into(),
    )
    .unwrap();
    (generated.cert, Arc::new(config))
}
impl Fixture {
    async fn new(tls: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (certificate, config) = if tls {
            let (cert, config) = certificate("localhost");
            (Some(cert), Some(config))
        } else {
            (None, None)
        };
        let (commands, _) = broadcast::channel(32);
        let image = jpeg();
        let state = Arc::new(State {
            requests: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            changed: Notify::new(),
            commands,
            pongs: AtomicBool::new(true),
            sockets: AtomicUsize::new(0),
            snapshot_calls: AtomicUsize::new(0),
            warmup: AtomicUsize::new(0),
            source_offline: AtomicBool::new(false),
            hid_online: AtomicBool::new(true),
            status: AtomicUsize::new(200),
            auth_status: AtomicUsize::new(200),
            body: Mutex::new(image.clone()),
            media_length: AtomicUsize::new(0),
            stream_parts: Mutex::new(vec![
                b"--fixture\r\nContent-Type: image/jpeg\r\n\r\n".to_vec(),
                image,
                b"\r\n--fixture\r\n".to_vec(),
            ]),
            cancel: CancellationToken::new(),
            tls: Mutex::new(config),
        });
        let server = state.clone();
        let worker = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {biased;
                    _=server.cancel.cancelled()=>break,
                    accepted=listener.accept()=>{
                        let (tcp,_) = accepted.unwrap(); let state=server.clone();
                        let tls=state.tls.lock().unwrap().clone();
                        connections.spawn(async move {
                            if let Some(tls)=tls {
                                if let Ok(tcp)=tokio_rustls::TlsAcceptor::from(tls).accept(tcp).await { serve(tcp,state).await; }
                            } else { serve(tcp,state).await; }
                        });
                    },
                    _=connections.join_next(), if !connections.is_empty()=>(),
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        Self {
            address: format!(
                "{}://{}:{}",
                if tls { "https" } else { "http" },
                if tls { "localhost" } else { "127.0.0.1" },
                address.port()
            ),
            state,
            worker,
            certificate,
        }
    }
    fn device(&self, model: &str) -> Device {
        Device::parse(json!({"device_id":uuid::Uuid::new_v4(),"name":"Protocol fixture","driver_id":if model.starts_with("GL-") {"glinet-comet"} else {"pikvm-v4"},"model":model,"address":self.address,"keyboard_layout":"de","minimum_action_latency_ms":0})).unwrap()
    }
    fn driver(&self, model: &str) -> Box<dyn KvmDriver> {
        let device = self.device(model);
        let secret = json!({"username":"admin","password":"fixture-only"});
        if model.starts_with("GL-") {
            Box::new(CometDriver::new(device, secret, None).unwrap())
        } else {
            Box::new(PiKvmDriver::new(device, secret, None).unwrap())
        }
    }
    fn transport(&self, tls: Tls, ca: Option<&str>) -> Result<Arc<LanTransport>> {
        let mut headers = HeaderMap::new();
        headers.insert("x-fixture-secret", HeaderValue::from_static("fixture-only"));
        LanTransport::new(&self.address, headers, &tls, ca)
    }
    async fn close(self) {
        self.state.cancel.cancel();
        self.worker.await.unwrap();
    }
}
async fn serve<T: AsyncRead + AsyncWrite + Unpin + Send + 'static>(io: T, state: Arc<State>) {
    let cancel = state.cancel.clone();
    let service = service_fn(move |request| handler(request, state.clone()));
    let connection = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(io), service)
        .with_upgrades();
    tokio::select! {biased;_=cancel.cancelled()=>(),_=connection=>()};
}
async fn handler(
    mut request: Request<Incoming>,
    state: Arc<State>,
) -> std::result::Result<Response<Body>, Infallible> {
    let path = request.uri().path().to_owned();
    state
        .requests
        .lock()
        .unwrap()
        .push((path.clone(), request.headers().clone()));
    let mut response = Response::new(full(Bytes::new()));
    // New sockets make pin rotation and repeated DNS checks observable.
    response
        .headers_mut()
        .insert("connection", HeaderValue::from_static("close"));
    let auth_status = state.auth_status.load(Ordering::Acquire);
    if path.starts_with("/api/") && auth_status != 200 {
        *response.status_mut() = StatusCode::from_u16(auth_status as u16).unwrap();
        return Ok(response);
    }
    if path == "/api/ws" {
        state.sockets.fetch_add(1, Ordering::AcqRel);
        let key = request.headers()["sec-websocket-key"].as_bytes();
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        response
            .headers_mut()
            .insert("connection", HeaderValue::from_static("Upgrade"));
        response
            .headers_mut()
            .insert("upgrade", HeaderValue::from_static("websocket"));
        response.headers_mut().insert(
            "sec-websocket-accept",
            HeaderValue::from_str(&derive_accept_key(key)).unwrap(),
        );
        let mut commands = state.commands.subscribe();
        tokio::spawn(async move {
            let Ok(upgraded) = hyper::upgrade::on(&mut request).await else {
                return;
            };
            let mut ws =
                WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
            loop {
                tokio::select! {biased;
                    _=state.cancel.cancelled()=>break,
                    command=commands.recv()=>match command {
                        Ok(Command::Message(value))=>if ws.send(Message::Text(value.to_string().into())).await.is_err() {break;},
                        Ok(Command::Close)=>{let _=ws.close(None).await;break;},
                        Err(_)=>break,
                    },
                    message=ws.next()=>match message {
                        Some(Ok(Message::Text(text)))=>{
                            let value:Value=serde_json::from_str(&text).unwrap();
                            state.events.lock().unwrap().push(value.clone());state.changed.notify_waiters();
                            if value["event_type"]=="ping" && state.pongs.load(Ordering::Acquire)
                                && ws.send(Message::Text(json!({"event_type":"pong","event":{}}).to_string().into())).await.is_err() {break;}
                        },
                        Some(Ok(Message::Close(_)))|Some(Err(_))|None=>break,
                        _=>(),
                    },
                }
            }
        });
    } else if path == "/redirect" {
        *response.status_mut() = StatusCode::FOUND;
        response.headers_mut().insert(
            "location",
            HeaderValue::from_static("http://8.8.8.8/credentials"),
        );
    } else if path == "/api/streamer/snapshot" {
        state.snapshot_calls.fetch_add(1, Ordering::AcqRel);
        let warmup = state
            .warmup
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok();
        let status = if warmup {
            503
        } else {
            state.status.load(Ordering::Acquire)
        };
        *response.status_mut() = StatusCode::from_u16(status as u16).unwrap();
        if status == 200 {
            *response.body_mut() = full(state.body.lock().unwrap().clone());
            let length = state.media_length.load(Ordering::Acquire);
            if length > 0 {
                response.headers_mut().insert(
                    "content-length",
                    HeaderValue::from_str(&length.to_string()).unwrap(),
                );
            }
        }
    } else if path == "/api/streamer" && state.source_offline.load(Ordering::Acquire) {
        *response.body_mut() =
            full(json!({"ok":true,"result":{"streamer":{"source":{"online":false}}}}).to_string());
    } else if path == "/streamer/stream" {
        let parts = state.stream_parts.lock().unwrap().clone();
        let stream = stream::iter(
            parts
                .into_iter()
                .map(|bytes| Ok::<_, Infallible>(Frame::data(Bytes::from(bytes)))),
        );
        *response.body_mut() = StreamBody::new(stream).boxed_unsync();
    } else {
        *response.body_mut()=full(json!({"ok":true,"result":if path=="/api/hid" {json!({"keyboard":{"online":state.hid_online.load(Ordering::Acquire)},"mouse":{"absolute":true}})} else {json!({})}}).to_string());
    }
    Ok(response)
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
fn context() -> ActionContext {
    ActionContext {
        cancellation: CancellationToken::new(),
        width: 640,
        height: 480,
        assert_control: Arc::new(|| Ok(())),
    }
}
fn action(value: Value) -> Action {
    Action::parse(value).unwrap()
}

#[tokio::test]
async fn pikvm_and_both_comet_models_preserve_persistent_hid_video_and_reconnect() {
    for model in ["PiKVM v4", "GL-RM1 (Comet)", "GL-RM10 (Comet Pro)"] {
        let fixture = Fixture::new(false).await;
        let driver = fixture.driver(model);
        driver.connect(CancellationToken::new()).await.unwrap();
        let frame = driver.snapshot(CancellationToken::new()).await.unwrap();
        assert_eq!(
            (frame.width, frame.height, frame.mime_type),
            (640, 480, kvmhelm::media::Mime::Jpeg)
        );
        let actions = vec![
            action(json!({"type":"type","text":"Ä@´"})),
            action(json!({"type":"click","x":639,"y":479})),
            action(json!({"type":"keypress","keys":["CTRL","ENTER"]})),
        ];
        driver.validate(&actions, 640, 480).unwrap();
        for action in &actions {
            driver.execute(action, &context()).await.unwrap();
        }
        until(|| {
            fixture
                .state
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|value| value["event"]["key"] == "Enter" && value["event"]["state"] == false)
        })
        .await;
        let events = fixture.state.events.lock().unwrap().clone();
        assert!(
            events
                .iter()
                .any(|value| value["event_type"] == "mouse_move"
                    && value["event"]["to"] == json!({"x":32767,"y":32767})),
            "{model}"
        );
        assert!(
            events
                .iter()
                .any(|value| value["event"]["key"] == "AltRight")
        );
        assert!(
            events
                .iter()
                .any(|value| value["event_type"] == "mouse_button"
                    && value["event"]["state"] == false)
        );
        assert_eq!(fixture.state.sockets.load(Ordering::Acquire), 1);
        assert_eq!(
            driver
                .validate(&[action(json!({"type":"type","text":"🙂"}))], 640, 480)
                .err()
                .unwrap()
                .code,
            "UNSUPPORTED_TEXT"
        );
        let caps = driver.capabilities().await.unwrap();
        if model.starts_with("GL-") {
            assert_eq!(caps.extensions["vendor"], "GL.iNet");
            assert_eq!(caps.extensions["model"], model);
        }
        assert!(
            fixture
                .state
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|(_, headers)| headers["x-kvmd-passwd"] == "fixture-only")
        );
        let mut video = driver
            .subscribe_video(CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(video.next().await.unwrap().unwrap().width, 640);
        drop(video);
        let mut changes = driver.events();
        fixture
            .state
            .commands
            .send(Command::Message(
                json!({"event_type":"streamer","event":{"streamer":{"source":{"online":false}}}}),
            ))
            .unwrap();
        assert!(matches!(
            changes.recv().await.unwrap(),
            DriverEvent::CapabilitiesChangedAt(_)
        ));
        assert_eq!(
            driver.capabilities().await.unwrap().video.signal,
            Signal::Absent
        );
        let epoch = driver.connection_epoch();
        driver.disconnect().await.unwrap();
        driver.connect(CancellationToken::new()).await.unwrap();
        assert!(driver.connection_epoch() > epoch);
        assert_eq!(
            driver.capabilities().await.unwrap().video.signal,
            Signal::Unknown
        );
        assert_eq!(fixture.state.sockets.load(Ordering::Acquire), 2);
        driver.disconnect().await.unwrap();
        fixture.close().await;
    }
}

#[tokio::test]
async fn snapshot_warmup_retries_only_503_and_cancellation_stops_read_retries() {
    let f = Fixture::new(false).await;
    let driver = f.driver("PiKVM v4");
    driver.connect(CancellationToken::new()).await.unwrap();
    f.state.warmup.store(2, Ordering::Release);
    assert_eq!(
        driver
            .snapshot(CancellationToken::new())
            .await
            .unwrap()
            .width,
        640
    );
    assert_eq!(f.state.snapshot_calls.load(Ordering::Acquire), 3);
    f.state.status.store(401, Ordering::Release);
    assert_eq!(
        driver
            .snapshot(CancellationToken::new())
            .await
            .err()
            .unwrap()
            .code,
        "AUTH_FAILED"
    );
    assert_eq!(f.state.snapshot_calls.load(Ordering::Acquire), 4);
    f.state.status.store(503, Ordering::Release);
    let cancel = CancellationToken::new();
    let timer = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        timer.cancel();
    });
    assert_eq!(
        driver.snapshot(cancel).await.err().unwrap().code,
        "FRAME_TIMEOUT"
    );
    assert_eq!(f.state.snapshot_calls.load(Ordering::Acquire), 5);
    assert_eq!(f.state.sockets.load(Ordering::Acquire), 1);
    driver.disconnect().await.unwrap();
    f.close().await;
}

#[tokio::test]
async fn ordinary_input_reaches_offline_hid_on_pikvm_and_both_comet_models() {
    for model in ["PiKVM v4", "GL-RM1 (Comet)", "GL-RM10 (Comet Pro)"] {
        let f = Fixture::new(false).await;
        f.state.hid_online.store(false, Ordering::Release);
        let driver = f.driver(model);
        driver.connect(CancellationToken::new()).await.unwrap();
        f.state.status.store(503, Ordering::Release);
        f.state.source_offline.store(true, Ordering::Release);
        assert_eq!(
            driver
                .snapshot(CancellationToken::new())
                .await
                .err()
                .unwrap()
                .code,
            "NO_SIGNAL"
        );
        assert_eq!(
            driver.capabilities().await.unwrap().video.signal,
            Signal::Absent
        );
        assert_eq!(f.state.snapshot_calls.load(Ordering::Acquire), 1);
        assert!(!driver.capabilities().await.unwrap().keyboard.hid);
        driver
            .execute(
                &action(json!({"type":"keypress","keys":["ENTER"]})),
                &context(),
            )
            .await
            .unwrap();
        driver
            .execute(
                &action(json!({"type":"move","x":32767,"y":0})),
                &ActionContext {
                    width: 32768,
                    height: 32768,
                    ..context()
                },
            )
            .await
            .unwrap();
        f.state.status.store(200, Ordering::Release);
        f.state.hid_online.store(true, Ordering::Release);
        f.state.source_offline.store(false, Ordering::Release);
        assert_eq!(
            driver
                .snapshot(CancellationToken::new())
                .await
                .unwrap()
                .signal,
            Signal::Present
        );
        assert!(driver.capabilities().await.unwrap().keyboard.hid);
        assert_eq!(f.state.sockets.load(Ordering::Acquire), 1);
        driver.disconnect().await.unwrap();
        let events = f.state.events.lock().unwrap().clone();
        assert!(
            events
                .iter()
                .any(|event| event["event"]["key"] == "Enter" && event["event"]["state"] == true)
        );
        assert!(
            events
                .iter()
                .any(|event| event["event"]["key"] == "Enter" && event["event"]["state"] == false)
        );
        assert!(
            events
                .iter()
                .any(|event| event["event_type"] == "mouse_move")
        );
        f.close().await;
    }
}

#[tokio::test]
async fn comet_firmware_failures_keep_identity_and_do_not_hide_authentication_failures() {
    for model in ["GL-RM1 (Comet)", "GL-RM10 (Comet Pro)"] {
        let f = Fixture::new(false).await;
        let driver = f.driver(model);
        for status in [404, 405, 401, 503] {
            f.state.auth_status.store(status, Ordering::Release);
            let error = driver
                .connect(CancellationToken::new())
                .await
                .err()
                .unwrap();
            assert_eq!(
                error.code,
                match status {
                    404 | 405 => "UNSUPPORTED_FIRMWARE",
                    401 => "AUTH_FAILED",
                    _ => "DEVICE_OFFLINE",
                }
            );
        }
        assert_eq!(f.state.sockets.load(Ordering::Acquire), 0);
        driver.disconnect().await.unwrap();
        f.close().await;
    }
}

fn pin(cert: &rcgen::Certificate) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(cert.der())
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}
#[tokio::test]
async fn certificate_inspection_sends_no_http_credentials_and_preserves_the_fingerprint() {
    let f = Fixture::new(true).await;
    let value = kvmhelm::transport::inspect_certificate(&f.address)
        .await
        .unwrap();
    assert_eq!(
        value["fingerprint"],
        pin(f.certificate.as_ref().unwrap())
            .replace(':', "")
            .to_ascii_lowercase()
    );
    assert!(
        value["valid_to"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
    assert!(f.state.requests.lock().unwrap().is_empty());
    assert_eq!(
        kvmhelm::transport::inspect_certificate("https://8.8.8.8")
            .await
            .err()
            .unwrap()
            .code,
        "INVALID_ADDRESS"
    );
    assert!(
        kvmhelm::transport::inspect_certificate("http://127.0.0.1")
            .await
            .is_err()
    );
    f.close().await;
}
#[tokio::test]
async fn tls_verifies_http_and_websocket_pins_before_credentials_including_after_rotation() {
    let f = Fixture::new(true).await;
    let wrong = Tls {
        fingerprint: Some("00".repeat(32)),
        insecure: Some(true),
        ..Tls::default()
    };
    let transport = f.transport(wrong, None).unwrap();
    let error = transport
        .get("/api/auth/check", &CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "AUTH_FAILED");
    assert!(!error.to_string().contains("fixture-only"));
    assert_eq!(
        transport
            .websocket("/api/ws", &CancellationToken::new())
            .await
            .err()
            .unwrap()
            .code,
        "AUTH_FAILED"
    );
    assert_eq!(f.state.requests.lock().unwrap().len(), 0);
    let correct = Tls {
        fingerprint: Some(pin(f.certificate.as_ref().unwrap())),
        ..Tls::default()
    };
    let transport = f.transport(correct, None).unwrap();
    transport
        .get("/api/auth/check", &CancellationToken::new())
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let socket = transport
        .websocket("/api/ws", &CancellationToken::new())
        .await
        .unwrap();
    drop(socket);
    assert_eq!(f.state.requests.lock().unwrap().len(), 2);
    let (_, rotated) = certificate("localhost");
    *f.state.tls.lock().unwrap() = Some(rotated);
    assert_eq!(
        transport
            .get("/api/auth/check", &CancellationToken::new())
            .await
            .err()
            .unwrap()
            .code,
        "AUTH_FAILED"
    );
    assert_eq!(
        transport
            .websocket("/api/ws", &CancellationToken::new())
            .await
            .err()
            .unwrap()
            .code,
        "AUTH_FAILED"
    );
    assert_eq!(
        f.state.requests.lock().unwrap().len(),
        2,
        "rotated peer must receive no credential-bearing request"
    );
    transport.close();
    f.close().await;
}

#[tokio::test]
async fn tls_custom_ca_hostname_and_explicit_insecure_modes_match_the_configured_policy() {
    let f = Fixture::new(true).await;
    let cancel = CancellationToken::new();
    let transport = f.transport(Tls::default(), None).unwrap();
    assert_eq!(
        transport
            .get("/api/auth/check", &cancel)
            .await
            .err()
            .unwrap()
            .code,
        "AUTH_FAILED"
    );
    assert_eq!(f.state.requests.lock().unwrap().len(), 0);
    let ca = f.certificate.as_ref().unwrap().pem();
    let transport = f.transport(Tls::default(), Some(&ca)).unwrap();
    transport
        .get("/api/auth/check", &cancel)
        .await
        .unwrap()
        .bytes(1024)
        .await
        .unwrap();
    drop(transport.websocket("/api/ws", &cancel).await.unwrap());
    let (cert, wrong_name) = certificate("different.invalid");
    *f.state.tls.lock().unwrap() = Some(wrong_name);
    let transport = f.transport(Tls::default(), Some(&cert.pem())).unwrap();
    assert_eq!(
        transport
            .get("/api/auth/check", &cancel)
            .await
            .err()
            .unwrap()
            .code,
        "AUTH_FAILED"
    );
    assert_eq!(
        transport
            .websocket("/api/ws", &cancel)
            .await
            .err()
            .unwrap()
            .code,
        "AUTH_FAILED"
    );
    assert_eq!(f.state.requests.lock().unwrap().len(), 2);
    let transport = f
        .transport(
            Tls {
                insecure: Some(true),
                ..Tls::default()
            },
            None,
        )
        .unwrap();
    transport
        .get("/api/auth/check", &cancel)
        .await
        .unwrap()
        .bytes(1024)
        .await
        .unwrap();
    drop(transport.websocket("/api/ws", &cancel).await.unwrap());
    assert!(
        f.transport(Tls::default(), Some("not a certificate"))
            .is_err()
    );
    transport.close();
    f.close().await;
}

struct Resolver(Mutex<Vec<IpAddr>>);
#[async_trait]
impl AddressResolver for Resolver {
    async fn lookup(&self, _: &str) -> Result<Vec<IpAddr>> {
        Ok(self.0.lock().unwrap().clone())
    }
}
#[tokio::test]
async fn mixed_or_rebound_dns_is_rejected_before_any_http_or_websocket_credentials() {
    let f = Fixture::new(false).await;
    let url = url::Url::parse(&f.address).unwrap();
    let address = format!("http://device.invalid:{}", url.port().unwrap());
    let dns = Arc::new(Resolver(Mutex::new(vec![
        "127.0.0.1".parse().unwrap(),
        "8.8.8.8".parse().unwrap(),
    ])));
    let mut headers = HeaderMap::new();
    headers.insert("x-fixture-secret", HeaderValue::from_static("fixture-only"));
    let transport =
        LanTransport::with_resolver(&address, headers, &Tls::default(), None, dns.clone()).unwrap();
    let cancel = CancellationToken::new();
    assert_eq!(
        transport
            .get("/api/auth/check", &cancel)
            .await
            .err()
            .unwrap()
            .code,
        "INVALID_ADDRESS"
    );
    assert_eq!(
        transport
            .websocket("/api/ws", &cancel)
            .await
            .err()
            .unwrap()
            .code,
        "INVALID_ADDRESS"
    );
    assert_eq!(f.state.requests.lock().unwrap().len(), 0);
    *dns.0.lock().unwrap() = vec!["127.0.0.1".parse().unwrap()];
    transport
        .get("/api/auth/check", &cancel)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    transport.close();
    *dns.0.lock().unwrap() = vec!["2001:4860::1".parse().unwrap()];
    assert_eq!(
        transport
            .get("/api/auth/check", &cancel)
            .await
            .err()
            .unwrap()
            .code,
        "INVALID_ADDRESS"
    );
    assert_eq!(
        transport
            .websocket("/api/ws", &cancel)
            .await
            .err()
            .unwrap()
            .code,
        "INVALID_ADDRESS"
    );
    assert_eq!(f.state.requests.lock().unwrap().len(), 1);
    for address in [
        "http://8.8.8.8",
        "http://[2001:4860::1]",
        "http://127.0.0.1/path",
    ] {
        assert!(LanTransport::new(address, HeaderMap::new(), &Tls::default(), None).is_err());
    }
    transport.close();
    f.close().await;
}

#[tokio::test]
async fn redirects_and_cross_origin_paths_never_forward_credentials() {
    let f = Fixture::new(false).await;
    let transport = f.transport(Tls::default(), None).unwrap();
    let cancel = CancellationToken::new();
    assert_eq!(
        transport
            .get("/redirect", &cancel)
            .await
            .err()
            .unwrap()
            .details["http_status"],
        302
    );
    assert_eq!(f.state.requests.lock().unwrap().len(), 1);
    for path in [
        "//8.8.8.8/credentials",
        "https://8.8.8.8/credentials",
        "/\\8.8.8.8/credentials",
    ] {
        assert_eq!(
            transport.get(path, &cancel).await.err().unwrap().code,
            "INVALID_ADDRESS"
        );
    }
    assert_eq!(f.state.requests.lock().unwrap().len(), 1);
    transport.close();
    f.close().await;
}

#[tokio::test]
async fn image_and_mjpeg_parsing_rejects_bad_or_oversized_media_and_split_markers_work() {
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, 32, 24);
        encoder.set_color(png::ColorType::Rgb);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&vec![127u8; 32 * 24 * 3])
            .unwrap();
    }
    let png_frame = frames::inspect(png_bytes.into(), Signal::Absent)
        .await
        .unwrap();
    assert_eq!(
        (
            png_frame.width,
            png_frame.height,
            png_frame.mime_type,
            png_frame.signal
        ),
        (32, 24, kvmhelm::media::Mime::Png, Signal::Absent)
    );
    let data = jpeg();
    let mut parser = Mjpeg::default();
    parser.push(&vec![65; 70000]).unwrap();
    assert!(parser.next_frame().unwrap().is_none());
    assert!(parser.buffered_bytes() <= 2);
    for byte in &data {
        parser.push(&[*byte]).unwrap();
        if parser.buffered_bytes() > 2 && *byte != 217 {
            assert!(parser.next_frame().unwrap().is_none());
        }
    }
    let parsed = parser.next_frame().unwrap().unwrap();
    assert_eq!(parsed.as_ref(), data);
    assert_eq!(
        frames::inspect(parsed, Signal::Unknown)
            .await
            .unwrap()
            .height,
        480
    );
    let mut many = Mjpeg::default();
    many.push(&[data.clone(), data.clone()].concat()).unwrap();
    assert!(many.next_frame().unwrap().is_some());
    assert!(many.next_frame().unwrap().is_some());
    assert!(many.next_frame().unwrap().is_none());
    assert_eq!(
        frames::inspect(vec![1, 2, 3].into(), Signal::Unknown)
            .await
            .err()
            .unwrap()
            .code,
        "FRAME_TIMEOUT"
    );
    assert_eq!(
        frames::inspect(vec![0; frames::FRAME_BYTES + 1].into(), Signal::Unknown)
            .await
            .err()
            .unwrap()
            .code,
        "RESOURCE_LIMIT"
    );
    let mut large = data;
    let sof = large
        .windows(2)
        .position(|part| part == [255, 192])
        .unwrap();
    large[sof + 5..sof + 7].copy_from_slice(&4096u16.to_be_bytes());
    large[sof + 7..sof + 9].copy_from_slice(&8192u16.to_be_bytes());
    assert_eq!(
        frames::inspect(large.into(), Signal::Unknown)
            .await
            .err()
            .unwrap()
            .code,
        "RESOURCE_LIMIT"
    );
    let mut pending = Mjpeg::default();
    pending.push(&[255, 216]).unwrap();
    pending.push(&vec![1; frames::FRAME_BYTES]).unwrap();
    assert_eq!(pending.next_frame().err().unwrap().code, "RESOURCE_LIMIT");
    assert!(
        Mjpeg::default()
            .push(&vec![0; 12 * 1024 * 1024 + 1])
            .is_err()
    );
}

#[tokio::test]
async fn snapshot_http_budgets_reject_oversize_bodies_and_invalid_frames() {
    let f = Fixture::new(false).await;
    let driver = f.driver("PiKVM v4");
    *f.state.body.lock().unwrap() = vec![0; frames::FRAME_BYTES + 1];
    assert_eq!(
        driver
            .snapshot(CancellationToken::new())
            .await
            .err()
            .unwrap()
            .code,
        "RESOURCE_LIMIT"
    );
    *f.state.body.lock().unwrap() = b"not an image".to_vec();
    assert_eq!(
        driver
            .snapshot(CancellationToken::new())
            .await
            .err()
            .unwrap()
            .code,
        "FRAME_TIMEOUT"
    );
    let transport = f.transport(Tls::default(), None).unwrap();
    assert_eq!(
        transport
            .get("/api/streamer/snapshot", &CancellationToken::new())
            .await
            .unwrap()
            .bytes(5)
            .await
            .err()
            .unwrap()
            .code,
        "RESOURCE_LIMIT"
    );
    transport.close();
    driver.disconnect().await.unwrap();
    f.close().await;
}

#[tokio::test]
async fn remote_close_emits_a_tagged_fault_and_heartbeat_reconnect_has_no_automatic_input_replay() {
    let f = Fixture::new(false).await;
    let driver = f.driver("PiKVM v4");
    driver.connect(CancellationToken::new()).await.unwrap();
    let mut events = driver.events();
    let epoch = driver.connection_epoch();
    f.state.commands.send(Command::Close).unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(event,DriverEvent::DisconnectedAt(value) if value==epoch));
    assert!(
        driver
            .execute(&action(json!({"type":"click","x":1,"y":1})), &context())
            .await
            .is_err()
    );
    driver.connect(CancellationToken::new()).await.unwrap();
    assert!(driver.connection_epoch() > epoch);
    assert!(
        !f.state
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|value| value["event_type"] == "mouse_button")
    );
    f.state.pongs.store(false, Ordering::Release);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(41)).await;
    let event = events.recv().await.unwrap();
    assert!(matches!(event,DriverEvent::DisconnectedAt(value) if value==driver.connection_epoch()));
    tokio::time::resume();
    driver.disconnect().await.unwrap();
    f.close().await;
}

struct RecordingSink {
    events: Mutex<Vec<Value>>,
    presses: AtomicUsize,
    fail_at: usize,
    cancel: Option<CancellationToken>,
}
#[async_trait]
impl HidTransport for RecordingSink {
    async fn event(&self, kind: &str, value: Value, context: Option<&ActionContext>) -> Result<()> {
        if let Some(context) = context {
            context.check()?;
        }
        self.events
            .lock()
            .unwrap()
            .push(json!({"event_type":kind,"event":value}));
        if value["state"] == true {
            let count = self.presses.fetch_add(1, Ordering::AcqRel) + 1;
            if count == self.fail_at {
                if let Some(cancel) = &self.cancel {
                    cancel.cancel();
                }
                return Err(GatewayError::new("EXECUTION_UNKNOWN"));
            }
        }
        Ok(())
    }
}
#[tokio::test]
async fn hid_releases_every_chord_member_after_an_uncertain_press_even_after_cancellation() {
    let hid = Hid::default();
    let ctx = context();
    let sink = RecordingSink {
        events: Mutex::new(Vec::new()),
        presses: AtomicUsize::new(0),
        fail_at: 2,
        cancel: Some(ctx.cancellation.clone()),
    };
    assert_eq!(
        hid.execute(
            &sink,
            &action(json!({"type":"keypress","keys":["CTRL","ENTER"]})),
            &ctx,
            Layout::De
        )
        .await
        .err()
        .unwrap()
        .code,
        "EXECUTION_UNKNOWN"
    );
    assert!(ctx.cancellation.is_cancelled());
    let events = sink.events.lock().unwrap().clone();
    assert_eq!(events.len(), 4);
    assert_eq!(events[2]["event"], json!({"key":"Enter","state":false}));
    assert_eq!(
        events[3]["event"],
        json!({"key":"ControlLeft","state":false})
    );
    assert!(!hid.has_pressed());
}

#[tokio::test]
async fn transport_connection_and_hid_byte_budgets_fail_before_accepting_extra_work() {
    let f = Fixture::new(false).await;
    let transport = f.transport(Tls::default(), None).unwrap();
    let cancel = CancellationToken::new();
    let mut responses = Vec::new();
    for _ in 0..4 {
        responses.push(transport.get("/api/auth/check", &cancel).await.unwrap());
    }
    assert_eq!(
        transport
            .get("/api/auth/check", &cancel)
            .await
            .err()
            .unwrap()
            .code,
        "RESOURCE_LIMIT"
    );
    drop(responses.pop());
    transport
        .get("/api/auth/check", &cancel)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    transport.close();
    assert_eq!(
        responses
            .pop()
            .unwrap()
            .bytes(1024)
            .await
            .err()
            .unwrap()
            .code,
        "DEVICE_OFFLINE"
    );
    drop(responses);
    let driver = PiKvmDriver::new(f.device("PiKVM v4"), json!({}), None).unwrap();
    driver.connect(cancel).await.unwrap();
    assert_eq!(
        driver
            .event(
                "key",
                json!({"key":"A".repeat(65536),"state":true}),
                Some(&context())
            )
            .await
            .err()
            .unwrap()
            .code,
        "RESOURCE_LIMIT"
    );
    driver
        .execute(
            &action(json!({"type":"keypress","keys":["ENTER"]})),
            &context(),
        )
        .await
        .unwrap();
    until(|| {
        f.state
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|value| value["event"]["key"] == "Enter" && value["event"]["state"] == false)
    })
    .await;
    assert!(!f.state.events.lock().unwrap().iter().any(|value| {
        value["event"]["key"]
            .as_str()
            .is_some_and(|key| key.len() > 100)
    }));
    driver.disconnect().await.unwrap();
    f.close().await;
}

#[tokio::test]
async fn native_core_uses_the_registered_network_adapter_and_never_replays_accepted_hid() {
    use kvmhelm::{
        auth::Auth,
        contracts::SCOPES,
        core::{Core, Limits},
        secrets::Secrets,
        store::Store,
    };
    let f = Fixture::new(false).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data")).await.unwrap();
    let auth = Arc::new(Auth::load(store.clone()).await.unwrap());
    let issued = auth
        .create(
            json!({"name":"Network core fixture","scopes":SCOPES,"devices":["*"]}),
            None,
        )
        .await
        .unwrap();
    let identity = auth.authenticate(&issued.token, "protocol-client").unwrap();
    let core = Core::new(
        store.clone(),
        Secrets::open(dir.path().join("secrets")).await.unwrap(),
        auth,
        Limits::default(),
        120000,
    )
    .unwrap();
    core.load().await.unwrap();
    let mut device = serde_json::to_value(f.device("PiKVM v4")).unwrap();
    device["media_profile"] = "active".into();
    let device = core
        .save_device(
            &identity,
            device,
            Some(json!({"username":"admin","password":"fixture-only"})),
            None,
        )
        .await
        .unwrap();
    let id = device["device_id"].as_str().unwrap();
    let opened = core
        .tool(
            &identity,
            "open_computer",
            json!({"computer_id":id,"mode":"control","request_id":"open"}),
        )
        .await;
    assert!(opened.success(), "{}", opened.metadata());
    assert_eq!(opened.images[0].info.width, 640);
    let session = opened.body["session"]["session_id"].as_str().unwrap();
    let frame = &opened.images[0].info;
    let request = json!({"session_id":session,"request_id":"click","reference":{"frame_id":frame.frame_id,"view_id":frame.view_id,"input_revision":frame.input_revision},"actions":[{"type":"click","x":639,"y":479}],"observation":{"mode":"none"}});
    let first = core.tool(&identity, "computer", request.clone()).await;
    assert!(first.success(), "{}", first.metadata());
    let retry = core.tool(&identity, "computer", request).await;
    assert_eq!(retry.metadata(), first.metadata());
    until(|| {
        f.state
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event["event_type"] == "mouse_button" && event["event"]["state"] == false)
    })
    .await;
    assert_eq!(f.state.events.lock().unwrap().iter().filter(|event|event["event_type"]=="mouse_button" && event["event"]["state"]==true).count(),1);
    core.stop_control(&identity, id, false).await.unwrap();
    assert!(core.get(id).unwrap().lease().is_none());
    core.shutdown().await;
    store.close().await.unwrap();
    f.close().await;
}
