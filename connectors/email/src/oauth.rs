//! OAuth2 for email — device-authorization flow (RFC 8628) + XOAUTH2
//! SASL auth for IMAP/SMTP.
//!
//! Device flow over loopback-redirect: the CLI runs headless and on
//! machines where spawning a browser + localhost listener is awkward.
//! Both providers support it:
//! - Google: `https://oauth2.googleapis.com/device/code` — the
//!   `https://mail.google.com/` scope covers IMAP + SMTP.
//! - Microsoft: `.../oauth2/v2.0/devicecode` on a tenant (default
//!   `common`) — `IMAP.AccessAsUser.All` + `SMTP.Send` + `offline_access`.
//!
//! Secrets: the *refresh token* is the only persisted credential — it
//! lives in the OS keystore at `email-oauth:<user>`, never in
//! `email.json`. Access tokens are resolved fresh per session via the
//! refresh grant (sessions are short-lived, so no cache layer is
//! needed); a rotated refresh token is re-stored transparently.

use base64::Engine;
use pai_core::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// OAuth block inside `email.json`. `provider` selects the endpoint
/// preset; `device_url`/`token_url`/`scopes` override for custom IdPs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthConfig {
    /// `google` | `microsoft` | `custom`.
    pub provider: String,
    /// Public-client id (device flow needs no secret).
    pub client_id: String,
    /// Microsoft tenant — `common`, `consumers`, or a GUID. Ignored by
    /// other providers.
    #[serde(default)]
    pub tenant: Option<String>,
    /// Custom-IdP overrides.
    #[serde(default)]
    pub device_url: Option<String>,
    #[serde(default)]
    pub token_url: Option<String>,
    /// Scope list override; defaults per provider.
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
}

impl OAuthConfig {
    pub fn device_url(&self) -> Result<String> {
        if let Some(u) = &self.device_url {
            return Ok(u.clone());
        }
        match self.provider.as_str() {
            "google" => Ok("https://oauth2.googleapis.com/device/code".into()),
            "microsoft" => Ok(format!(
                "https://login.microsoftonline.com/{}/oauth2/v2.0/devicecode",
                self.tenant.as_deref().unwrap_or("common")
            )),
            other => Err(Error::InvalidInput(format!(
                "unknown oauth provider {other:?} — set device_url/token_url for a custom IdP"
            ))),
        }
    }

    pub fn token_url(&self) -> Result<String> {
        if let Some(u) = &self.token_url {
            return Ok(u.clone());
        }
        match self.provider.as_str() {
            "google" => Ok("https://oauth2.googleapis.com/token".into()),
            "microsoft" => Ok(format!(
                "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
                self.tenant.as_deref().unwrap_or("common")
            )),
            other => Err(Error::InvalidInput(format!(
                "unknown oauth provider {other:?} — set device_url/token_url for a custom IdP"
            ))),
        }
    }

    /// Scope string for the device-code request.
    pub fn scope_string(&self) -> Result<String> {
        if let Some(s) = &self.scopes {
            return Ok(s.join(" "));
        }
        match self.provider.as_str() {
            // mail.google.com covers IMAP read/write + SMTP submission.
            "google" => Ok("https://mail.google.com/".into()),
            "microsoft" => Ok(
                "offline_access https://outlook.office365.com/IMAP.AccessAsUser.All \
                 https://outlook.office365.com/SMTP.Send"
                    .into(),
            ),
            other => Err(Error::InvalidInput(format!(
                "unknown oauth provider {other:?} — set scopes for a custom IdP"
            ))),
        }
    }
}

/// What the device-code endpoint hands back.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceGrant {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    /// Seconds between token polls.
    #[serde(default = "default_interval")]
    pub interval: u64,
    #[serde(default)]
    pub expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

/// Token endpoint success payload.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

/// One token-poll attempt's outcome — the CLI loops on `Pending`.
#[derive(Debug)]
pub enum Poll {
    /// Not authorized yet; keep polling (maybe slower after `slow_down`).
    Pending,
    /// Full token set.
    Granted(TokenSet),
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Provider(format!("oauth http: {e}")))
}

async fn post_form(url: &str, form: &HashMap<&str, &str>) -> Result<(u16, serde_json::Value)> {
    let resp = client()?
        .post(url)
        .form(form)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("oauth http: {e}")))?;
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("oauth response: {e}")))?;
    Ok((status, body))
}

fn json_err(body: &serde_json::Value) -> String {
    body["error_description"]
        .as_str()
        .or_else(|| body["error"].as_str())
        .unwrap_or("unknown oauth error")
        .to_string()
}

/// Step 1: request a device code. The CLI prints `user_code` +
/// `verification_uri` (or `verification_uri_complete` when present).
pub async fn device_flow(cfg: &OAuthConfig) -> Result<DeviceGrant> {
    let mut form = HashMap::new();
    form.insert("client_id", cfg.client_id.as_str());
    let scope = cfg.scope_string()?;
    form.insert("scope", scope.as_str());
    let (status, body) = post_form(&cfg.device_url()?, &form).await?;
    if status != 200 {
        return Err(Error::Provider(format!(
            "oauth device flow: {}",
            json_err(&body)
        )));
    }
    serde_json::from_value(body).map_err(|e| Error::Provider(format!("oauth device grant: {e}")))
}

