//! Cross-language tests deliberately require the *actual* original code. Keep
//! them opt-in so the native library and normal cargo tests never require Node.
use kvmhelm::{
    auth::{Auth, TokenSpec},
    contracts::{Action, Device, Layout, SCOPES, ToolRequest, Validate},
    core::{Core, Limits, ToolResult},
    drivers::{
        ActionContext,
        network_hid::{Hid, HidTransport},
    },
    keyboard::{key_name, text_keys, usb_key},
    network::is_local_address,
    secrets::Secrets,
    store::{Kind, Store},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
};

struct TraceSink(Mutex<Vec<Value>>);

#[test]
#[ignore = "requires Node 24, npm dependencies and npm run build"]
fn advertised_mcp_contract_matches_the_original_sdk_registration() {
    let expected: Value = serde_json::from_str(&legacy("mcp-contract", None)).unwrap();
    assert_eq!(kvmhelm::mcp::contract(), &expected);
}
#[test]
#[ignore = "requires Node 24, npm dependencies and npm run build"]
fn auth_rate_keys_match_the_original_ipv6_subnet_and_mapped_ipv4_policy() {
    let cases: Vec<Value> = serde_json::from_str(&legacy("rate-keys", None)).unwrap();
    for case in cases {
        assert_eq!(
            kvmhelm::server::rate_limit_key(case["value"].as_str().unwrap().parse().unwrap()),
            case["key"]
        );
    }
}
#[async_trait::async_trait]
impl HidTransport for TraceSink {
    async fn event(
        &self,
        kind: &str,
        value: Value,
        context: Option<&ActionContext>,
    ) -> kvmhelm::Result<()> {
        if let Some(context) = context {
            context.check()?;
        }
        self.0
            .lock()
            .unwrap()
            .push(json!({"event_type":kind,"event":value}));
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires Node 24, npm dependencies and npm run build"]
async fn actual_dom_hid_event_sequences_match_for_layouts_drag_scroll_and_rounding() {
    let cases: Vec<Value> = serde_json::from_str(&legacy("hid", None)).unwrap();
    for case in cases {
        let action = Action::parse(case["action"].clone()).unwrap();
        let sink = TraceSink(Mutex::new(Vec::new()));
        let hid = Hid::default();
        let context = ActionContext {
            cancellation: tokio_util::sync::CancellationToken::new(),
            width: case["width"].as_u64().unwrap() as u32,
            height: case["height"].as_u64().unwrap() as u32,
            assert_control: Arc::new(|| Ok(())),
        };
        let status = hid
            .execute(&sink, &action, &context, Layout::De)
            .await
            .unwrap();
        assert_eq!(json!(status), case["status"]);
        assert_eq!(
            json!(*sink.0.lock().unwrap()),
            case["events"],
            "{}",
            case["action"]
        );
        assert!(!hid.has_pressed());
    }
}

fn legacy(command: &str, dir: Option<&Path>) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut process = Command::new("node");
    process
        .current_dir(root)
        .arg("scripts/rust-compat-fixture.mjs")
        .arg(command);
    if let Some(dir) = dir {
        process.arg(dir);
    }
    let output = process
        .output()
        .expect("Node 24 and npm run build required for legacy comparison");
    assert!(
        output.status.success(),
        "legacy fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test]
#[ignore = "requires Node 24, npm dependencies and npm run build"]
async fn actual_typescript_and_rust_read_each_others_database_secrets_and_tokens() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(legacy("prepare", Some(dir.path())), "ok");
    let store = Store::open(dir.path().join("data")).await.unwrap();
    assert!(
        store.migration_backup().is_none(),
        "schema v1 is already the common on-disk format"
    );
    let secrets = Secrets::open(dir.path().join("secrets")).await.unwrap();
    let auth = Auth::load(store.clone()).await.unwrap();
    let pat = fs::read_to_string(dir.path().join("legacy.pat")).unwrap();
    let identity = auth.authenticate(&pat, "native-client").unwrap();
    auth.check(&identity, Some("input:write"), None).unwrap();
    let device_id = "11111111-1111-4111-8111-111111111111";
    let mut device: Value = store.get(Kind::Devices, device_id).await.unwrap().unwrap();
    let reference = device["secret_ref"].as_str().unwrap();
    let mut secret = secrets.get(Some(reference)).await.unwrap();
    assert_eq!(secret["password"], "fixture-ä🙂");
    assert_eq!(
        secrets.get(Some("legacy-ca")).await.unwrap(),
        json!({"pem":"fixture certificate text"})
    );
    secret["native"] = true.into();
    secrets.set(reference, secret).await.unwrap();
    secrets
        .set("native-secret", json!({"unicode":"Grüße 🙂","count":7}))
        .await
        .unwrap();
    device["name"] = "Native fixture".into();
    store
        .put(Kind::Devices, device_id, device, 1)
        .await
        .unwrap();
    let mut preferences: Value = store
        .get(Kind::Preferences, "owner")
        .await
        .unwrap()
        .unwrap();
    preferences["theme"] = "light".into();
    store
        .put(Kind::Preferences, "owner", preferences, 1)
        .await
        .unwrap();
    assert_eq!(
        store
            .get::<Value>(Kind::Plugins, "fixture")
            .await
            .unwrap()
            .unwrap()["enabled"],
        false
    );
    let native = auth
        .create(
            json!({"name":"native fixture","scopes":SCOPES,"devices":["*"]}),
            Some(&identity),
        )
        .await
        .unwrap();
    fs::write(dir.path().join("native.pat"), native.token.as_bytes()).unwrap();
    auth.revoke(&identity.token_id).await.unwrap();
    store.close().await.unwrap();
    assert_eq!(legacy("verify", Some(dir.path())), "ok");
}

#[test]
#[ignore = "requires Node 24, npm dependencies and npm run build"]
fn strict_schemas_and_defaults_match_the_actual_zod_implementation() {
    let cases: Vec<Value> = serde_json::from_str(&legacy("contracts", None)).unwrap();
    for (index, case) in cases.iter().enumerate() {
        let input = case["input"].clone();
        let kind = case["kind"].as_str().unwrap();
        let result = match kind {
            "device" => Device::parse(input).map(|v| serde_json::to_value(v).unwrap()),
            "action" => Action::parse(input).map(|v| serde_json::to_value(v).unwrap()),
            "token" => TokenSpec::parse(input).map(|v| serde_json::to_value(v).unwrap()),
            kind => ToolRequest::parse_tool(kind, input).map(|v| v.normalized_args()),
        };
        assert_eq!(
            result.is_ok(),
            case["ok"].as_bool().unwrap(),
            "case {index} kind {kind}"
        );
        if let Ok(normalized) = result {
            // JSON number equality across int/f64 serialization is semantic.
            let expected: Value = serde_json::from_str(&case["normalized"].to_string()).unwrap();
            if kind == "token" && normalized["expires_at"].is_number() {
                assert_eq!(
                    normalized["expires_at"].as_f64(),
                    expected["expires_at"].as_f64()
                );
                let mut normalized = normalized;
                normalized["expires_at"] = expected["expires_at"].clone();
                assert_eq!(normalized, expected, "case {index}");
            } else {
                assert_eq!(normalized, expected, "case {index} kind {kind}");
            }
        }
    }
    assert!(cases.len() >= 100);
}

#[test]
#[ignore = "requires Node 24, npm dependencies and npm run build"]
fn all_reference_text_mappings_key_aliases_and_usb_usages_match() {
    let reference: Value = serde_json::from_str(&legacy("mapping", None)).unwrap();
    for case in reference["text"].as_array().unwrap() {
        let layout = if case["layout"] == "de" {
            Layout::De
        } else {
            Layout::Us
        };
        let result = text_keys(case["text"].as_str().unwrap(), layout);
        assert_eq!(
            result.is_ok(),
            case["ok"].as_bool().unwrap(),
            "character in {:?}",
            layout
        );
        if let Ok(result) = result {
            assert_eq!(json!(result), case["value"]);
        } else {
            assert_eq!(result.unwrap_err().code, case["code"]);
        }
    }
    for case in reference["keys"].as_array().unwrap() {
        let key = case["key"].as_str().unwrap();
        let dom = key_name(key);
        let usb = usb_key(key);
        assert_eq!(dom.is_ok(), case["dom"]["ok"].as_bool().unwrap(), "{key}");
        assert_eq!(usb.is_ok(), case["usb"]["ok"].as_bool().unwrap(), "{key}");
        if let Ok(dom) = dom {
            assert_eq!(dom, case["dom"]["value"]);
        }
        if let Ok(usb) = usb {
            assert_eq!(json!(usb), case["usb"]["value"]);
        }
    }
}

#[test]
#[ignore = "requires Node 24, npm dependencies and npm run build"]
fn network_policy_matches_reference_boundaries() {
    let cases: Vec<Value> = serde_json::from_str(&legacy("network", None)).unwrap();
    for case in cases {
        assert_eq!(
            is_local_address(case["value"].as_str().unwrap()),
            case["local"].as_bool().unwrap()
        );
    }
}

#[tokio::test]
#[ignore = "requires Node 24, npm dependencies and npm run build"]
async fn native_session_action_replay_and_error_transcripts_match_the_original_core() {
    fn summary(r: &ToolResult) -> Value {
        let b = &r.body;
        json!({"ok":r.success(),"code":b.get("error").and_then(|e|e.get("code")),"actions":b.get("actions").cloned().unwrap_or_else(||json!([])),"frames":r.images.len(),
            "mode":b.get("session").and_then(|s|s.get("mode")),"lease":b.get("lease").is_some_and(|l|!l.is_null()),"closed":b.get("session_closed").cloned().unwrap_or(false.into()),
            "remaining":b.get("journal_remaining").or_else(||b.get("session").and_then(|s|s.get("journal_remaining"))),"input_revision":b.get("input_revision"),
            "suspended":b.get("control_suspended"),"computers":b.get("computers").and_then(Value::as_array).map(Vec::len)})
    }
    fn record(trace: &mut Vec<Value>, label: &str, r: &ToolResult) {
        trace.push(json!([label, summary(r)]));
    }
    let original = tempfile::tempdir().unwrap();
    let expected: Value = serde_json::from_str(&legacy("core", Some(original.path()))).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data")).await.unwrap();
    let auth = Arc::new(Auth::load(store.clone()).await.unwrap());
    let token = auth
        .create(
            json!({"name":"core fixture","scopes":SCOPES,"devices":["*"]}),
            None,
        )
        .await
        .unwrap();
    let a = auth.authenticate(&token.token, "a").unwrap();
    let b = auth.authenticate(&token.token, "b").unwrap();
    let core = Core::new(
        store.clone(),
        Secrets::open(dir.path().join("secrets")).await.unwrap(),
        auth,
        Limits::default(),
        120000,
    )
    .unwrap();
    core.load().await.unwrap();
    let device=core.save_device(&a,json!({"name":"Core fixture","driver_id":"simulator","transport":"simulator","minimum_action_latency_ms":0,"tags":["core"]}),None,None).await.unwrap();
    let mut trace = Vec::new();
    record(
        &mut trace,
        "list",
        &core
            .tool(&a, "list_computers", json!({"tags":["core"]}))
            .await,
    );
    let open = core
        .tool(
            &a,
            "open_computer",
            json!({"computer_id":device["device_id"],"mode":"observe","request_id":"open"}),
        )
        .await;
    record(&mut trace, "open", &open);
    let session = open.body["session"]["session_id"].clone();
    record(
        &mut trace,
        "acquire",
        &core
            .tool(
                &a,
                "computer_control",
                json!({"session_id":session,"request_id":"acquire","operation":"acquire"}),
            )
            .await,
    );
    let observed = core
        .tool(&a, "computer_screenshot", json!({"session_id":session}))
        .await;
    record(&mut trace, "observe", &observed);
    let i = &observed.images[0].info;
    let request = json!({"session_id":session,"request_id":"click","reference":{"frame_id":i.frame_id,"view_id":i.view_id,"input_revision":i.input_revision},"actions":[{"type":"click","x":10,"y":10}],"observation":{"mode":"none"}});
    record(
        &mut trace,
        "input",
        &core.tool(&a, "computer", request.clone()).await,
    );
    record(
        &mut trace,
        "retry",
        &core.tool(&a, "computer", request.clone()).await,
    );
    let mut conflict = request.clone();
    conflict["actions"][0]["x"] = 11.into();
    record(
        &mut trace,
        "conflict",
        &core.tool(&a, "computer", conflict).await,
    );
    let mut stale = request;
    stale["request_id"] = "stale".into();
    record(&mut trace, "stale", &core.tool(&a, "computer", stale).await);
    record(
        &mut trace,
        "forbidden",
        &core
            .tool(&b, "computer_screenshot", json!({"session_id":session}))
            .await,
    );
    record(
        &mut trace,
        "stop",
        &ToolResult::ok(
            core.stop_control(&b, device["device_id"].as_str().unwrap(), false)
                .await
                .unwrap(),
        ),
    );
    record(
        &mut trace,
        "blocked",
        &core
            .tool(
                &a,
                "computer_control",
                json!({"session_id":session,"request_id":"blocked","operation":"acquire"}),
            )
            .await,
    );
    record(
        &mut trace,
        "resume",
        &ToolResult::ok(
            core.stop_control(&b, device["device_id"].as_str().unwrap(), true)
                .await
                .unwrap(),
        ),
    );
    record(
        &mut trace,
        "reacquire",
        &core
            .tool(
                &a,
                "computer_control",
                json!({"session_id":session,"request_id":"reacquire","operation":"acquire"}),
            )
            .await,
    );
    record(
        &mut trace,
        "release",
        &core
            .tool(
                &a,
                "computer_control",
                json!({"session_id":session,"request_id":"release","operation":"release"}),
            )
            .await,
    );
    record(
        &mut trace,
        "close",
        &core
            .tool(&a, "close_computer", json!({"session_id":session}))
            .await,
    );
    record(
        &mut trace,
        "lost",
        &core
            .tool(&a, "computer_screenshot", json!({"session_id":session}))
            .await,
    );
    let actual = json!({"trace":trace,"batches":core.metrics()["batches"]});
    core.shutdown().await;
    store.close().await.unwrap();
    assert_eq!(actual, expected);
}
