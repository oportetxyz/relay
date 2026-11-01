//! Better Auth JWT verification for user-tied gas-sponsorship quota.
//!
//! The app's Better Auth `jwt()` plugin issues EdDSA (Ed25519) JWTs whose `sub`
//! is the user id, and serves the public keys as a JWKS at
//! `${authUrl}/api/auth/jwks`. This module verifies a bearer token locally
//! against that JWKS and exposes the verified `sub` to the RPC layer via an http
//! [`Extensions`](http::Extensions) insert, so the sponsorship evaluator can key
//! quota by user rather than by address.
//!
//! Verification is deliberately hand-rolled on `ed25519-dalek` (already in the
//! tree) rather than a JOSE crate: an EdDSA JWT is three base64url segments and
//! the JWKS key is a raw 32-byte Ed25519 public key, so the whole path is a
//! decode + one signature check. Every failure is fail-closed (returns `None`);
//! an unauthenticated request simply carries no `sub` and falls back to
//! address-mode quota.

use std::{
    collections::HashMap,
    task::{Context, Poll},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::primitives::Address;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, VerifyingKey};
use futures::{FutureExt, future::BoxFuture};
use http::header;
use jsonrpsee::{
    core::BoxError,
    server::{HttpBody, HttpRequest, HttpResponse},
};
use serde::Deserialize;
use tokio::sync::RwLock;
use tower::{Layer, Service};
use tracing::{debug, warn};

/// A verified Better Auth subject (`sub` claim = user id), inserted into the
/// request extensions by [`JwtAuthService`] and read by the RPC layer.
#[derive(Clone, Debug)]
pub struct VerifiedSub(pub String);

/// LOCAL-ONLY dev escape hatch. A request whose bearer token equals `api_key`
/// (compared in constant time) is accepted without JWKS verification and gets
/// `subject` injected as its [`VerifiedSub`], so local dev can exercise the
/// user-mode quota path without a real Better Auth JWT.
///
/// The token is STATIC by design — rotatable via Infisical, but never a
/// JWKS-signing secret (a rotating signing secret would reintroduce the
/// JWKS-rotation orphan bug). It is allowed on the shared prod relay; a boot
/// assertion requires the injected subject to carry a dedicated non-zero quota
/// override so the exposed key is always tightly capped. Injecting the subject
/// does NOT bypass gating — it still flows through the chain/target guard,
/// breaker, and quota; the hatch only shortcuts JWKS *identity*.
#[derive(Clone, Debug)]
pub struct DevBypass {
    /// The static bearer that unlocks the hatch.
    pub api_key: String,
    /// The subject injected on a match (defaults to `"dev-local"` at the call site).
    pub subject: String,
}

/// Constant-time byte-slice equality, to avoid a timing oracle on the dev key.
/// Returns `false` immediately on a length mismatch (the key length is not
/// itself secret), otherwise accumulates differences without early exit.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Deserialize)]
struct JwtHeader {
    alg: String,
    kid: String,
}

#[derive(Deserialize)]
struct JwtPayload {
    sub: String,
    exp: i64,
}

#[derive(Deserialize)]
struct WalletsClaim {
    wallets: Vec<String>,
}

/// The addresses a request may be sponsored for, present only when
/// `auth.enforce_wallet_claim` is on. Empty for a caller without a valid token, and absent for
/// the dev hatch, which has no token to read.
#[derive(Debug, Clone)]
pub struct TokenWallets(pub Vec<String>);

impl TokenWallets {
    /// Whether the token lists `address`.
    pub fn permits(&self, address: Address) -> bool {
        let address = format!("{address:#x}");
        self.0.iter().any(|wallet| wallet.eq_ignore_ascii_case(&address))
    }
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    crv: String,
    /// base64url-encoded raw 32-byte Ed25519 public key.
    x: String,
    kid: String,
}

/// Fetches and caches the app's JWKS, and verifies EdDSA bearer tokens against
/// it. Cheap to clone (shared `Arc` inner via the `RwLock`); one instance is
/// shared across all requests.
#[derive(Clone, Debug)]
pub struct JwksCache {
    client: reqwest::Client,
    jwks_url: String,
    keys: std::sync::Arc<RwLock<HashMap<String, VerifyingKey>>>,
}