/// Step 2: one poll of the token endpoint. `Pending` covers
/// `authorization_pending` AND `slow_down` — the caller sleeps
/// `grant.interval` (+5s after slow_down) and retries.
pub async fn poll_token_once(cfg: &OAuthConfig, device_code: &str) -> Result<Poll> {
    let mut form = HashMap::new();
    form.insert("client_id", cfg.client_id.as_str());
    form.insert("device_code", device_code);
    form.insert("grant_type", "urn:ietf:params:oauth:grant-type:device_code");
    let (status, body) = post_form(&cfg.token_url()?, &form).await?;
    if status == 200 {
        let set: TokenSet = serde_json::from_value(body)
            .map_err(|e| Error::Provider(format!("oauth token set: {e}")))?;
        return Ok(Poll::Granted(set));
    }
    match body["error"].as_str().unwrap_or("") {
        "authorization_pending" | "slow_down" => Ok(Poll::Pending),
        _ => Err(Error::Provider(format!("oauth poll: {}", json_err(&body)))),
    }
}

/// Exchange a refresh token for an access token. Pure w.r.t. storage —
/// callers decide where the refresh token lives. Returns the access
/// token plus a rotated refresh token when the IdP sent one.
pub async fn refresh_access_token(
    cfg: &OAuthConfig,
    refresh_token: &str,
) -> Result<(String, Option<String>)> {
    let mut form = HashMap::new();
    form.insert("client_id", cfg.client_id.as_str());
    form.insert("refresh_token", refresh_token);
    form.insert("grant_type", "refresh_token");
    let (status, body) = post_form(&cfg.token_url()?, &form).await?;
    if status != 200 {
        return Err(Error::Provider(format!(
            "oauth refresh: {} — re-run `pai email configure --oauth`",
            json_err(&body)
        )));
    }
    let set: TokenSet = serde_json::from_value(body)
        .map_err(|e| Error::Provider(format!("oauth refresh response: {e}")))?;
    Ok((set.access_token, set.refresh_token))
}

// ---------------------------------------------------------------------------
// Keystore + SASL-IR
// ---------------------------------------------------------------------------

fn oauth_key(user: &str) -> String {
    format!("email-oauth:{user}")
}

/// Persist the refresh token (the only durable OAuth secret).
pub fn store_refresh_token(user: &str, refresh_token: &str) -> bool {
    pai_identity::keystore::store(&oauth_key(user), refresh_token.as_bytes())
}

/// Resolve an access token: refresh token from the keystore → refresh
/// grant → transparent rotation re-store.
pub async fn access_token(cfg: &OAuthConfig, user: &str) -> Result<String> {
    let refresh = pai_identity::keystore::load(&oauth_key(user))
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| {
            Error::InvalidInput(
                "no oauth refresh token — run `pai email configure --oauth <provider>`".into(),
            )
        })?;
    let (access, rotated) = refresh_access_token(cfg, &refresh).await?;
    if let Some(r) = rotated {
        if !r.is_empty() && r != refresh {
            let _ = store_refresh_token(user, &r);
        }
    }
    Ok(access)
}

/// XOAUTH2 initial client response — the raw SASL bytes
/// `user=<u>\x01auth=Bearer <t>\x01\x01` (NOT base64; the SASL layer /
/// SMTP `AUTH` command encodes).
pub fn xoauth2_ir(user: &str, access_token: &str) -> String {
    format!("user={user}\x01auth=Bearer {access_token}\x01\x01")
}

/// Base64 form for SMTP's single-line `AUTH XOAUTH2 <b64>`.
pub fn xoauth2_b64(user: &str, access_token: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(xoauth2_ir(user, access_token))
}

/// imap-crate `Authenticator` carrying a prebuilt SASL-IR (XOAUTH2) —
/// `process` returns the raw bytes, the client base64-encodes them.
pub struct SaslIr(pub String);

impl imap::Authenticator for SaslIr {
    type Response = String;
    fn process(&self, _challenge: &[u8]) -> String {
        self.0.clone()
    }
}

/// What a session authenticates with — resolved before the (blocking)
/// protocol work begins.
#[derive(Debug)]
pub enum AuthMaterial {
    /// App-password path (IMAP LOGIN / SMTP AUTH PLAIN).
    Password(String),
    /// OAuth path (IMAP AUTHENTICATE XOAUTH2 / SMTP AUTH XOAUTH2) —
    /// carries the raw SASL-IR, not the bare token.
    Xoauth2(String),
}

/// Pick the credential for one session under `user`: OAuth config wins,
/// else the `email:<user>` password entry.
pub async fn resolve_auth_for(oauth: Option<&OAuthConfig>, user: &str) -> Result<AuthMaterial> {
    if let Some(oauth) = oauth {
        let token = access_token(oauth, user).await?;
        return Ok(AuthMaterial::Xoauth2(xoauth2_ir(user, &token)));
    }
    Ok(AuthMaterial::Password(crate::imap::resolve_password(user)?))
}

/// `resolve_auth_for` on the account's own IMAP user.
pub async fn resolve_auth(cfg: &crate::ImapConfig) -> Result<AuthMaterial> {
    resolve_auth_for(cfg.oauth.as_ref(), &cfg.user).await
}

/// SMTP-side auth view — the send path wants Plain(user,pass) or the
/// b64'd XOAUTH2 string, or no auth at all for open relays.
pub fn smtp_auth(m: AuthMaterial) -> SmtpAuth {
    match m {
        AuthMaterial::Password(p) => SmtpAuth::Plain(p),
        AuthMaterial::Xoauth2(ir) => {
            SmtpAuth::Xoauth2(base64::engine::general_purpose::STANDARD.encode(ir))
        }
    }
}

pub enum SmtpAuth {
    /// No AUTH command (open relay / localhost test server).
    None,
    /// AUTH PLAIN — the resolved app password.
    Plain(String),
    /// AUTH XOAUTH2 — already base64-encoded SASL-IR.
    Xoauth2(String),
}
