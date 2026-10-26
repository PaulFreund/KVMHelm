//! Native command entry point. Remote administration talks to the existing
//! daemon; offline commands explicitly open the selected protected store.
use crate::{
    GatewayError, Result,
    auth::Auth,
    contracts::SCOPES,
    core::{Core, Limits},
    private_fs,
    secrets::Secrets,
    server::{Options, Server},
    store::{Kind, Store},
};
use reqwest::Method;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

#[derive(Default)]
struct Arguments {
    positions: Vec<String>,
    values: HashMap<String, Vec<String>>,
    dev: bool,
}
impl Arguments {
    fn parse() -> Result<Self> {
        let mut result = Self::default();
        let mut input = std::env::args().skip(1);
        while let Some(arg) = input.next() {
            if arg == "--" {
                result.positions.extend(input);
                break;
            }
            if arg == "--dev" {
                result.dev = true;
                continue;
            }
            if arg == "--help" || arg == "-h" {
                result.positions = vec!["help".into()];
                continue;
            }
            if let Some(option) = arg.strip_prefix("--") {
                let (name, value) = if let Some((name, value)) = option.split_once('=') {
                    (name.to_owned(), value.to_owned())
                } else {
                    (
                        option.to_owned(),
                        input
                            .next()
                            .filter(|s| !s.starts_with("--"))
                            .ok_or_else(GatewayError::invalid)?,
                    )
                };
                if ![
                    "data",
                    "secrets",
                    "host",
                    "lan-host",
                    "lan-port",
                    "port",
                    "cert",
                    "key",
                    "ca",
                    "origin",
                    "url",
                    "pat-file",
                    "name",
                    "scope",
                    "device",
                    "file",
                    "transport",
                    "out",
                ]
                .contains(&name.as_str())
                {
                    return Err(GatewayError::invalid());
                }
                result.values.entry(name).or_default().push(value);
            } else {
                result.positions.push(arg);
            }
        }
        Ok(result)
    }
    fn value(&self, name: &str) -> Option<String> {
        self.values.get(name).and_then(|v| v.last()).cloned()
    }
    fn configured(&self, name: &str, env: &str) -> Option<String> {
        self.value(name).or_else(|| std::env::var(env).ok())
    }
    fn list(&self, name: &str, default: &[&str]) -> Vec<String> {
        self.values
            .get(name)
            .cloned()
            .unwrap_or_else(|| default.iter().map(|v| (*v).into()).collect())
    }
}
struct Client {
    http: reqwest::Client,
    url: url::Url,
    pat: zeroize::Zeroizing<String>,
}
fn storage_path(args: &Arguments, name: &str, environment: &str) -> Result<PathBuf> {
    if let Some(path) = args.configured(name, environment) {
        return Ok(path.into());
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            GatewayError::message("STORAGE_ERROR", "Set --data/--secrets or a home directory")
        })?;
    Ok(home.join(".kvmhelm").join(name))
}
fn pat_file(args: &Arguments) -> Result<PathBuf> {
    if let Some(path) = args.value("pat-file") {
        return Ok(path.into());
    }
    // The fixed process environment takes precedence and needs no filesystem.
    if std::env::var_os("KVMHELM_PAT").is_some() {
        return Ok(PathBuf::new());
    }
    Ok(storage_path(args, "secrets", "KVMHELM_SECRETS")?.join("cli.pat"))
}
impl Client {
    fn new(args: &Arguments) -> Result<Self> {
        let (http, url) = Self::transport(args)?;
        let pat = crate::mcp_bridge::read_pat(&pat_file(args)?)?;
        Ok(Self { http, url, pat })
    }
    fn transport(args: &Arguments) -> Result<(reqwest::Client, url::Url)> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let url = url::Url::parse(
            &args
                .configured("url", "KVMHELM_URL")
                .unwrap_or_else(|| "http://127.0.0.1:8765".into()),
        )
        .map_err(|_| GatewayError::new("INVALID_ADDRESS"))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(GatewayError::new("INVALID_ADDRESS"));
        }
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(40));
        if let Some(path) = args.configured("ca", "KVMHELM_CA") {
            let mut bytes = Vec::new();
            std::fs::File::open(path)
                .and_then(|file| file.take(1024 * 1024 + 1).read_to_end(&mut bytes))
                .map_err(|_| GatewayError::new("TLS_CERTIFICATE_ERROR"))?;
            if bytes.len() > 1024 * 1024 {
                return Err(GatewayError::new("RESOURCE_LIMIT"));
            }
            let certs = reqwest::Certificate::from_pem_bundle(&bytes)
                .map_err(|_| GatewayError::new("TLS_CERTIFICATE_ERROR"))?;
            if certs.is_empty() {
                return Err(GatewayError::new("TLS_CERTIFICATE_ERROR"));
            }
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
        }
        let http = builder
            .build()
            .map_err(|_| GatewayError::new("INTERNAL_ERROR"))?;
        Ok((http, url))
    }
    async fn request(&self, path: &str, method: Method, body: Option<Value>) -> Result<Value> {
        let url = self
            .url
            .join(path)
            .map_err(|_| GatewayError::new("INVALID_ADDRESS"))?;
        if url.origin() != self.url.origin() {
            return Err(GatewayError::new("INVALID_ADDRESS"));
        }
        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(self.pat.as_str())
            .header("x-kvmhelm-client", "cli");
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        let mut response = request
            .send()
            .await
            .map_err(|error| crate::transport::network_error(&error))?;
        let status = response.status();
        let mut data = Vec::new();
        while let Some(bytes) = response
            .chunk()
            .await
            .map_err(|_| GatewayError::new("DEVICE_OFFLINE"))?
        {
            if bytes.len() > (64 * 1024 * 1024usize).saturating_sub(data.len()) {
                return Err(GatewayError::new("RESOURCE_LIMIT"));
            }
            data.extend_from_slice(&bytes);
        }
        let value: Value =
            serde_json::from_slice(&data).map_err(|_| GatewayError::new("DEVICE_OFFLINE"))?;
        if !status.is_success() {
            return Err(GatewayError::message(
                value["error"]["code"].as_str().unwrap_or("HTTP_ERROR"),
                value["error"]["message"]
                    .as_str()
                    .unwrap_or("Daemon request failed"),
            ));
        }
        Ok(value)
    }
    async fn api(&self, path: &str, method: Method, body: Option<Value>) -> Result<Value> {
        self.request(&format!("/api/v1{path}"), method, body).await
    }
}
fn json_file(path: &str) -> Result<Value> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(256 * 1024 + 1).read_to_end(&mut bytes))
        .map_err(|_| GatewayError::message("INVALID_ARGUMENT", "Cannot read configuration file"))?;
    if bytes.len() > 256 * 1024 {
        return Err(GatewayError::new("RESOURCE_LIMIT"));
    }
    serde_json::from_slice(&bytes).map_err(|_| GatewayError::invalid())
}
struct ProtectedOutput {
    path: Option<PathBuf>,
    file: Option<std::fs::File>,
}
impl ProtectedOutput {
    fn reserve(path: PathBuf) -> Result<Self> {
        let file = private_fs::create_new(&path).map_err(|_| {
            GatewayError::message(
                "STORAGE_ERROR",
                "Protected output must be a new private file",
            )
        })?;
        Ok(Self {
            path: Some(path),
            file: Some(file),
        })
    }
    fn write(mut self, text: &str) -> Result<()> {
        let file = self.file.as_mut().unwrap();
        file.write_all(text.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|_| GatewayError::storage())?;
        self.path = None;
        Ok(())
    }
}
impl Drop for ProtectedOutput {
    fn drop(&mut self) {
        self.file.take();
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}
fn print(value: Value) {
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
fn port(value: Option<String>, default: u16) -> Result<u16> {
    value
        .map(|v| v.parse().map_err(|_| GatewayError::invalid()))
        .unwrap_or(Ok(default))
}
fn replay_budget(value: Option<String>) -> Result<usize> {
    let mb = value
        .map(|v| v.parse::<f64>())
        .transpose()
        .map_err(|_| GatewayError::invalid())?
        .unwrap_or(64.0);
    let bytes = mb * 1024.0 * 1024.0;
    if !bytes.is_finite() || bytes < 0.0 || bytes >= usize::MAX as f64 {
        return Err(GatewayError::invalid());
    }
    Ok(bytes.floor() as usize)
}
pub(crate) struct Lifecycle {
    pub stop: tokio_util::sync::CancellationToken,
    pub ready: Box<dyn Fn() -> Result<()> + Send + Sync>,
    pub stopping: Box<dyn Fn() -> Result<()> + Send + Sync>,
}
pub async fn run() -> Result<()> {
    run_inner(None).await
}
#[cfg(windows)]
pub(crate) async fn run_service(lifecycle: Lifecycle) -> Result<()> {
    run_inner(Some(lifecycle)).await
}
async fn run_inner(lifecycle: Option<Lifecycle>) -> Result<()> {
    let mut args = Arguments::parse()?;
    if lifecycle.is_some() {
        if !args
            .positions
            .iter()
            .take(2)
            .map(String::as_str)
            .eq(["service", "run"])
        {
            return Err(GatewayError::invalid());
        }
        args.positions.splice(..2, ["serve".into()]);
    }
    let command = args.positions.first().map(String::as_str).unwrap_or("help");
    if command == "help" {
        println!(
            "KVMHelm native\n  serve [--host 127.0.0.1 --port 8765 --cert cert.pem --key key.pem]\n  token bootstrap|create|list|revoke\n  demo\n  mcp --transport stdio [--url http://127.0.0.1:8765 --pat-file protected-file]\n  device list|add --file config.json|test|benchmark|stop|resume\n  plugin list|install <directory>|enable|disable|restart|uninstall\n  secrets cleanup\n  doctor\n  healthcheck [--url https://localhost:8765 --ca trusted-ca.pem]\nUse --data and --secrets for separate protected storage locations. Remote CLI/MCP TLS trusts --ca or KVMHELM_CA in addition to public roots. JetKVM video requires FFmpeg (KVMHELM_FFMPEG, beside this executable or PATH). Plugins optionally require Node (KVMHELM_NODE, beside this executable or PATH). Build, migration and platform validation: docs/native-packaging.md and docs/native-platforms.md."
        );
        #[cfg(windows)]
        println!(
            "  service run [serve options] (Windows SCM; registration is an explicit operator step)"
        );
        return Ok(());
    }
    if command == "healthcheck" {
        let (http, url) = Client::transport(&args)?;
        let response = http
            .get(url.join("/readyz").map_err(|_| GatewayError::invalid())?)
            .timeout(Duration::from_secs(4))
            .send()
            .await
            .map_err(|error| crate::transport::network_error(&error))?;
        if !response.status().is_success() {
            return Err(GatewayError::new("STORAGE_ERROR"));
        }
        // Read only the small readiness body, independently of credentials and
        // local storage; the daemon's HTTP limits still apply to this request.
        let mut bytes = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| GatewayError::new("DEVICE_OFFLINE"))?
        {
            if bytes.len() + chunk.len() > 4096 {
                return Err(GatewayError::new("RESOURCE_LIMIT"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| GatewayError::new("DEVICE_OFFLINE"))?;
        if value["ok"] != true {
            return Err(GatewayError::new("STORAGE_ERROR"));
        }
        print(value);
        return Ok(());
    }
    let sub = args.positions.get(1).map(String::as_str).unwrap_or("");
    if command == "mcp" {
        if args
            .value("transport")
            .is_some_and(|transport| transport != "stdio")
        {
            return Err(GatewayError::message(
                "INVALID_ARGUMENT",
                "Use --transport stdio",
            ));
        }
        let client = Client::new(&args)?;
        return crate::mcp_bridge::run(client.http, client.url, pat_file(&args)?).await;
    }
    if command == "doctor"
        || command == "device"
        || command == "plugin"
        || command == "token" && sub != "bootstrap"
    {
        let client = Client::new(&args)?;
        let value = match (command, sub) {
            ("doctor", _) => {
                json!({"runtime":"rust","platform":std::env::consts::OS,"arch":std::env::consts::ARCH,"url":client.url.as_str(),"health":client.request("/healthz",Method::GET,None).await?,"diagnostics":client.api("/diagnostics",Method::GET,None).await?})
            }
            ("device", "list") => client.api("/devices", Method::GET, None).await?,
            ("plugin", "list") => client.api("/plugins", Method::GET, None).await?,
            ("plugin", "install") => {
                let path =
                    std::path::absolute(args.positions.get(2).ok_or_else(GatewayError::invalid)?)
                        .map_err(|_| GatewayError::invalid())?;
                client
                    .api(
                        "/plugins",
                        Method::POST,
                        Some(json!({"path":path,"devices":args.list("device",&[])})),
                    )
                    .await?
            }
            ("plugin", "enable" | "disable" | "uninstall" | "restart") => {
                let id = args
                    .positions
                    .get(2)
                    .filter(|s| {
                        !s.is_empty()
                            && s.bytes().all(|b| {
                                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
                            })
                    })
                    .ok_or_else(GatewayError::invalid)?;
                client
                    .api(
                        &format!("/plugins/{id}/{sub}"),
                        Method::POST,
                        Some(json!({})),
                    )
                    .await?
            }
            ("device", "add") => {
                client
                    .api(
                        "/devices",
                        Method::POST,
                        Some(json_file(
                            &args.value("file").ok_or_else(GatewayError::invalid)?,
                        )?),
                    )
                    .await?
            }
            ("device", "test" | "benchmark" | "stop" | "resume") => {
                let id = args.positions.get(2).ok_or_else(GatewayError::invalid)?;
                if uuid::Uuid::parse_str(id).is_err() {
                    return Err(GatewayError::invalid());
                }
                client
                    .api(
                        &format!("/devices/{id}/{sub}"),
                        Method::POST,
                        Some(json!({})),
                    )
                    .await?
            }
            ("token", "list") => client.api("/tokens", Method::GET, None).await?,
            ("token", "revoke") => {
                let id = args.positions.get(2).ok_or_else(GatewayError::invalid)?;
                if uuid::Uuid::parse_str(id).is_err() {
                    return Err(GatewayError::invalid());
                }
                client
                    .api(&format!("/tokens/{id}"), Method::DELETE, None)
                    .await?
            }
            ("token", "create") => {
                let output = args
                    .value("out")
                    .map(|path| ProtectedOutput::reserve(path.into()))
                    .transpose()?;
                let mut value = client.api("/tokens", Method::POST, Some(json!({
                    "name":args.value("name").unwrap_or_else(||"client".into()),
                    "scopes":args.list("scope",&["devices:read","video:read","input:write"]),
                    "devices":args.list("device",&["*"])
                }))).await?;
                if let Some(output) = output {
                    output.write(&format!(
                        "{}\n",
                        value["token"]
                            .as_str()
                            .ok_or_else(|| GatewayError::new("INTERNAL_ERROR"))?
                    ))?;
                    value.as_object_mut().unwrap().remove("token");
                }
                value
            }
            _ => return Err(GatewayError::invalid()),
        };
        print(value);
        return Ok(());
    }
    if !matches!(command, "serve" | "demo" | "secrets" | "token")
        || command == "token" && sub != "bootstrap"
        || command == "secrets" && sub != "cleanup"
    {
        return Err(GatewayError::invalid());
    }
    let data = storage_path(&args, "data", "KVMHELM_DATA")?;
    let secrets = storage_path(&args, "secrets", "KVMHELM_SECRETS")?;
    let store = Store::open(&data).await?;
    let work = async {
        let vault = Secrets::open(&secrets).await?;
        let auth = Arc::new(Auth::load(store.clone()).await?);
        if command == "token" {
            let path = args.value("pat-file").map(PathBuf::from)
                .unwrap_or_else(|| secrets.join("cli.pat"));
            // Reserve the destination before issuing a token, so an existing
            // bootstrap PAT never causes an unusable new token to be issued.
            let output = ProtectedOutput::reserve(path.clone())?;
            let token = auth.create(json!({
                "name":args.value("name").unwrap_or_else(||"local owner".into()),
                "scopes":SCOPES,"devices":["*"]
            }), None).await?;
            output.write(&format!("{}\n", token.token.as_str()))?;
            println!("Bootstrap PAT written once to {}. Protect this file; restart existing daemons to load locally bootstrapped tokens.", path.display());
            return Ok(());
        }
        if command == "secrets" {
            let mut refs = HashSet::new();
            for device in store.list::<Value>(Kind::Devices).await? {
                for reference in [device["secret_ref"].as_str(), device["tls"]["ca_ref"].as_str()]
                    .into_iter().flatten() {
                    refs.insert(reference.into());
                }
            }
            print(json!({"deleted":vault.prune(refs).await?}));
            return Ok(());
        }
        let limits = Limits {
            max_sessions: std::env::var("KVMHELM_MAX_SESSIONS").ok().map(|v| v.parse())
                .transpose().map_err(|_| GatewayError::invalid())?.unwrap_or(128),
            replay_bytes: replay_budget(std::env::var("KVMHELM_REPLAY_MB").ok())?,
            ..Limits::default()
        };
        let core = Core::new(store.clone(), vault, auth, limits, 120000)?;
        let result = async {
            if command == "demo" {
                core.load_devices().await?;
                let pat = crate::mcp_bridge::read_pat(&pat_file(&args)?)?;
                let identity = core.auth.authenticate(&pat, "cli")?;
                for n in 1..=4 {
                    core.save_device(&identity, json!({
                        "name":format!("Lab {n:02} · Simulator"),"driver_id":"simulator",
                        "model":"Synthetic KVM","transport":"simulator","tags":["demo"]
                    }), None, None).await?;
                }
                println!("Added four explicitly simulated targets.");
                return Ok(());
            }
            core.load().await?;
            let lan = args.configured("lan-host", "KVMHELM_LAN_HOST");
            let cert = args.configured("cert", "KVMHELM_CERT").map(PathBuf::from);
            let key = args.configured("key", "KVMHELM_KEY").map(PathBuf::from);
            let options = Options {
                host: args.configured("host", "KVMHELM_HOST").unwrap_or_else(|| "127.0.0.1".into()),
                port: port(args.configured("port", "KVMHELM_PORT"), 8765)?,
                certificate: if lan.is_some() { None } else { cert.clone() },
                key: if lan.is_some() { None } else { key.clone() },
                origins: args.list("origin", &[]), dev: args.dev,
                ..Options::default()
            };
            let primary = Server::bind(core.clone(), options, crate::web_assets::WEB_ASSETS).await?;
            let secondary = if let Some(host) = lan {
                let options = Options {
                    host, port: port(args.configured("lan-port", "KVMHELM_LAN_PORT"), 8766)?,
                    certificate: cert, key, origins: args.list("origin", &[]),
                    ..Options::default()
                };
                match Server::bind(core.clone(), options, crate::web_assets::WEB_ASSETS).await {
                    Ok(server) => Some(server),
                    Err(error) => { primary.close().await; return Err(error); }
                }
            } else { None };
            eprintln!("KVMHelm listening on {}", primary.url);
            if let Some(server) = &secondary { eprintln!("KVMHelm LAN listening on {}", server.url); }
            let signal = async {
                if let Some(lifecycle) = &lifecycle {
                    (lifecycle.ready)()?;
                    // SCM processes have no console. Their registered control
                    // handler owns stop/shutdown; do not install Ctrl+C here.
                    lifecycle.stop.cancelled().await;
                    Ok(())
                } else { shutdown_signal().await }
            }.await;
            let stopping = lifecycle.as_ref().map(|l| (l.stopping)()).transpose();
            primary.close().await;
            if let Some(server) = secondary { server.close().await; }
            signal.and(stopping.map(|_| ()))
        }.await;
        core.shutdown().await;
        result
    }.await;
    let closed = store.close().await;
    work.and(closed)
}
pub(crate) async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|_| GatewayError::new("INTERNAL_ERROR"))?;
        tokio::select! {_=tokio::signal::ctrl_c()=>(),_=terminate.recv()=>()};
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .map_err(|_| GatewayError::new("INTERNAL_ERROR"))
    }
}