impl JwksCache {
    /// Create a cache that fetches keys from `jwks_url`
    /// (e.g. `https://onramp.xyz/api/auth/jwks`).
    pub fn new(jwks_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            jwks_url,
            keys: std::sync::Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Verify a bearer token, returning the `sub` claim on success. Fail-closed:
    /// any malformed segment, unknown key, bad signature, or expired token
    /// yields `None`.
    pub async fn verify(&self, token: &str) -> Option<String> {
        let mut parts = token.split('.');
        let header_b64 = parts.next()?;
        let payload_b64 = parts.next()?;
        let sig_b64 = parts.next()?;
        if parts.next().is_some() {
            return None;
        }

        let header: JwtHeader =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header_b64).ok()?).ok()?;
        if header.alg != "EdDSA" {
            debug!(alg = %header.alg, "jwt: unsupported alg");
            return None;
        }

        let key = self.key_for_kid(&header.kid).await?;

        let sig = Signature::from_slice(&URL_SAFE_NO_PAD.decode(sig_b64).ok()?).ok()?;
        // The JWS signing input is the ASCII `header.payload` (pre-decode bytes).
        let signing_input = format!("{header_b64}.{payload_b64}");
        key.verify_strict(signing_input.as_bytes(), &sig).ok()?;

        let payload: JwtPayload =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload_b64).ok()?).ok()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
        if payload.exp <= now {
            debug!("jwt: expired");
            return None;
        }

        Some(payload.sub)
    }

    /// Look up a key by `kid`, refreshing the JWKS once on a miss (handles key
    /// rotation without a restart).
    async fn key_for_kid(&self, kid: &str) -> Option<VerifyingKey> {
        if let Some(key) = self.keys.read().await.get(kid).copied() {
            return Some(key);
        }
        // Unknown kid: the app may have rotated its signing key. Refresh once.
        if let Err(err) = self.refresh().await {
            warn!(%err, "jwt: JWKS refresh failed");
            return None;
        }
        self.keys.read().await.get(kid).copied()
    }

    async fn refresh(&self) -> eyre::Result<()> {
        let body = self.client.get(&self.jwks_url).send().await?.bytes().await?;
        let jwks: Jwks = serde_json::from_slice(&body)?;

        let mut map = HashMap::new();
        for jwk in jwks.keys {
            if jwk.kty != "OKP" || jwk.crv != "Ed25519" {
                continue;
            }
            let Ok(bytes) = URL_SAFE_NO_PAD.decode(&jwk.x) else { continue };
            let Ok(bytes): Result<[u8; 32], _> = bytes.try_into() else { continue };
            let Ok(key) = VerifyingKey::from_bytes(&bytes) else { continue };
            map.insert(jwk.kid, key);
        }
        *self.keys.write().await = map;
        Ok(())
    }
}

fn bearer_token(headers: &http::HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
}

/// Tower layer that resolves an optional `Authorization: Bearer` credential to a
/// [`VerifiedSub`] and inserts it into the request extensions. A matching dev
/// key (when configured) is honored first; otherwise the token is verified
/// against the JWKS (when a cache is configured). With neither cache nor dev
/// hatch it is a pass-through, so address-mode / `sponsor_all` deployments need
/// no JWKS endpoint.
#[derive(Clone, Debug)]
pub struct JwtAuthLayer {
    cache: Option<JwksCache>,
    dev: Option<DevBypass>,
    enforce_wallets: bool,
}

impl JwtAuthLayer {
    /// Build the layer. `cache` enables JWKS verification; `dev` enables the
    /// local escape hatch. Both `None` = pass-through. `enforce_wallets` exposes a token's
    /// addresses as [`TokenWallets`].
    pub fn new(cache: Option<JwksCache>, dev: Option<DevBypass>, enforce_wallets: bool) -> Self {
        Self { cache, dev, enforce_wallets }
    }
}

impl<S> Layer<S> for JwtAuthLayer {
    type Service = JwtAuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        JwtAuthService {
            inner,
            cache: self.cache.clone(),
            dev: self.dev.clone(),
            enforce_wallets: self.enforce_wallets,
        }
    }
}

/// The `wallets` claim of an already verified token. Read apart from the rest of the payload so
/// that it plays no part in verification. Empty when the claim is absent or malformed.
fn wallets_claim(token: &str) -> Vec<String> {
    token
        .split('.')
        .nth(1)
        .and_then(|payload| URL_SAFE_NO_PAD.decode(payload).ok())
        .and_then(|payload| serde_json::from_slice::<WalletsClaim>(&payload).ok())
        .map(|claim| claim.wallets)
        .unwrap_or_default()
}

