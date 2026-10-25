//! Shared LAN HTTP/WebSocket transport. Pins are checked in the TLS handshake
//! of the credential-bearing socket, never in a separate preflight connection.
use crate::{
    GatewayError, Result,
    contracts::Tls,
    network::{check_resolved_addresses, device_origin},
};
use async_trait::async_trait;
use bytes::Bytes;
use reqwest::{Client, Method, header::HeaderMap};
use rustls::{
    ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};
use std::{
    error::Error,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpStream,
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream,
    tungstenite::{client::IntoClientRequest, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;
use url::Url;

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const TIMEOUT: Duration = Duration::from_secs(5);

#[async_trait]
pub trait AddressResolver: Send + Sync {
    async fn lookup(&self, hostname: &str) -> Result<Vec<IpAddr>>;
}
pub struct SystemResolver;
#[async_trait]
impl AddressResolver for SystemResolver {
    async fn lookup(&self, hostname: &str) -> Result<Vec<IpAddr>> {
        tokio::net::lookup_host((hostname, 0))
            .await
            .map(|addresses| addresses.map(|a| a.ip()).collect())
            .map_err(|_| GatewayError::new("DEVICE_OFFLINE"))
    }
}
#[derive(Clone)]
struct LanDns(Arc<dyn AddressResolver>);
impl LanDns {
    async fn addresses(&self, hostname: &str) -> Result<Vec<IpAddr>> {
        let addresses = match hostname.parse() {
            Ok(ip) => vec![ip],
            Err(_) => self.0.lookup(hostname).await?,
        };
        check_resolved_addresses(&addresses)?;
        Ok(addresses)
    }
}
impl reqwest::dns::Resolve for LanDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let this = self.clone();
        Box::pin(async move {
            let addresses = this.addresses(name.as_str()).await?;
            let iterator: reqwest::dns::Addrs =
                Box::new(addresses.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(iterator)
        })
    }
}

#[derive(Debug)]
struct ExplicitVerifier {
    pin: Option<[u8; 32]>,
    provider: Arc<CryptoProvider>,
}
impl ServerCertVerifier for ExplicitVerifier {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        if let Some(pin) = &self.pin
            && !bool::from(pin.ct_eq(Sha256::digest(cert.as_ref()).as_slice()))
        {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ));
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
fn tls_config(tls: &Tls, ca: Option<&str>) -> Result<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    if let Some(pem) = ca {
        let certificates = rustls_pemfile::certs(&mut pem.as_bytes())
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|_| GatewayError::new("AUTH_FAILED"))?;
        if certificates.is_empty() {
            return Err(GatewayError::new("AUTH_FAILED"));
        }
        for certificate in certificates {
            roots
                .add(certificate)
                .map_err(|_| GatewayError::new("AUTH_FAILED"))?;
        }
    } else {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|_| GatewayError::new("AUTH_FAILED"))?;
    let mut config = builder.with_root_certificates(roots).with_no_client_auth();
    let pin = tls
        .fingerprint
        .as_ref()
        .map(|value| {
            let value = value.replace(':', "").to_ascii_lowercase();
            if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(GatewayError::message(
                    "AUTH_FAILED",
                    "Invalid SHA-256 certificate pin",
                ));
            }
            let mut pin = [0; 32];
            for (index, part) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
                pin[index] = u8::from_str_radix(std::str::from_utf8(part).unwrap(), 16).unwrap();
            }
            Ok(pin)
        })
        .transpose()?;
    if pin.is_some() || tls.insecure == Some(true) {
        if pin.is_some() {
            // A resumed TLS session can omit the peer certificate. Require a
            // new certificate check for each new pinned socket.
            config.resumption = rustls::client::Resumption::disabled();
        }
        config
            .dangerous()
            .set_certificate_verifier(Arc::new(ExplicitVerifier { pin, provider }));
    }
    // Never expose credentials in TLS 1.3 early data or retry a non-idempotent
    // request before certificate verification on a new connection.
    config.enable_early_data = false;
    Ok(config)
}

