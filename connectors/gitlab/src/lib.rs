//! GitLab connector interface — REST API v4.
//!
//! `RestGitLab` is the reference adapter; the core only ever sees the
//! `GitLabProvider` trait. Adapters map their API onto these types and
//! declare which `Permission` each op needs.
//!
//! Auth: a personal access token (`api` scope) in the OS keystore at
//! `gitlab:<host>` — never in `gitlab.json` — or an OAuth device-flow
//! refresh token at `gitlab-oauth:<host>` (GitLab 17.9+). `PAI_GITLAB_TOKEN`
//! wins when set, so CI and keystore-less runs still work.

pub mod rest;
pub use pai_oauth::OAuthConfig;
pub use rest::RestGitLab;

use async_trait::async_trait;
use pai_core::*;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// One GitLab instance binding. Persisted as `<data_dir>/gitlab.json`;
/// the token is never in it — see [`resolve_token`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabConfig {
    /// Instance root, e.g. `https://gitlab.com` or a self-managed URL.
    pub host: String,
    /// Default project (`group/sub/repo` or numeric id) every project-
    /// scoped op falls back to when the caller doesn't pass one.
    #[serde(default)]
    pub project: Option<String>,
    /// OAuth2 device-flow auth — when present, requests authenticate
    /// with a refreshed Bearer token instead of a PAT.
    #[serde(default)]
    pub oauth: Option<OAuthConfig>,
}

impl GitLabConfig {
    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        let f = data_dir.join("gitlab.json");
        if !f.exists() {
            return Ok(None);
        }
        let raw = std::fs::read(&f).map_err(|e| Error::Storage(e.to_string()))?;
        let cfg = serde_json::from_slice(&raw)
            .map_err(|e| Error::InvalidInput(format!("bad gitlab.json: {e}")))?;
        Ok(Some(cfg))
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(data_dir).map_err(|e| Error::Storage(e.to_string()))?;
        let raw = serde_json::to_vec_pretty(self).map_err(|e| Error::Storage(e.to_string()))?;
        std::fs::write(data_dir.join("gitlab.json"), raw).map_err(|e| Error::Storage(e.to_string()))
    }

    /// Host with the scheme normalized: bare `gitlab.com` becomes
    /// `https://gitlab.com`; trailing slashes are stripped.
    pub fn normalized_host(&self) -> String {
        let h = self.host.trim().trim_end_matches('/');
        if h.starts_with("http://") || h.starts_with("https://") {
            h.to_string()
        } else {
            format!("https://{h}")
        }
    }
}

/// Token persistence: `gitlab:<host>` — keyed on host so a PAT for
/// gitlab.com and a self-managed instance can coexist.
fn token_key(host: &str) -> String {
    format!("gitlab:{host}")
}

fn oauth_key(host: &str) -> String {
    format!("gitlab-oauth:{host}")
}

/// Persist a personal access token in the OS keystore.
pub fn store_token(host: &str, token: &str) -> bool {
    pai_identity::keystore::store(&token_key(host), token.as_bytes())
}

/// PAT resolution: `PAI_GITLAB_TOKEN` env → OS keystore `gitlab:<host>`.
pub fn resolve_token(host: &str) -> Result<String> {
    if let Ok(t) = std::env::var("PAI_GITLAB_TOKEN") {
        if !t.is_empty() {
            return Ok(t);
        }
    }
    pai_identity::keystore::load(&token_key(host))
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| {
            Error::InvalidInput(
                "no gitlab token — set PAI_GITLAB_TOKEN or run `pai gitlab configure`".into(),
            )
        })
}

/// Persist the OAuth refresh token (the only durable OAuth secret).
pub fn store_refresh_token(host: &str, refresh_token: &str) -> bool {
    pai_identity::keystore::store(&oauth_key(host), refresh_token.as_bytes())
}

/// Whether an OAuth refresh token is stored for `host`.
pub fn has_refresh_token(host: &str) -> bool {
    pai_identity::keystore::load(&oauth_key(host)).is_some()
}

/// OAuth access-token resolution: refresh token from the keystore →
/// refresh grant → transparent rotation re-store (same contract as the
/// email connector).
pub async fn access_token(cfg: &OAuthConfig, host: &str) -> Result<String> {
    let refresh = pai_identity::keystore::load(&oauth_key(host))
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| {
            Error::InvalidInput(
                "no gitlab oauth refresh token — run `pai gitlab configure --oauth`".into(),
            )
        })?;
    let (access, rotated) = pai_oauth::refresh_access_token(cfg, &refresh).await?;
    if let Some(r) = rotated {
        if !r.is_empty() && r != refresh {
            let _ = store_refresh_token(host, &r);
        }
    }
    Ok(access)
}

/// OAuth endpoints for a GitLab instance (device authorization grant —
/// GA since GitLab 17.9; older instances should use a PAT).
pub fn oauth_config(host: &str, client_id: &str) -> OAuthConfig {
    let host = host.trim().trim_end_matches('/');
    OAuthConfig {
        provider: "custom".into(),
        client_id: client_id.to_string(),
        tenant: None,
        device_url: Some(format!("{host}/oauth/authorize_device")),
        token_url: Some(format!("{host}/oauth/token")),
        scopes: Some(vec!["api".into()]),
    }
}

