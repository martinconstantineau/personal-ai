//! V5s: GitLab connector — REST adapter against a mock API server, tool
//! gating, config lifecycle, and `pai_gitlab_configure` FFI hot-swap.
//!
//! `PAI_GITLAB_TOKEN` supplies auth so no keystore is needed;
//! `PAI_KEYSTORE_OFF` keeps the FFI tests out of the OS credential vault.

use pai_connector_gitlab::*;
use pai_core::*;
use pai_ffi::*;
use pai_permissions::{Permission, PolicyTable, RiskLevel};
use pai_tools::{Tool, ToolContext};
use std::ffi::{CStr, CString};

use std::sync::Mutex;

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v5s-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

// --- mock provider through the tool layer ---------------------------------

struct MockGitLab {
    merged: Mutex<Vec<u64>>,
    notes: Mutex<Vec<String>>,
}

impl MockGitLab {
    fn issue(iid: u64, title: &str) -> IssueSummary {
        IssueSummary {
            iid,
            id: iid * 10,
            title: title.into(),
            state: "opened".into(),
            author: "alice".into(),
            labels: vec!["bug".into()],
            created_at: now(),
            updated_at: now(),
            web_url: format!("https://gl.test/p/r/-/issues/{iid}"),
            project: "p/r".into(),
        }
    }
    fn mr(iid: u64, title: &str) -> MrSummary {
        MrSummary {
            iid,
            id: iid * 10,
            title: title.into(),
            state: "opened".into(),
            draft: false,
            author: "alice".into(),
            source_branch: "feat".into(),
            target_branch: "main".into(),
            created_at: now(),
            updated_at: now(),
            web_url: format!("https://gl.test/p/r/-/merge_requests/{iid}"),
            sha: Some("abc123".into()),
            merge_status: Some("can_be_merged".into()),
            project: "p/r".into(),
        }
    }
}

#[async_trait::async_trait]
impl GitLabProvider for MockGitLab {
    fn id(&self) -> &'static str {
        "mock"
    }
    async fn whoami(&self) -> Result<String> {
        Ok("marti".into())
    }
    async fn projects(&self, _s: Option<&str>, _l: u32) -> Result<Vec<ProjectSummary>> {
        Ok(vec![])
    }
    async fn issues(&self, q: &IssueQuery) -> Result<Vec<IssueSummary>> {
        let all = vec![Self::issue(7, "flaky test"), Self::issue(9, "docs update")];
        Ok(match &q.search {
            Some(n) => all
                .into_iter()
                .filter(|i| i.title.contains(n.as_str()))
                .collect(),
            None => all,
        })
    }
    async fn issue(&self, iid: u64, _p: Option<&str>) -> Result<Issue> {
        Ok(Issue {
            summary: Self::issue(iid, "flaky test"),
            description: Some("fails on CI runners".into()),
            user_notes_count: 2,
        })
    }
    async fn create_issue(&self, new: &NewIssue) -> Result<IssueSummary> {
        Ok(Self::issue(42, &new.title))
    }
    async fn comment(&self, _t: CommentTarget, body: &str, _p: Option<&str>) -> Result<Note> {
        self.notes.lock().unwrap().push(body.into());
        Ok(Note {
            id: 1,
            body: body.into(),
            author: "marti".into(),
            created_at: now(),
        })
    }
    async fn merge_requests(&self, _q: &MrQuery) -> Result<Vec<MrSummary>> {
        Ok(vec![Self::mr(3, "wire sync")])
    }
    async fn merge_request(&self, iid: u64, _p: Option<&str>) -> Result<MergeRequest> {
        Ok(MergeRequest {
            summary: Self::mr(iid, "wire sync"),
            description: None,
            head_pipeline_status: Some("success".into()),
            user_notes_count: 0,
        })
    }
    async fn create_merge_request(&self, new: &NewMr) -> Result<MrSummary> {
        Ok(Self::mr(11, &new.title))
    }
    async fn merge(&self, iid: u64, _p: Option<&str>) -> Result<MrSummary> {
        self.merged.lock().unwrap().push(iid);
        let mut m = Self::mr(iid, "wire sync");
        m.state = "merged".into();
        Ok(m)
    }
    async fn pipelines(&self, _l: u32, _p: Option<&str>) -> Result<Vec<Pipeline>> {
        Ok(vec![Pipeline {
            id: 900,
            status: "success".into(),
            git_ref: "main".into(),
            sha: Some("abc".into()),
            created_at: now(),
            web_url: "https://gl.test/p/r/-/pipelines/900".into(),
            project: "p/r".into(),
        }])
    }
    async fn trigger_pipeline(&self, r: &str, _p: Option<&str>) -> Result<Pipeline> {
        Ok(Pipeline {
            id: 901,
            status: "created".into(),
            git_ref: r.into(),
            sha: None,
            created_at: now(),
            web_url: String::new(),
            project: "p/r".into(),
        })
    }
    async fn repo_file(&self, path: &str, _r: &str, _p: Option<&str>) -> Result<String> {
        Ok(format!("// {path} contents"))
    }
}

