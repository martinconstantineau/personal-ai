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

/// Re-exported from `pai-oauth` — the device-flow machinery is generic
/// now that app-level OAuth shares it; `crate::oauth::OAuthConfig`
/// etc. keep resolving.
pub use pai_oauth::{
    device_flow, poll_token_once, refresh_access_token, DeviceGrant, OAuthConfig, Poll, TokenSet,
};

// ---------------------------------------------------------------------------
// Keystore + SASL-IR
// ---------------------------------------------------------------------------

/// Refresh tokens are scoped to the host they authenticate against —
/// `email-oauth:<user>@<host>` — so repointing `email.json` at another
/// server can't pull the old host's grant. Entries under the legacy
/// `email-oauth:<user>` key are migrated on first read.
fn oauth_key(user: &str, host: &str) -> String {
    format!("email-oauth:{user}@{}", host.trim().to_ascii_lowercase())
}

/// Persist the refresh token (the only durable OAuth secret).
pub fn store_refresh_token(user: &str, host: &str, refresh_token: &str) -> bool {
    let ok = pai_identity::keystore::store(&oauth_key(user, host), refresh_token.as_bytes());
    if ok {
        pai_identity::keystore::delete(&format!("email-oauth:{user}"));
    }
    ok
}

/// Whether a refresh token exists for `user@host` (legacy key counts).
pub fn has_refresh_token(user: &str, host: &str) -> bool {
    pai_identity::keystore::load(&oauth_key(user, host)).is_some()
        || pai_identity::keystore::load(&format!("email-oauth:{user}")).is_some()
}

/// Resolve an access token: refresh token from the keystore → refresh
/// grant → transparent rotation re-store.
pub async fn access_token(cfg: &OAuthConfig, user: &str, host: &str) -> Result<String> {
    let key = oauth_key(user, host);
    let legacy = format!("email-oauth:{user}");
    let stored = pai_identity::keystore::load(&key).or_else(|| {
        let b = pai_identity::keystore::load(&legacy)?;
        if pai_identity::keystore::store(&key, &b) {
            pai_identity::keystore::delete(&legacy);
        }
        Some(b)
    });
    let refresh = stored
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| {
            Error::InvalidInput(
                "no oauth refresh token — run `pai email configure --oauth <provider>`".into(),
            )
        })?;
    let (access, rotated) = refresh_access_token(cfg, &refresh).await?;
    if let Some(r) = rotated {
        if !r.is_empty() && r != refresh {
            let _ = store_refresh_token(user, host, &r);
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

/// Pick the credential for one session under `user` at `host`: OAuth
/// config wins, else the `email:<user>@<host>` password entry.
pub async fn resolve_auth_for(
    oauth: Option<&OAuthConfig>,
    user: &str,
    host: &str,
) -> Result<AuthMaterial> {
    if let Some(oauth) = oauth {
        let token = access_token(oauth, user, host).await?;
        return Ok(AuthMaterial::Xoauth2(xoauth2_ir(user, &token)));
    }
    Ok(AuthMaterial::Password(crate::imap::resolve_password(
        user, host,
    )?))
}

/// `resolve_auth_for` on the account's own IMAP user and host.
pub async fn resolve_auth(cfg: &crate::ImapConfig) -> Result<AuthMaterial> {
    resolve_auth_for(cfg.oauth.as_ref(), &cfg.user, &cfg.host).await
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