// ---------------------------------------------------------------------------
// Types — field names mirror GitLab's REST API so tool output reads like the
// upstream docs.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub id: u64,
    pub name: String,
    /// `group/sub/repo` — the canonical human-readable path.
    pub path_with_namespace: String,
    #[serde(default)]
    pub default_branch: Option<String>,
    pub web_url: String,
    #[serde(default)]
    pub last_activity_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueSummary {
    /// Project-scoped number shown in the UI (#42).
    pub iid: u64,
    pub id: u64,
    pub title: String,
    /// `opened` | `closed`.
    pub state: String,
    pub author: String,
    #[serde(default)]
    pub labels: Vec<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub web_url: String,
    /// The project this issue was read from.
    pub project: String,
}

/// A full issue body is always `TrustLevel::Untrusted` — description text
/// is data to summarize/act on, never instructions to obey.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    #[serde(flatten)]
    pub summary: IssueSummary,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub user_notes_count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IssueQuery {
    /// Project override — falls back to the configured default.
    #[serde(default)]
    pub project: Option<String>,
    /// `opened` | `closed` | `all` (default `opened`).
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewIssue {
    #[serde(default)]
    pub project: Option<String>,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MrSummary {
    pub iid: u64,
    pub id: u64,
    pub title: String,
    /// `opened` | `closed` | `merged` | `locked`.
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    pub author: String,
    pub source_branch: String,
    pub target_branch: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub web_url: String,
    #[serde(default)]
    pub sha: Option<String>,
    #[serde(default)]
    pub merge_status: Option<String>,
    pub project: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeRequest {
    #[serde(flatten)]
    pub summary: MrSummary,
    #[serde(default)]
    pub description: Option<String>,
    /// Head pipeline status when GitLab reports one.
    #[serde(default)]
    pub head_pipeline_status: Option<String>,
    #[serde(default)]
    pub user_notes_count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MrQuery {
    #[serde(default)]
    pub project: Option<String>,
    /// `opened` | `closed` | `merged` | `all` (default `opened`).
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMr {
    #[serde(default)]
    pub project: Option<String>,
    pub source_branch: String,
    /// Defaults to the project's default branch when omitted.
    #[serde(default)]
    pub target_branch: Option<String>,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pipeline {
    pub id: u64,
    /// `created` | `running` | `success` | `failed` | `canceled` | ...
    pub status: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    #[serde(default)]
    pub sha: Option<String>,
    pub created_at: Timestamp,
    pub web_url: String,
    pub project: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: u64,
    pub body: String,
    pub author: String,
    pub created_at: Timestamp,
}

/// What `comment` posts to — issue threads and MR threads are the same
/// Notes API under different parents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentTarget {
    Issue(u64),
    MergeRequest(u64),
}

/// Vendor-shaped GitLab operations. Each method's required permission is
/// documented next to it — the tool layer enforces before dispatch.
#[async_trait]
pub trait GitLabProvider: Send + Sync {
    fn id(&self) -> &'static str;

    /// GITLAB_READ — token sanity check; returns the account username.
    async fn whoami(&self) -> Result<String>;
    /// GITLAB_READ — visible projects (for onboarding/search).
    async fn projects(&self, search: Option<&str>, limit: u32) -> Result<Vec<ProjectSummary>>;
    /// GITLAB_READ
    async fn issues(&self, q: &IssueQuery) -> Result<Vec<IssueSummary>>;
    /// GITLAB_READ — `project: None` uses the configured default.
    async fn issue(&self, iid: u64, project: Option<&str>) -> Result<Issue>;
    /// GITLAB_WRITE
    async fn create_issue(&self, new: &NewIssue) -> Result<IssueSummary>;
    /// GITLAB_WRITE — comment on an issue or MR thread.
    async fn comment(
        &self,
        target: CommentTarget,
        body: &str,
        project: Option<&str>,
    ) -> Result<Note>;
    /// GITLAB_READ
    async fn merge_requests(&self, q: &MrQuery) -> Result<Vec<MrSummary>>;
    /// GITLAB_READ
    async fn merge_request(&self, iid: u64, project: Option<&str>) -> Result<MergeRequest>;
    /// GITLAB_WRITE
    async fn create_merge_request(&self, new: &NewMr) -> Result<MrSummary>;
    /// GITLAB_MERGE — merges immediately; approval-gated by default.
    async fn merge(&self, iid: u64, project: Option<&str>) -> Result<MrSummary>;
    /// GITLAB_READ
    async fn pipelines(&self, limit: u32, project: Option<&str>) -> Result<Vec<Pipeline>>;
    /// GITLAB_WRITE — run CI for a ref.
    async fn trigger_pipeline(&self, git_ref: &str, project: Option<&str>) -> Result<Pipeline>;
    /// GITLAB_READ — file content is untrusted data.
    async fn repo_file(&self, path: &str, git_ref: &str, project: Option<&str>) -> Result<String>;
}