fn ctx<'a>(gl: Option<&'a dyn GitLabProvider>) -> ToolContext<'a> {
    ToolContext {
        run: AgentRunId::new(),
        device: DeviceId::new(),
        memory: None,
        memory_scope: None,
        documents: None,
        email: None,
        gitlab: gl,
        vision: None,
        notify: None,
        apps: None,
        audio_gen: None,
        media_dir: None,
        allowed_roots: &[],
    }
}

#[tokio::test]
async fn gitlab_tools_read_write_merge() {
    let gl = MockGitLab {
        merged: Default::default(),
        notes: Default::default(),
    };
    let c = ctx(Some(&gl));

    let out = pai_tools::GitLabIssuesTool
        .execute(serde_json::json!({"search": "flaky"}), &c)
        .await
        .unwrap();
    let rows = out.value["issues"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["iid"], 7);

    let out = pai_tools::GitLabIssueReadTool
        .execute(serde_json::json!({"iid": 7}), &c)
        .await
        .unwrap();
    assert_eq!(out.value["issue"]["description"], "fails on CI runners");

    let out = pai_tools::GitLabIssueCreateTool
        .execute(serde_json::json!({"title": "new bug"}), &c)
        .await
        .unwrap();
    assert_eq!(out.value["issue"]["iid"], 42);

    pai_tools::GitLabCommentTool
        .execute(
            serde_json::json!({"kind": "issue", "iid": 7, "body": "confirmed"}),
            &c,
        )
        .await
        .unwrap();
    assert_eq!(gl.notes.lock().unwrap().as_slice(), ["confirmed"]);

    let out = pai_tools::GitLabMrMergeTool
        .execute(serde_json::json!({"iid": 3}), &c)
        .await
        .unwrap();
    assert_eq!(out.value["merge_request"]["state"], "merged");
    assert_eq!(gl.merged.lock().unwrap().as_slice(), [3]);

    let out = pai_tools::GitLabPipelinesTool
        .execute(serde_json::json!({}), &c)
        .await
        .unwrap();
    assert_eq!(out.value["pipelines"][0]["status"], "success");

    let out = pai_tools::GitLabFileTool
        .execute(serde_json::json!({"path": "README.md"}), &c)
        .await
        .unwrap();
    assert!(out.value["content"].as_str().unwrap().contains("README.md"));
}

#[tokio::test]
async fn gitlab_tools_without_provider_error_cleanly() {
    let c = ctx(None);
    let err = pai_tools::GitLabIssuesTool
        .execute(serde_json::json!({}), &c)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("gitlab"));
}

#[test]
fn gitlab_tools_gate_on_permissions() {
    let reg = pai_tools::builtin_registry();
    let expect = [
        ("gitlab.projects", Permission::GitLabRead, RiskLevel::Low),
        ("gitlab.issues", Permission::GitLabRead, RiskLevel::Low),
        ("gitlab.issue_read", Permission::GitLabRead, RiskLevel::Low),
        (
            "gitlab.issue_create",
            Permission::GitLabWrite,
            RiskLevel::Medium,
        ),
        ("gitlab.comment", Permission::GitLabWrite, RiskLevel::Medium),
        ("gitlab.mrs", Permission::GitLabRead, RiskLevel::Low),
        ("gitlab.mr_read", Permission::GitLabRead, RiskLevel::Low),
        (
            "gitlab.mr_create",
            Permission::GitLabWrite,
            RiskLevel::Medium,
        ),
        ("gitlab.mr_merge", Permission::GitLabMerge, RiskLevel::High),
        ("gitlab.pipelines", Permission::GitLabRead, RiskLevel::Low),
        (
            "gitlab.pipeline_trigger",
            Permission::GitLabWrite,
            RiskLevel::Medium,
        ),
        ("gitlab.file_read", Permission::GitLabRead, RiskLevel::Low),
    ];
    for (name, perm, risk) in expect {
        let d = reg
            .descriptors()
            .into_iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("{name} not registered"));
        assert_eq!(d.required_permissions, vec![perm], "{name} permission");
        assert_eq!(d.risk, risk, "{name} risk");
    }
}

