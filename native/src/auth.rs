//! Existing PAT hashes and UI sessions. Identities carry only stable IDs;
//! authorization always looks up the current token, including after revocation.
use crate::{
    GatewayError, Result,
    contracts::{SCOPES, Validate, bounded_text},
    store::{Kind, Store},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    sync::RwLock,
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use zeroize::Zeroizing;

#[derive(Clone, Deserialize, Serialize)]
pub struct Token {
    pub id: String,
    pub name: String,
    pub hash: String,
    pub scopes: Vec<String>,
    pub devices: Vec<String>,
    pub expires_at: Option<f64>,
    pub created_at: u64,
    pub last_used: Option<u64>,
    pub revoked: bool,
    pub revision: i64,
}

impl Token {
    pub fn public(&self) -> Value {
        let mut value = serde_json::to_value(self).expect("token is serializable");
        value
            .as_object_mut()
            .expect("token is an object")
            .remove("hash");
        value
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TokenSpec {
    pub name: String,
    pub scopes: Vec<String>,
    pub devices: Vec<String>,
    #[serde(default)]
    pub expires_at: Option<f64>,
}

impl Validate for TokenSpec {
    fn validate(&self) -> Result<()> {
        bounded_text(&self.name, 1, 100)?;
        if self.scopes.is_empty()
            || self.devices.is_empty()
            || self
                .scopes
                .iter()
                .any(|scope| !SCOPES.contains(&scope.as_str()))
            || self.expires_at.is_some_and(|expires| !expires.is_finite())
        {
            return Err(GatewayError::invalid());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Identity {
    pub token_id: String,
    pub client_id: String,
    pub ui_id: Option<String>,
}

pub struct CreatedToken {
    pub token: Zeroizing<String>,
    pub metadata: Value,
}
impl CreatedToken {
    pub fn wire(&self) -> Value {
        let mut value = self.metadata.clone();
        value
            .as_object_mut()
            .expect("token metadata is an object")
            .insert("token".into(), self.token.as_str().into());
        value
    }
}

pub struct Login {
    pub id: String,
    pub csrf: String,
}
struct UiSession {
    token_id: String,
    csrf: String,
    expires_at: u64,
}
#[derive(Default)]
struct State {
    tokens: BTreeMap<String, Token>,
    uis: HashMap<String, UiSession>,
}
pub struct Auth {
    store: Store,
    state: RwLock<State>,
    writing: Mutex<()>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

pub fn digest(input: &str) -> String {
    format!("{:x}", Sha256::digest(input.as_bytes()))
}
pub fn bearer_client(secret: &str, client: &str) -> String {
    digest(&format!("{secret}:{client}"))
}
fn auth_failed() -> GatewayError {
    GatewayError::new("AUTH_FAILED")
}
fn random_secret() -> Result<String> {
    let mut bytes = Zeroizing::new([0u8; 32]);
    OsRng
        .try_fill_bytes(bytes.as_mut())
        .map_err(|_| GatewayError::new("EXECUTION_UNKNOWN"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes.as_ref()))
}

impl Auth {
    pub async fn load(store: Store) -> Result<Self> {
        let tokens: Vec<Token> = store.list(Kind::Tokens).await?;
        Ok(Self {
            store,
            state: RwLock::new(State {
                tokens: tokens.into_iter().map(|t| (t.id.clone(), t)).collect(),
                uis: HashMap::new(),
            }),
            writing: Mutex::new(()),
        })
    }

    pub fn check(
        &self,
        identity: &Identity,
        scope: Option<&str>,
        device: Option<&str>,
    ) -> Result<Token> {
        let state = self.state.read().map_err(|_| auth_failed())?;
        check(&state, identity, scope, device, now_ms()).cloned()
    }

    pub fn list(&self) -> Result<Vec<Value>> {
        Ok(self
            .state
            .read()
            .map_err(|_| auth_failed())?
            .tokens
            .values()
            .map(Token::public)
            .collect())
    }

    pub fn authenticate(&self, secret: &str, client_id: &str) -> Result<Identity> {
        // PATs have an ASCII UUID at byte offsets 3..39. Avoid slicing arbitrary
        // UTF-8 input at those offsets (which must never panic on malformed input).
        let id = secret
            .strip_prefix("kh_")
            .and_then(|s| s.get(..36))
            .unwrap_or("");
        let mut state = self.state.write().map_err(|_| auth_failed())?;
        let token = state.tokens.get(id).ok_or_else(auth_failed)?;
        let expected = Sha256::digest(secret.as_bytes());
        let stored = decode_hash(&token.hash).ok_or_else(auth_failed)?;
        if !bool::from(expected.as_slice().ct_eq(&stored)) {
            return Err(auth_failed());
        }
        let identity = Identity {
            token_id: id.into(),
            client_id: client_id.into(),
            ui_id: None,
        };
        let now = now_ms();
        check(&state, &identity, None, None, now)?;
        state.tokens.get_mut(id).ok_or_else(auth_failed)?.last_used = Some(now);
        Ok(identity)
    }

    pub async fn create(&self, input: Value, issuer: Option<&Identity>) -> Result<CreatedToken> {
        let spec = TokenSpec::parse(input)?;
        let _writing = self.writing.lock().await;
        if let Some(issuer) = issuer {
            let token = self.check(issuer, Some("tokens:manage"), None)?;
            if spec
                .scopes
                .iter()
                .any(|scope| !token.scopes.contains(scope))
                || spec
                    .devices
                    .iter()
                    .any(|device| !token.devices.iter().any(|d| d == "*" || d == device))
            {
                return Err(GatewayError::message(
                    "FORBIDDEN",
                    "Cannot grant permissions beyond the issuing token",
                ));
            }
            if let Some(expires) = token.expires_at
                && spec.expires_at.is_none_or(|end| end > expires)
            {
                return Err(GatewayError::message(
                    "FORBIDDEN",
                    "Cannot extend token lifetime",
                ));
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        let secret = Zeroizing::new(format!("kh_{id}_{}", random_secret()?));
        let token = Token {
            id: id.clone(),
            name: spec.name,
            hash: digest(&secret),
            scopes: spec.scopes,
            devices: spec.devices,
            expires_at: spec.expires_at,
            created_at: now_ms(),
            last_used: None,
            revoked: false,
            revision: 0,
        };
        let stored = self
            .store
            .put(
                Kind::Tokens,
                &id,
                serde_json::to_value(token).map_err(|_| GatewayError::storage())?,
                0,
            )
            .await?;
        let token: Token = serde_json::from_value(stored).map_err(|_| GatewayError::storage())?;
        let metadata = token.public();
        self.state
            .write()
            .map_err(|_| auth_failed())?
            .tokens
            .insert(id, token);
        Ok(CreatedToken {
            token: secret,
            metadata,
        })
    }

    /// The integrating Core must also close this token's sessions and release
    /// its inputs. Ongoing operations fail their next check immediately.
    pub async fn revoke(&self, id: &str) -> Result<()> {
        let _writing = self.writing.lock().await;
        let mut token = self
            .state
            .read()
            .map_err(|_| auth_failed())?
            .tokens
            .get(id)
            .cloned()
            .ok_or_else(|| GatewayError::new("NOT_FOUND"))?;
        token.revoked = true;
        let value = serde_json::to_value(&token).map_err(|_| GatewayError::storage())?;
        let stored = self
            .store
            .put(Kind::Tokens, id, value, token.revision)
            .await?;
        let token: Token = serde_json::from_value(stored).map_err(|_| GatewayError::storage())?;
        let mut state = self.state.write().map_err(|_| auth_failed())?;
        state.tokens.insert(id.into(), token);
        state.uis.retain(|_, session| session.token_id != id);
        Ok(())
    }

    pub fn login(&self, pat: &str) -> Result<Login> {
        let id = random_secret()?;
        let csrf = random_secret()?;
        let identity = self.authenticate(pat, &id)?;
        let mut state = self.state.write().map_err(|_| auth_failed())?;
        let now = now_ms();
        check(&state, &identity, None, None, now)?;
        state.uis.retain(|_, session| session.expires_at > now);
        if state.uis.len() >= 1024 {
            return Err(GatewayError::new("RESOURCE_LIMIT"));
        }
        state.uis.insert(
            id.clone(),
            UiSession {
                token_id: identity.token_id,
                csrf: csrf.clone(),
                expires_at: now + 8 * 3_600_000,
            },
        );
        Ok(Login { id, csrf })
    }

    pub fn cookie(&self, id: &str, client: &str) -> Result<(Identity, String)> {
        let state = self.state.read().map_err(|_| auth_failed())?;
        let session = state.uis.get(id).ok_or_else(auth_failed)?;
        let identity = Identity {
            token_id: session.token_id.clone(),
            client_id: digest(&format!("{id}:{client}")),
            ui_id: Some(id.into()),
        };
        check(&state, &identity, None, None, now_ms())?;
        Ok((identity, session.csrf.clone()))
    }

    pub fn logout(&self, id: &str) -> Result<()> {
        self.state
            .write()
            .map_err(|_| auth_failed())?
            .uis
            .remove(id);
        Ok(())
    }

    pub fn valid_csrf(&self, identity: &Identity, supplied: &str) -> Result<bool> {
        let state = self.state.read().map_err(|_| auth_failed())?;
        check(&state, identity, None, None, now_ms())?;
        let session = identity
            .ui_id
            .as_ref()
            .and_then(|id| state.uis.get(id))
            .ok_or_else(auth_failed)?;
        Ok(bool::from(
            session.csrf.as_bytes().ct_eq(supplied.as_bytes()),
        ))
    }
}

fn check<'a>(
    state: &'a State,
    identity: &Identity,
    scope: Option<&str>,
    device: Option<&str>,
    now: u64,
) -> Result<&'a Token> {
    if let Some(id) = &identity.ui_id {
        let ui = state.uis.get(id).ok_or_else(auth_failed)?;
        if ui.token_id != identity.token_id || ui.expires_at <= now {
            return Err(auth_failed());
        }
    }
    let token = state
        .tokens
        .get(&identity.token_id)
        .ok_or_else(auth_failed)?;
    if token.revoked || token.expires_at.is_some_and(|end| end <= now as f64) {
        return Err(auth_failed());
    }
    if scope.is_some_and(|scope| !token.scopes.iter().any(|s| s == scope)) {
        return Err(GatewayError::new("FORBIDDEN"));
    }
    if device.is_some_and(|device| !token.devices.iter().any(|d| d == "*" || d == device)) {
        return Err(GatewayError::new("FORBIDDEN"));
    }
    Ok(token)
}

fn decode_hash(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    let mut hash = [0u8; 32];
    for (out, pair) in hash.iter_mut().zip(value.as_bytes().as_chunks::<2>().0) {
        *out = digit(pair[0])? * 16 + digit(pair[1])?;
    }
    Some(hash)
}