/// The service produced by [`JwtAuthLayer`]; resolves the bearer credential (dev
/// hatch first, then JWKS) before delegating to `inner`.
#[derive(Clone, Debug)]
pub struct JwtAuthService<S> {
    inner: S,
    cache: Option<JwksCache>,
    dev: Option<DevBypass>,
    enforce_wallets: bool,
}

impl<S, B> Service<HttpRequest<B>> for JwtAuthService<S>
where
    S: Service<HttpRequest<B>, Response = HttpResponse<HttpBody>> + Clone + Send + 'static,
    S::Error: Into<BoxError>,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut request: HttpRequest<B>) -> Self::Future {
        let cache = self.cache.clone();
        let dev = self.dev.clone();
        let enforce_wallets = self.enforce_wallets;
        // Call the ready clone; keep `self.inner` as the fresh clone so we never
        // call a service that wasn't polled ready.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        // Copy the bearer out so the immutable header borrow is released before
        // we mutate the request extensions below.
        let token = bearer_token(request.headers()).map(str::to_owned);

        async move {
            // why: with the check on, a request with no valid token lists nothing. Leaving it
            // out would skip the check on chains that sponsor callers without a token.
            let mut wallets = enforce_wallets.then(Vec::new);
            if let Some(token) = token {
                if let Some(dev) = &dev
                    && ct_eq(token.as_bytes(), dev.api_key.as_bytes())
                {
                    // Local dev escape hatch: static bearer, no JWKS round-trip.
                    debug!(subject = %dev.subject, "jwt: dev escape hatch accepted bearer");
                    request.extensions_mut().insert(VerifiedSub(dev.subject.clone()));
                    wallets = None;
                } else if let Some(cache) = &cache
                    && let Some(sub) = cache.verify(&token).await
                {
                    request.extensions_mut().insert(VerifiedSub(sub));
                    wallets = wallets.map(|_| wallets_claim(&token));
                }
            }
            if let Some(wallets) = wallets {
                request.extensions_mut().insert(TokenWallets(wallets));
            }
            inner.call(request).await.map_err(Into::into)
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn make_jwt(sk: &SigningKey, kid: &str, sub: &str, exp: i64) -> String {
        let header = serde_json::json!({ "alg": "EdDSA", "kid": kid });
        let payload = serde_json::json!({ "sub": sub, "exp": exp });
        let h = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let p = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let signing_input = format!("{h}.{p}");
        let sig = sk.sign(signing_input.as_bytes());
        let s = URL_SAFE_NO_PAD.encode(sig.to_bytes());
        format!("{h}.{p}.{s}")
    }

    async fn preloaded_cache(kid: &str, key: VerifyingKey) -> JwksCache {
        // Port 1 = immediate connection refusal, so an unknown-kid refresh
        // fails fast instead of hanging the test.
        let cache = JwksCache::new("http://127.0.0.1:1/jwks".into());
        cache.keys.write().await.insert(kid.to_string(), key);
        cache
    }

    #[tokio::test]
    async fn verifies_valid_token() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let cache = preloaded_cache("kid-1", sk.verifying_key()).await;
        let token = make_jwt(&sk, "kid-1", "user-123", 32_503_680_000);
        assert_eq!(cache.verify(&token).await.as_deref(), Some("user-123"));
    }

    #[tokio::test]
    async fn rejects_tampered_signature() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let cache = preloaded_cache("kid-1", sk.verifying_key()).await;
        let mut token = make_jwt(&sk, "kid-1", "user-123", 32_503_680_000);
        let last = token.pop().unwrap();
        token.push(if last == 'A' { 'B' } else { 'A' });
        assert!(cache.verify(&token).await.is_none());
    }

    #[tokio::test]
    async fn rejects_expired_token() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let cache = preloaded_cache("kid-1", sk.verifying_key()).await;
        let token = make_jwt(&sk, "kid-1", "user-123", 1);
        assert!(cache.verify(&token).await.is_none());
    }

    #[tokio::test]
    async fn rejects_wrong_key() {
        let signer = SigningKey::from_bytes(&[7u8; 32]);
        let other = SigningKey::from_bytes(&[9u8; 32]);
        // Cache holds `other`'s key under kid-1; token is signed by `signer`.
        let cache = preloaded_cache("kid-1", other.verifying_key()).await;
        let token = make_jwt(&signer, "kid-1", "user-123", 32_503_680_000);
        assert!(cache.verify(&token).await.is_none());
    }

    /// A signed token with `wallets` as its claim.
    fn jwt_with_wallets(sk: &SigningKey, wallets: serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"EdDSA","kid":"kid-1"}"#);
        let payload =
            serde_json::json!({ "sub": "u", "exp": 32_503_680_000i64, "wallets": wallets });
        let input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(payload.to_string()));
        let signature = sk.sign(input.as_bytes()).to_bytes();
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    #[tokio::test]
    async fn a_malformed_claim_lists_nothing_and_still_verifies() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let cache = preloaded_cache("kid-1", sk.verifying_key()).await;
        let deep = format!("{}{}", "[".repeat(300), "]".repeat(300));
        let deep = serde_json::Value::String(deep);
        for wallets in [serde_json::json!("0xabc"), serde_json::json!([1]), deep] {
            let token = jwt_with_wallets(&sk, wallets);
            assert!(cache.verify(&token).await.is_some());
            assert!(wallets_claim(&token).is_empty());
        }
    }

    #[test]
    fn the_claim_is_read_from_the_payload() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let token = jwt_with_wallets(&sk, serde_json::json!(["0xabc"]));
        assert_eq!(wallets_claim(&token), ["0xabc"]);
    }

    #[test]
    fn permits_compares_addresses_whatever_their_case() {
        let address = Address::repeat_byte(0xab);
        let upper = format!("0x{}", "AB".repeat(20));
        assert!(TokenWallets(vec![format!("{address:#x}")]).permits(address));
        assert!(TokenWallets(vec![upper]).permits(address));
        assert!(!TokenWallets(vec![format!("{:#x}", Address::repeat_byte(1))]).permits(address));
        assert!(!TokenWallets(vec![]).permits(address));
    }

    #[test]
    fn ct_eq_matches_std_eq() {
        assert!(ct_eq(b"secret-token", b"secret-token"));
        assert!(!ct_eq(b"secret-token", b"secret-toker"));
        assert!(!ct_eq(b"secret", b"secret-token")); // length mismatch
        assert!(ct_eq(b"", b""));
    }

    /// Drives a request with the given bearer through the real [`JwtAuthLayer`]
    /// tower service and returns the [`VerifiedSub`] the inner service observed.
    async fn resolved_sub(
        dev: Option<DevBypass>,
        cache: Option<JwksCache>,
        bearer: Option<&str>,
    ) -> Option<String> {
        use std::sync::{Arc, Mutex};
        use tower::ServiceExt;

        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sink = seen.clone();
        let inner = tower::service_fn(move |req: HttpRequest<HttpBody>| {
            let sink = sink.clone();
            async move {
                if let Some(v) = req.extensions().get::<VerifiedSub>() {
                    *sink.lock().unwrap() = Some(v.0.clone());
                }
                Ok::<HttpResponse<HttpBody>, BoxError>(HttpResponse::new(HttpBody::empty()))
            }
        });

        let mut svc = JwtAuthLayer::new(cache, dev, false).layer(inner);
        let mut builder = HttpRequest::builder();
        if let Some(t) = bearer {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = builder.body(HttpBody::empty()).unwrap();
        svc.ready().await.unwrap().call(req).await.unwrap();
        seen.lock().unwrap().clone()
    }

    fn dev_bypass() -> DevBypass {
        DevBypass { api_key: "dev-secret-123".into(), subject: "dev-local".into() }
    }

    // The dev escape hatch accepts a matching static bearer and injects the
    // fixed dev subject — no JWKS cache involved.
    #[tokio::test]
    async fn dev_hatch_injects_subject() {
        let sub = resolved_sub(Some(dev_bypass()), None, Some("dev-secret-123")).await;
        assert_eq!(sub.as_deref(), Some("dev-local"));
    }

    // A non-matching bearer with the hatch on but no JWKS cache yields no
    // verified subject (fail-closed; falls through to address-mode).
    #[tokio::test]
    async fn dev_hatch_rejects_wrong_key() {
        let sub = resolved_sub(Some(dev_bypass()), None, Some("not-the-key")).await;
        assert_eq!(sub, None);
    }

    // No bearer at all -> no subject.
    #[tokio::test]
    async fn no_bearer_no_subject() {
        let sub = resolved_sub(Some(dev_bypass()), None, None).await;
        assert_eq!(sub, None);
    }

    // Hatch off + no cache = pure pass-through even with a bearer present.
    #[tokio::test]
    async fn passthrough_when_disabled() {
        let sub = resolved_sub(None, None, Some("anything")).await;
        assert_eq!(sub, None);
    }
}
