//! OAuth2 device-authorization flow (RFC 8628) — generic across IdPs.
//!
//! Device flow over loopback-redirect: the CLI runs headless and on
//! machines where spawning a browser + localhost listener is awkward.
//! Provider presets:
//! - Google: `https://oauth2.googleapis.com/device/code` +
//!   `.../token`.
//! - Microsoft: `.../oauth2/v2.0/devicecode` on a tenant (default
//!   `common`).
//! - `custom`: explicit `device_url`/`token_url`/`scopes`.
//!
//! Extracted from the email connector so app-level OAuth ("add Google
//! login to this app") shares one implementation. Callers own secret
//! storage — this crate never touches the keystore.

use pai_core::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// OAuth client config — `provider` selects the endpoint preset;
/// `device_url`/`token_url`/`scopes` override for custom IdPs.
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
                "unknown oauth provider {other:?} — set token_url for a custom IdP"
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

/// One token-poll attempt's outcome — the caller loops on `Pending`.
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

/// Step 1: request a device code. The caller prints `user_code` +
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
            "oauth refresh: {} — re-run the oauth configure flow",
            json_err(&body)
        )));
    }
    let set: TokenSet = serde_json::from_value(body)
        .map_err(|e| Error::Provider(format!("oauth refresh response: {e}")))?;
    Ok((set.access_token, set.refresh_token))
}
