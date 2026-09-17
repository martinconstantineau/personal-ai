//! REST adapter — `GitLabProvider` over the GitLab API v4.
//!
//! Deliberately boring: each op is one reqwest call against
//! `{host}/api/v4`, mirroring the email connector's short-lived session
//! posture. Tokens resolve per call — PAT from `PAI_GITLAB_TOKEN` /
//! keystore (`PRIVATE-TOKEN` header) or a refreshed OAuth Bearer token.

use crate::{
    CommentTarget, GitLabConfig, GitLabProvider, Issue, IssueQuery, IssueSummary, MergeRequest,
    MrQuery, MrSummary, NewIssue, NewMr, Note, Pipeline, ProjectSummary,
};
use async_trait::async_trait;
use pai_core::*;
use serde_json::json;

pub struct RestGitLab {
    cfg: GitLabConfig,
    http: reqwest::Client,
}

/// What a request authenticates with — resolved per call.
enum Auth {
    /// `PRIVATE-TOKEN: <pat>` — personal/project access token.
    Pat(String),
    /// `Authorization: Bearer <access>` — OAuth device-flow tokens.
    Bearer(String),
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Provider(format!("gitlab http: {e}")))
}

/// Percent-encode one path segment — GitLab wants `group/sub` as
/// `group%2Fsub` and file paths likewise.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `author` is a user object upstream; we keep the username.
fn author_name(v: &serde_json::Value) -> String {
    v["author"]["username"]
        .as_str()
        .or_else(|| v["author"]["name"].as_str())
        .unwrap_or("")
        .to_string()
}

fn ts(v: &serde_json::Value, key: &str) -> Timestamp {
    v[key]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc))
        .unwrap_or_else(now)
}

impl RestGitLab {
    pub fn new(cfg: GitLabConfig) -> Self {
        Self {
            cfg,
            http: client().expect("reqwest client"),
        }
    }

    fn api(&self, path: &str) -> String {
        format!("{}/api/v4{}", self.cfg.normalized_host(), path)
    }

    /// Resolve the project for a call: explicit arg wins, else the
    /// configured default. Errors when neither exists.
    fn project(&self, project: Option<&str>) -> Result<String> {
        project
            .map(str::to_string)
            .or_else(|| self.cfg.project.clone())
            .filter(|p| !p.trim().is_empty())
            .ok_or_else(|| {
                Error::InvalidInput(
                    "no project — pass one or set a default via `pai gitlab configure`".into(),
                )
            })
    }

    fn proj_path(&self, project: Option<&str>, suffix: &str) -> Result<String> {
        let p = self.project(project)?;
        Ok(format!("/projects/{}{}", enc(&p), suffix))
    }

    async fn auth(&self) -> Result<Auth> {
        match &self.cfg.oauth {
            Some(oc) => Ok(Auth::Bearer(
                crate::access_token(oc, &self.cfg.normalized_host()).await?,
            )),
            None => Ok(Auth::Pat(crate::resolve_token(
                &self.cfg.normalized_host(),
            )?)),
        }
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder, auth: &Auth) -> reqwest::RequestBuilder {
        match auth {
            Auth::Pat(t) => req.header("PRIVATE-TOKEN", t),
            Auth::Bearer(t) => req.bearer_auth(t),
        }
    }

