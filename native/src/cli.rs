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
    io::Write,
    path::{Path, PathBuf},
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
impl Client {
    fn new(args: &Arguments, secrets: &Path) -> Result<Self> {
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
        let pat = if let Ok(pat) = std::env::var("KVMHELM_PAT") {
            pat
        } else {
            std::fs::read_to_string(
                args.value("pat-file")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| secrets.join("cli.pat")),
            )
            .map_err(|_| GatewayError::message("AUTH_FAILED", "Cannot read protected PAT file"))?
        };
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(40))
            .build()
            .map_err(|_| GatewayError::new("INTERNAL_ERROR"))?;
        Ok(Self {
            http,
            url,
            pat: zeroize::Zeroizing::new(pat.trim().to_owned()),
        })
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
    let bytes = std::fs::read(path)
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
pub async fn run() -> Result<()> {
    let args = Arguments::parse()?;
    let command = args.positions.first().map(String::as_str).unwrap_or("help");
    if command == "help" {
        println!(
            "KVMHelm native\n  serve [--host 127.0.0.1 --port 8765 --cert cert.pem --key key.pem]\n  token bootstrap|create|list|revoke\n  demo\n  device list|add --file config.json|test|benchmark|stop|resume\n  secrets cleanup\n  doctor\nUse --data and --secrets for separate protected storage locations. MCP, audio, JetKVM and plugin runtime integration are still in progress; use the documented TypeScript daemon for the complete feature set."
        );
        return Ok(());
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .ok_or_else(|| GatewayError::new("STORAGE_ERROR"))?;
    let data = args
        .configured("data", "KVMHELM_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".kvmhelm/data"));
    let secrets = args
        .configured("secrets", "KVMHELM_SECRETS")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".kvmhelm/secrets"));
    let sub = args.positions.get(1).map(String::as_str).unwrap_or("");
    if command == "doctor" || command == "device" || command == "token" && sub != "bootstrap" {
        let client = Client::new(&args, &secrets)?;
        let value = match (command, sub) {
            ("doctor", _) => {
                json!({"runtime":"rust","platform":std::env::consts::OS,"arch":std::env::consts::ARCH,"url":client.url.as_str(),"health":client.request("/healthz",Method::GET,None).await?,"diagnostics":client.api("/diagnostics",Method::GET,None).await?})
            }
            ("device", "list") => client.api("/devices", Method::GET, None).await?,
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
            replay_bytes: std::env::var("KVMHELM_REPLAY_MB").ok().map(|v| v.parse::<usize>())
                .transpose().map_err(|_| GatewayError::invalid())?.unwrap_or(64)
                .checked_mul(1024 * 1024).ok_or_else(GatewayError::invalid)?,
            ..Limits::default()
        };
        let core = Core::new(store.clone(), vault, auth, limits, 120000)?;
        let result = async {
            core.load().await?;
            if command == "demo" {
                let client = Client::new(&args, &secrets)?;
                let identity = core.auth.authenticate(&client.pat, "cli")?;
                for n in 1..=4 {
                    core.save_device(&identity, json!({
                        "name":format!("Lab {n:02} · Simulator"),"driver_id":"simulator",
                        "model":"Synthetic KVM","transport":"simulator","tags":["demo"]
                    }), None, None).await?;
                }
                println!("Added four explicitly simulated targets.");
                return Ok(());
            }
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
            let signal = shutdown_signal().await;
            primary.close().await;
            if let Some(server) = secondary { server.close().await; }
            signal
        }.await;
        core.shutdown().await;
        result
    }.await;
    let closed = store.close().await;
    work.and(closed)
}
async fn shutdown_signal() -> Result<()> {
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