/// Deliberately loses transport detail; errors may contain URLs or credentials.
pub fn network_error(error: &(dyn Error + 'static)) -> GatewayError {
    let mut source = Some(error);
    while let Some(error) = source {
        if let Some(error) = error.downcast_ref::<GatewayError>() {
            return error.clone();
        }
        if let Some(error) = error.downcast_ref::<rustls::Error>()
            && matches!(
                error,
                rustls::Error::InvalidCertificate(_)
                    | rustls::Error::NoCertificatesPresented
                    | rustls::Error::UnsupportedNameType
            )
        {
            return GatewayError::new("AUTH_FAILED");
        }
        // io::Error::source can skip its boxed custom error. Inspect get_ref
        // explicitly so rustls certificate failures retain their AUTH_FAILED
        // classification through both Hyper and Tungstenite's I/O wrappers.
        source = if let Some(io) = error.downcast_ref::<std::io::Error>() {
            io.get_ref().map(|inner| inner as &(dyn Error + 'static))
        } else {
            error.source()
        };
    }
    GatewayError::new("DEVICE_OFFLINE")
}
#[derive(Clone)]
struct Session {
    client: Client,
    cancellation: CancellationToken,
}
pub struct LanTransport {
    origin: Url,
    headers: HeaderMap,
    tls: Arc<ClientConfig>,
    dns: LanDns,
    session: Mutex<Option<Session>>,
    slots: Arc<Semaphore>,
}
pub struct Body {
    response: reqwest::Response,
    cancellation: CancellationToken,
    caller: CancellationToken,
    _slot: OwnedSemaphorePermit,
}
impl Body {
    pub fn content_length(&self) -> Option<u64> {
        self.response.content_length()
    }
    pub fn headers(&self) -> &HeaderMap {
        self.response.headers()
    }
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        tokio::select! {biased;
            _=self.caller.cancelled()=>Err(GatewayError::new("FRAME_TIMEOUT")),
            _=self.cancellation.cancelled()=>Err(GatewayError::new("DEVICE_OFFLINE")),
            result=tokio::time::timeout(TIMEOUT,self.response.chunk())=>result
                .map_err(|_|GatewayError::new("FRAME_TIMEOUT"))?
                .map_err(|error|if error.is_timeout() { GatewayError::new("FRAME_TIMEOUT") } else { network_error(&error) }),
        }
    }
    pub async fn bytes(mut self, limit: usize) -> Result<Vec<u8>> {
        if self
            .content_length()
            .is_some_and(|size| size > limit as u64)
        {
            return Err(GatewayError::new("RESOURCE_LIMIT"));
        }
        tokio::time::timeout(TIMEOUT, async {
            let mut result = Vec::new();
            while let Some(chunk) = self.chunk().await? {
                if chunk.len() > limit.saturating_sub(result.len()) {
                    return Err(GatewayError::new("RESOURCE_LIMIT"));
                }
                result.extend_from_slice(&chunk);
            }
            Ok(result)
        })
        .await
        .unwrap_or_else(|_| Err(GatewayError::new("FRAME_TIMEOUT")))
    }
    pub async fn json(self) -> Result<serde_json::Value> {
        serde_json::from_slice(&self.bytes(1024 * 1024).await?)
            .map_err(|_| GatewayError::message("DEVICE_OFFLINE", "Invalid device response"))
    }
}
impl LanTransport {
    pub fn new(
        address: &str,
        headers: HeaderMap,
        tls: &Tls,
        ca: Option<&str>,
    ) -> Result<Arc<Self>> {
        Self::with_resolver(address, headers, tls, ca, Arc::new(SystemResolver))
    }
    pub fn with_resolver(
        address: &str,
        headers: HeaderMap,
        tls: &Tls,
        ca: Option<&str>,
        resolver: Arc<dyn AddressResolver>,
    ) -> Result<Arc<Self>> {
        let origin = device_origin(address)?;
        if origin.scheme() == "http" && tls.fingerprint.is_some() {
            return Err(GatewayError::message(
                "AUTH_FAILED",
                "Certificate pins require HTTPS",
            ));
        }
        Ok(Arc::new(Self {
            origin,
            headers,
            tls: Arc::new(tls_config(tls, ca)?),
            dns: LanDns(resolver),
            session: Mutex::new(None),
            slots: Arc::new(Semaphore::new(4)),
        }))
    }
    fn session(&self) -> Result<Session> {
        let mut session = self.session.lock().unwrap();
        if let Some(session) = session.as_ref() {
            return Ok(session.clone());
        }
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .http1_only()
            .connect_timeout(TIMEOUT)
            .read_timeout(TIMEOUT)
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(Duration::from_secs(30))
            .dns_resolver(Arc::new(self.dns.clone()))
            .tls_backend_preconfigured(self.tls.as_ref().clone())
            .build()
            .map_err(|_| GatewayError::new("DEVICE_OFFLINE"))?;
        let value = Session {
            client,
            cancellation: CancellationToken::new(),
        };
        *session = Some(value.clone());
        Ok(value)
    }
    fn url(&self, path: &str) -> Result<Url> {
        let url = self
            .origin
            .join(path)
            .map_err(|_| GatewayError::new("INVALID_ADDRESS"))?;
        if !path.starts_with('/')
            || path.starts_with("//")
            || url.origin() != self.origin.origin()
            || url.fragment().is_some()
        {
            return Err(GatewayError::new("INVALID_ADDRESS"));
        }
        Ok(url)
    }
    pub fn close(&self) {
        if let Some(session) = self.session.lock().unwrap().take() {
            session.cancellation.cancel();
        }
    }
    pub async fn get(&self, path: &str, cancellation: &CancellationToken) -> Result<Body> {
        self.request(Method::GET, path, HeaderMap::new(), None, cancellation)
            .await
    }
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        headers: HeaderMap,
        body: Option<Vec<u8>>,
        cancellation: &CancellationToken,
    ) -> Result<Body> {
        let url = self.url(path)?;
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| GatewayError::new("RESOURCE_LIMIT"))?;
        let session = self.session()?;
        let mut request = session
            .client
            .request(method, url)
            .headers(self.headers.clone())
            .headers(headers);
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = tokio::select! {biased;
            _=cancellation.cancelled()=>return Err(GatewayError::new("FRAME_TIMEOUT")),
            _=session.cancellation.cancelled()=>return Err(GatewayError::new("DEVICE_OFFLINE")),
            result=tokio::time::timeout(TIMEOUT,request.send())=>result
                .map_err(|_|GatewayError::new("DEVICE_OFFLINE"))?
                .map_err(|error|network_error(&error))?,
        };
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let mut error = GatewayError::message(
                if matches!(status, 401 | 403) {
                    "AUTH_FAILED"
                } else {
                    "DEVICE_OFFLINE"
                },
                &format!("Device HTTP {status}"),
            );
            error.details.insert("http_status".into(), status.into());
            return Err(error);
        }
        Ok(Body {
            response,
            cancellation: session.cancellation,
            caller: cancellation.clone(),
            _slot: slot,
        })
    }
    pub async fn websocket(&self, path: &str, cancellation: &CancellationToken) -> Result<Socket> {
        let mut url = self.url(path)?;
        let hostname = url.host_str().unwrap().trim_matches(['[', ']']).to_owned();
        let port = url.port_or_known_default().unwrap();
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| GatewayError::new("INVALID_ADDRESS"))?;
        let work = async {
            let addresses = self.dns.addresses(&hostname).await?;
            let addresses = addresses
                .into_iter()
                .map(|ip| SocketAddr::new(ip, port))
                .collect::<Vec<_>>();
            let tcp = TcpStream::connect(addresses.as_slice())
                .await
                .map_err(|_| GatewayError::new("DEVICE_OFFLINE"))?;
            tcp.set_nodelay(true)
                .map_err(|_| GatewayError::new("DEVICE_OFFLINE"))?;
            let mut request = url
                .as_str()
                .into_client_request()
                .map_err(|_| GatewayError::new("INVALID_ADDRESS"))?;
            request.headers_mut().extend(self.headers.clone());
            let config = WebSocketConfig::default()
                .read_buffer_size(4096)
                .write_buffer_size(0)
                .max_write_buffer_size(65536)
                .max_message_size(Some(65536))
                .max_frame_size(Some(65536));
            let (socket, _) = tokio_tungstenite::client_async_tls_with_config(
                request,
                tcp,
                Some(config),
                Some(Connector::Rustls(self.tls.clone())),
            )
            .await
            .map_err(|error| match &error {
                tokio_tungstenite::tungstenite::Error::Http(response)
                    if matches!(response.status().as_u16(), 401 | 403) =>
                {
                    GatewayError::new("AUTH_FAILED")
                }
                _ => network_error(&error),
            })?;
            Ok(socket)
        };
        tokio::select! {biased;
            _=cancellation.cancelled()=>Err(GatewayError::new("DEVICE_OFFLINE")),
            result=tokio::time::timeout(TIMEOUT,work)=>result.unwrap_or_else(|_|Err(GatewayError::new("DEVICE_OFFLINE"))),
        }
    }
}
impl Drop for LanTransport {
    fn drop(&mut self) {
        self.close();
    }
}
