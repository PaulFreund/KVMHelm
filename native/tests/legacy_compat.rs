//! Cross-language tests deliberately require the *actual* original code. Keep
//! them opt-in so the native library and normal cargo tests never require Node.
use kvmhelm::{
    auth::{Auth, TokenSpec},
    contracts::{Action, Device, Layout, SCOPES, ToolRequest, Validate},
    keyboard::{key_name, text_keys, usb_key},
    network::is_local_address,
    secrets::Secrets,
    store::{Kind, Store},
};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

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