    /// GET json array/object; non-2xx → Provider error with GitLab's
    /// `message`/`error` field when present.
    async fn get(&self, url: String, query: &[(&str, String)]) -> Result<serde_json::Value> {
        let auth = self.auth().await?;
        let resp = self
            .apply_auth(self.http.get(&url), &auth)
            .query(query)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("gitlab http: {e}")))?;
        self.decode(resp).await
    }

    async fn post(&self, url: String, body: &serde_json::Value) -> Result<serde_json::Value> {
        let auth = self.auth().await?;
        let resp = self
            .apply_auth(self.http.post(&url), &auth)
            .json(body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("gitlab http: {e}")))?;
        self.decode(resp).await
    }

    async fn put(&self, url: String, body: &serde_json::Value) -> Result<serde_json::Value> {
        let auth = self.auth().await?;
        let resp = self
            .apply_auth(self.http.put(&url), &auth)
            .json(body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("gitlab http: {e}")))?;
        self.decode(resp).await
    }

    async fn decode(&self, resp: reqwest::Response) -> Result<serde_json::Value> {
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Provider(format!("gitlab response: {e}")))?;
        if !(200..300).contains(&status) {
            let body: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            // `message` is usually a string but can be {"field": ["err"]}.
            let msg = body["message"]
                .as_str()
                .or_else(|| body["message"].as_object().map(|_| "validation failed"))
                .or_else(|| body["error_description"].as_str())
                .or_else(|| body["error"].as_str())
                .unwrap_or(if text.is_empty() {
                    "request failed"
                } else {
                    &text
                });
            return Err(Error::Provider(format!("gitlab {status}: {msg}")));
        }
        Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
    }

    fn issue_summary(v: &serde_json::Value, project: &str) -> IssueSummary {
        IssueSummary {
            iid: v["iid"].as_u64().unwrap_or(0),
            id: v["id"].as_u64().unwrap_or(0),
            title: v["title"].as_str().unwrap_or("").into(),
            state: v["state"].as_str().unwrap_or("").into(),
            author: author_name(v),
            labels: v["labels"]
                .as_array()
                .map(|l| {
                    l.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            created_at: ts(v, "created_at"),
            updated_at: ts(v, "updated_at"),
            web_url: v["web_url"].as_str().unwrap_or("").into(),
            project: project.into(),
        }
    }

    fn mr_summary(v: &serde_json::Value, project: &str) -> MrSummary {
        MrSummary {
            iid: v["iid"].as_u64().unwrap_or(0),
            id: v["id"].as_u64().unwrap_or(0),
            title: v["title"].as_str().unwrap_or("").into(),
            state: v["state"].as_str().unwrap_or("").into(),
            draft: v["draft"].as_bool().unwrap_or(false)
                || v["work_in_progress"].as_bool().unwrap_or(false),
            author: author_name(v),
            source_branch: v["source_branch"].as_str().unwrap_or("").into(),
            target_branch: v["target_branch"].as_str().unwrap_or("").into(),
            created_at: ts(v, "created_at"),
            updated_at: ts(v, "updated_at"),
            web_url: v["web_url"].as_str().unwrap_or("").into(),
            sha: v["sha"].as_str().map(String::from),
            merge_status: v["merge_status"].as_str().map(String::from),
            project: project.into(),
        }
    }

    fn pipeline(v: &serde_json::Value, project: &str) -> Pipeline {
        Pipeline {
            id: v["id"].as_u64().unwrap_or(0),
            status: v["status"].as_str().unwrap_or("").into(),
            git_ref: v["ref"].as_str().unwrap_or("").into(),
            sha: v["sha"].as_str().map(String::from),
            created_at: ts(v, "created_at"),
            web_url: v["web_url"].as_str().unwrap_or("").into(),
            project: project.into(),
        }
    }

    /// The project's default branch — `create_merge_request` falls back
    /// to it when the caller doesn't name a target.
    async fn default_branch(&self, project: Option<&str>) -> Result<String> {
        let path = self.proj_path(project, "")?;
        let v = self.get(self.api(&path), &[]).await?;
        v["default_branch"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| Error::Provider("gitlab: no default_branch in project".into()))
    }
}

#[async_trait]
impl GitLabProvider for RestGitLab {
    fn id(&self) -> &'static str {
        "gitlab-rest"
    }

    async fn whoami(&self) -> Result<String> {
        let v = self.get(self.api("/user"), &[]).await?;
        v["username"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| Error::Provider("gitlab: no username in /user".into()))
    }

    async fn projects(&self, search: Option<&str>, limit: u32) -> Result<Vec<ProjectSummary>> {
        let mut q: Vec<(&str, String)> = vec![
            ("membership", "true".into()),
            ("order_by", "last_activity_at".into()),
            ("per_page", limit.clamp(1, 100).to_string()),
        ];
        if let Some(s) = search {
            q.push(("search", s.to_string()));
        }
        let v = self.get(self.api("/projects"), &q).await?;
        let rows = v.as_array().cloned().unwrap_or_default();
        Ok(rows
            .iter()
            .map(|p| ProjectSummary {
                id: p["id"].as_u64().unwrap_or(0),
                name: p["name"].as_str().unwrap_or("").into(),
                path_with_namespace: p["path_with_namespace"].as_str().unwrap_or("").into(),
                default_branch: p["default_branch"].as_str().map(String::from),
                web_url: p["web_url"].as_str().unwrap_or("").into(),
                last_activity_at: p["last_activity_at"]
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.with_timezone(&chrono::Utc)),
            })
            .collect())
    }

    async fn issues(&self, q: &IssueQuery) -> Result<Vec<IssueSummary>> {
        let project = self.project(q.project.as_deref())?;
        let mut query: Vec<(&str, String)> = vec![
            ("state", q.state.clone().unwrap_or_else(|| "opened".into())),
            ("per_page", q.limit.clamp(1, 100).max(20).to_string()),
        ];
        if let Some(s) = &q.search {
            query.push(("search", s.clone()));
        }
        if !q.labels.is_empty() {
            query.push(("labels", q.labels.join(",")));
        }
        let path = format!("/projects/{}/issues", enc(&project));
        let v = self.get(self.api(&path), &query).await?;
        Ok(v.as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|i| Self::issue_summary(i, &project))
            .collect())
    }

    async fn issue(&self, iid: u64, project: Option<&str>) -> Result<Issue> {
        let project = self.project(project)?;
        let path = format!("/projects/{}/issues/{iid}", enc(&project));
        let v = self.get(self.api(&path), &[]).await?;
        Ok(Issue {
            summary: Self::issue_summary(&v, &project),
            description: v["description"].as_str().map(String::from),
            user_notes_count: v["user_notes_count"].as_u64().unwrap_or(0),
        })
    }

    async fn create_issue(&self, new: &NewIssue) -> Result<IssueSummary> {
        let project = self.project(new.project.as_deref())?;
        let path = format!("/projects/{}/issues", enc(&project));
        let v = self
            .post(
                self.api(&path),
                &json!({
                    "title": new.title,
                    "description": new.description,
                    "labels": new.labels.join(","),
                }),
            )
            .await?;
        Ok(Self::issue_summary(&v, &project))
    }

    async fn comment(
        &self,
        target: CommentTarget,
        body: &str,
        project: Option<&str>,
    ) -> Result<Note> {
        let project = self.project(project)?;
        let parent = match target {
            CommentTarget::Issue(iid) => format!("issues/{iid}/notes"),
            CommentTarget::MergeRequest(iid) => format!("merge_requests/{iid}/notes"),
        };
        let path = format!("/projects/{}/{parent}", enc(&project));
        let v = self.post(self.api(&path), &json!({"body": body})).await?;
        Ok(Note {
            id: v["id"].as_u64().unwrap_or(0),
            body: v["body"].as_str().unwrap_or("").into(),
            author: author_name(&v),
            created_at: ts(&v, "created_at"),
        })
    }

    async fn merge_requests(&self, q: &MrQuery) -> Result<Vec<MrSummary>> {
        let project = self.project(q.project.as_deref())?;
        let mut query: Vec<(&str, String)> = vec![
            ("state", q.state.clone().unwrap_or_else(|| "opened".into())),
            ("per_page", q.limit.clamp(1, 100).max(20).to_string()),
        ];
        if let Some(s) = &q.search {
            query.push(("search", s.clone()));
        }
        let path = format!("/projects/{}/merge_requests", enc(&project));
        let v = self.get(self.api(&path), &query).await?;
        Ok(v.as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|m| Self::mr_summary(m, &project))
            .collect())
    }

    async fn merge_request(&self, iid: u64, project: Option<&str>) -> Result<MergeRequest> {
        let project = self.project(project)?;
        let path = format!("/projects/{}/merge_requests/{iid}", enc(&project));
        let v = self.get(self.api(&path), &[]).await?;
        Ok(MergeRequest {
            summary: Self::mr_summary(&v, &project),
            description: v["description"].as_str().map(String::from),
            head_pipeline_status: v["head_pipeline"]["status"].as_str().map(String::from),
            user_notes_count: v["user_notes_count"].as_u64().unwrap_or(0),
        })
    }

    async fn create_merge_request(&self, new: &NewMr) -> Result<MrSummary> {
        let project = self.project(new.project.as_deref())?;
        let target = match &new.target_branch {
            Some(b) if !b.trim().is_empty() => b.clone(),
            _ => self.default_branch(Some(&project)).await?,
        };
        let path = format!("/projects/{}/merge_requests", enc(&project));
        let v = self
            .post(
                self.api(&path),
                &json!({
                    "source_branch": new.source_branch,
                    "target_branch": target,
                    "title": new.title,
                    "description": new.description,
                }),
            )
            .await?;
        Ok(Self::mr_summary(&v, &project))
    }

    async fn merge(&self, iid: u64, project: Option<&str>) -> Result<MrSummary> {
        let project = self.project(project)?;
        let path = format!("/projects/{}/merge_requests/{iid}/merge", enc(&project));
        let v = self.put(self.api(&path), &json!({})).await?;
        Ok(Self::mr_summary(&v, &project))
    }

    async fn pipelines(&self, limit: u32, project: Option<&str>) -> Result<Vec<Pipeline>> {
        let project = self.project(project)?;
        let path = format!("/projects/{}/pipelines", enc(&project));
        let q: Vec<(&str, String)> = vec![("per_page", limit.clamp(1, 100).max(20).to_string())];
        let v = self.get(self.api(&path), &q).await?;
        Ok(v.as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|p| Self::pipeline(p, &project))
            .collect())
    }

    async fn trigger_pipeline(&self, git_ref: &str, project: Option<&str>) -> Result<Pipeline> {
        let project = self.project(project)?;
        let path = format!("/projects/{}/pipeline", enc(&project));
        let v = self.post(self.api(&path), &json!({"ref": git_ref})).await?;
        Ok(Self::pipeline(&v, &project))
    }

    async fn repo_file(&self, path: &str, git_ref: &str, project: Option<&str>) -> Result<String> {
        let project = self.project(project)?;
        let url = self.api(&format!(
            "/projects/{}/repository/files/{}/raw",
            enc(&project),
            enc(path)
        ));
        let auth = self.auth().await?;
        let resp = self
            .apply_auth(self.http.get(&url), &auth)
            .query(&[("ref", git_ref)])
            .send()
            .await
            .map_err(|e| Error::Provider(format!("gitlab http: {e}")))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Provider(format!("gitlab response: {e}")))?;
        if !(200..300).contains(&status) {
            return Err(Error::Provider(format!("gitlab {status}: {text}")));
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn encodes_project_paths() {
        assert_eq!(super::enc("group/sub repo"), "group%2Fsub%20repo");
        assert_eq!(super::enc("plain.name_1"), "plain.name_1");
    }
}