#[test]
fn gitlab_permissions_have_sensible_defaults() {
    let d = PolicyTable::with_defaults();
    use ExecutionPolicy::*;
    assert_eq!(d.get(Permission::GitLabRead), Some(AlwaysAllow));
    assert_eq!(d.get(Permission::GitLabWrite), Some(AskUser));
    assert_eq!(d.get(Permission::GitLabMerge), Some(AskUser));
}

#[test]
fn gitlab_config_roundtrip() {
    let dir = tmpdir("cfg");
    assert!(GitLabConfig::load(&dir).unwrap().is_none());
    let cfg = GitLabConfig {
        host: "gitlab.example.test".into(),
        project: Some("group/repo".into()),
        oauth: None,
    };
    cfg.save(&dir).unwrap();
    let back = GitLabConfig::load(&dir).unwrap().unwrap();
    assert_eq!(back.normalized_host(), "https://gitlab.example.test");
    assert_eq!(back.project.as_deref(), Some("group/repo"));
    // No token anywhere in the file — credentials are keystored.
    let raw = std::fs::read_to_string(dir.join("gitlab.json")).unwrap();
    assert!(!raw.contains("token"));
}

// --- REST adapter against a mock GitLab API --------------------------------

/// Spin a tiny_http server emulating GitLab API v4 for one project.
/// Returns the base URL. Asserts the PRIVATE-TOKEN header on every call.
fn gitlab_stub() -> String {
    let http = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = http.server_addr().to_string();
    std::thread::spawn(move || {
        for mut req in http.incoming_requests() {
            let url = req.url().to_string();
            let (path, _query) = match url.split_once('?') {
                Some((a, b)) => (a.to_string(), b.to_string()),
                None => (url, String::new()),
            };
            let method = req.method().as_str().to_string();
            let auth_ok = req.headers().iter().any(|h| {
                h.field
                    .as_str()
                    .as_str()
                    .eq_ignore_ascii_case("private-token")
                    && h.value.as_str() == "test-token"
            });
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            let resp = if !auth_ok {
                tiny_http::Response::from_data(br#"{"message":"401 Unauthorized"}"#.to_vec())
                    .with_status_code(401)
            } else {
                let v: serde_json::Value = match (method.as_str(), path.as_str()) {
                    ("GET", "/api/v4/user") => serde_json::json!({"username": "marti"}),
                    ("GET", "/api/v4/projects") => serde_json::json!([{
                        "id": 5, "name": "Repo", "path_with_namespace": "group/repo",
                        "default_branch": "main", "web_url": "https://gl.test/group/repo"
                    }]),
                    ("GET", "/api/v4/projects/group%2Frepo/issues") => serde_json::json!([{
                        "iid": 7, "id": 70, "title": "flaky test", "state": "opened",
                        "author": {"username": "alice"}, "labels": ["bug"],
                        "created_at": "2026-09-01T10:00:00Z",
                        "updated_at": "2026-09-02T10:00:00Z",
                        "web_url": "https://gl.test/group/repo/-/issues/7"
                    }]),
                    ("GET", "/api/v4/projects/group%2Frepo/issues/7") => serde_json::json!({
                        "iid": 7, "id": 70, "title": "flaky test", "state": "opened",
                        "author": {"username": "alice"}, "labels": ["bug"],
                        "created_at": "2026-09-01T10:00:00Z",
                        "updated_at": "2026-09-02T10:00:00Z",
                        "web_url": "https://gl.test/group/repo/-/issues/7",
                        "description": "fails on CI", "user_notes_count": 3
                    }),
                    ("POST", "/api/v4/projects/group%2Frepo/issues") => {
                        let sent: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_default();
                        serde_json::json!({
                            "iid": 42, "id": 420, "title": sent["title"],
                            "state": "opened", "author": {"username": "marti"},
                            "labels": [], "created_at": "2026-09-16T00:00:00Z",
                            "updated_at": "2026-09-16T00:00:00Z",
                            "web_url": "https://gl.test/group/repo/-/issues/42"
                        })
                    }
                    ("POST", "/api/v4/projects/group%2Frepo/issues/7/notes") => {
                        let sent: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_default();
                        serde_json::json!({
                            "id": 99, "body": sent["body"],
                            "author": {"username": "marti"},
                            "created_at": "2026-09-16T00:00:00Z"
                        })
                    }
                    ("GET", "/api/v4/projects/group%2Frepo/merge_requests") => {
                        serde_json::json!([{
                            "iid": 3, "id": 30, "title": "wire sync", "state": "opened",
                            "author": {"username": "alice"},
                            "source_branch": "feat", "target_branch": "main",
                            "created_at": "2026-09-01T10:00:00Z",
                            "updated_at": "2026-09-02T10:00:00Z",
                            "web_url": "https://gl.test/group/repo/-/merge_requests/3",
                            "sha": "abc", "merge_status": "can_be_merged", "draft": false
                        }])
                    }
                    ("PUT", "/api/v4/projects/group%2Frepo/merge_requests/3/merge") => {
                        serde_json::json!({
                            "iid": 3, "id": 30, "title": "wire sync", "state": "merged",
                            "author": {"username": "alice"},
                            "source_branch": "feat", "target_branch": "main",
                            "created_at": "2026-09-01T10:00:00Z",
                            "updated_at": "2026-09-16T00:00:00Z",
                            "web_url": "https://gl.test/group/repo/-/merge_requests/3"
                        })
                    }
                    ("GET", "/api/v4/projects/group%2Frepo/pipelines") => serde_json::json!([{
                        "id": 900, "status": "success", "ref": "main", "sha": "abc",
                        "created_at": "2026-09-10T10:00:00Z",
                        "web_url": "https://gl.test/group/repo/-/pipelines/900"
                    }]),
                    ("POST", "/api/v4/projects/group%2Frepo/pipeline") => {
                        let sent: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_default();
                        serde_json::json!({
                            "id": 901, "status": "created", "ref": sent["ref"],
                            "created_at": "2026-09-16T00:00:00Z",
                            "web_url": "https://gl.test/group/repo/-/pipelines/901"
                        })
                    }
                    ("GET", "/api/v4/projects/group%2Frepo/repository/files/README.md/raw") => {
                        let r = tiny_http::Response::from_data(b"hello readme".to_vec())
                            .with_status_code(200);
                        let _ = req.respond(r);
                        continue;
                    }
                    _ => {
                        let r = tiny_http::Response::from_data(
                            format!(r#"{{"message":"404 Not Found: {method} {path}"}}"#)
                                .into_bytes(),
                        )
                        .with_status_code(404);
                        let _ = req.respond(r);
                        continue;
                    }
                };
                tiny_http::Response::from_data(v.to_string().into_bytes()).with_status_code(200)
            };
            let _ = req.respond(resp);
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn rest_adapter_round_trips_gitlab_api() {
    std::env::set_var("PAI_GITLAB_TOKEN", "test-token");
    std::env::set_var("PAI_KEYSTORE_OFF", "1");
    let host = gitlab_stub();
    let gl = RestGitLab::new(GitLabConfig {
        host,
        project: Some("group/repo".into()),
        oauth: None,
    });

    assert_eq!(gl.whoami().await.unwrap(), "marti");

    let projects = gl.projects(None, 10).await.unwrap();
    assert_eq!(projects[0].path_with_namespace, "group/repo");

    let issues = gl.issues(&IssueQuery::default()).await.unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].iid, 7);
    assert_eq!(issues[0].author, "alice");
    assert_eq!(issues[0].project, "group/repo");

    let issue = gl.issue(7, None).await.unwrap();
    assert_eq!(issue.description.as_deref(), Some("fails on CI"));
    assert_eq!(issue.user_notes_count, 3);

    let created = gl
        .create_issue(&NewIssue {
            project: None,
            title: "from test".into(),
            description: None,
            labels: vec![],
        })
        .await
        .unwrap();
    assert_eq!(created.iid, 42);

    let note = gl
        .comment(CommentTarget::Issue(7), "looking", None)
        .await
        .unwrap();
    assert_eq!(note.id, 99);
    assert_eq!(note.body, "looking");

    let mrs = gl.merge_requests(&MrQuery::default()).await.unwrap();
    assert_eq!(mrs[0].iid, 3);
    assert_eq!(mrs[0].source_branch, "feat");

    let merged = gl.merge(3, None).await.unwrap();
    assert_eq!(merged.state, "merged");

    let pipes = gl.pipelines(10, None).await.unwrap();
    assert_eq!(pipes[0].status, "success");
    assert_eq!(pipes[0].git_ref, "main");

    let p = gl.trigger_pipeline("main", None).await.unwrap();
    assert_eq!(p.id, 901);

    let text = gl.repo_file("README.md", "main", None).await.unwrap();
    assert_eq!(text, "hello readme");

    // Missing project → the honest "no project" error, not a request.
    let no_default = RestGitLab::new(GitLabConfig {
        host: "http://127.0.0.1:1".into(),
        project: None,
        oauth: None,
    });
    let err = no_default.issues(&IssueQuery::default()).await.unwrap_err();
    assert!(err.to_string().contains("no project"), "{err}");
}

#[tokio::test]
async fn rest_adapter_surfaces_gitlab_errors() {
    std::env::set_var("PAI_GITLAB_TOKEN", "test-token");
    std::env::set_var("PAI_KEYSTORE_OFF", "1");
    let host = gitlab_stub();
    let gl = RestGitLab::new(GitLabConfig {
        host,
        project: Some("group/repo".into()),
        oauth: None,
    });
    // No route for issue 999 → 404 with GitLab's message field.
    let err = gl.issue(999, None).await.unwrap_err();
    assert!(err.to_string().contains("404"), "{err}");
}

// --- FFI: in-app configure hot-swap -----------------------------------------

unsafe fn init(dir: &std::path::Path) -> *mut PaiRuntime {
    let cfg = CString::new(
        serde_json::json!({
            "data_dir": dir.to_string_lossy(),
            "provider": "echo",
        })
        .to_string(),
    )
    .unwrap();
    let h = pai_init(cfg.as_ptr());
    assert!(!h.is_null(), "pai_init failed");
    h
}

unsafe fn jso(p: *mut std::ffi::c_char) -> serde_json::Value {
    assert!(!p.is_null());
    let v: serde_json::Value = serde_json::from_str(CStr::from_ptr(p).to_str().unwrap()).unwrap();
    pai_free_string(p);
    v
}

#[test]
fn configure_hot_swaps_provider() {
    std::env::set_var("PAI_KEYSTORE_OFF", "1");
    std::env::set_var("PAI_GITLAB_TOKEN", "test-token");
    unsafe {
        let dir = tmpdir("ffi");
        let h = init(&dir);

        // Before configuration the connector reports the honest error.
        let r = jso(pai_gitlab_issues(h, std::ptr::null()));
        assert_eq!(r["error"], "gitlab not configured (gitlab.json)");
        let r = jso(pai_gitlab_status(h));
        assert_eq!(r["configured"], false);

        // Configure with an unreachable host — config + credential writes
        // succeed; only live calls would hit the network.
        let cfg = CString::new(
            serde_json::json!({
                "host": "gitlab.invalid",
                "token": "glpat-x",
                "project": "group/repo",
            })
            .to_string(),
        )
        .unwrap();
        let r = jso(pai_gitlab_configure(h, cfg.as_ptr()));
        assert_eq!(r["configured"], true, "{r}");
        assert_eq!(r["project"], "group/repo");

        // gitlab.json landed — and the token is NOT in it.
        let raw = std::fs::read_to_string(dir.join("gitlab.json")).unwrap();
        let saved: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(saved["host"], "gitlab.invalid");
        assert!(!raw.contains("glpat-x"), "token must not be persisted");

        // The live provider is swapped — issues now fails with a
        // connection error, not "not configured".
        let r = jso(pai_gitlab_issues(h, std::ptr::null()));
        let err = r["error"].as_str().unwrap_or_default();
        assert!(r["error"].is_string(), "{r}");
        assert!(!err.contains("not configured"), "{err}");

        // Status reports the binding; env token counts as auth.
        let r = jso(pai_gitlab_status(h));
        assert_eq!(r["configured"], true);
        assert_eq!(r["host"], "https://gitlab.invalid");
        assert_eq!(r["auth"], "token");

        // A fresh init picks the config up from disk.
        let h2 = init(&dir);
        let r = jso(pai_gitlab_status(h2));
        assert_eq!(r["configured"], true);
        let r = jso(pai_gitlab_issues(h2, std::ptr::null()));
        let err = r["error"].as_str().unwrap_or_default();
        assert!(!err.contains("not configured"), "{err}");

        pai_free(h);
        pai_free(h2);
    }
}

#[test]
fn configure_validates_input() {
    std::env::set_var("PAI_KEYSTORE_OFF", "1");
    unsafe {
        let dir = tmpdir("bad");
        let h = init(&dir);

        // Missing host → error, nothing written.
        let cfg = CString::new(r#"{"host":"","project":"g/r"}"#).unwrap();
        let r = jso(pai_gitlab_configure(h, cfg.as_ptr()));
        assert!(r["error"].is_string(), "{r}");
        assert!(!dir.join("gitlab.json").exists());

        pai_free(h);
    }
}
