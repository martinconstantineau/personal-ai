//! `pai gitlab …` — GitLab REST v4 connector: configure (PAT or OAuth
//! device flow) plus issues/MRs/pipelines/repo-file operations.

use crate::util::{read_prompt, wait_device_grant};
use clap::Subcommand;
use pai_core::*;

#[derive(Subcommand)]
pub(crate) enum GitLabCmd {
    /// Configure the instance binding: writes gitlab.json; the personal
    /// access token goes to the OS keystore (`gitlab:<host>`), never the
    /// file. `--oauth <client_id>` uses the device-authorization flow
    /// (GitLab 17.9+) instead — a refresh token replaces the PAT.
    Configure {
        /// OAuth2 device flow — needs a client_id from a GitLab OAuth
        /// application (Admin → Applications, `api` scope).
        #[arg(long)]
        oauth: Option<String>,
    },
    /// Show the configured binding (never prints the token).
    Status,
    /// List projects the token can see (for finding the project path).
    Projects {
        search: Option<String>,
        #[arg(long, default_value = "20")]
        limit: u32,
    },
    /// List/search issues on the default (or --project) project.
    Issues {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        search: Option<String>,
        #[arg(long)]
        label: Vec<String>,
        #[arg(long, default_value = "20")]
        limit: u32,
    },
    /// Read one issue by iid.
    Issue {
        iid: u64,
        #[arg(long)]
        project: Option<String>,
    },
    /// Open a new issue.
    IssueNew {
        title: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        label: Vec<String>,
        #[arg(long)]
        project: Option<String>,
    },
    /// Comment on an issue or merge request.
    Comment {
        /// `issue` or `mr`.
        kind: String,
        iid: u64,
        body: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// List/search merge requests.
    Mrs {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        search: Option<String>,
        #[arg(long, default_value = "20")]
        limit: u32,
    },
    /// Read one merge request by iid.
    Mr {
        iid: u64,
        #[arg(long)]
        project: Option<String>,
    },
    /// Open a merge request (target defaults to the default branch).
    MrCreate {
        source_branch: String,
        title: String,
        #[arg(long)]
        target_branch: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        project: Option<String>,
    },
    /// Merge a merge request by iid.
    MrMerge {
        iid: u64,
        #[arg(long)]
        project: Option<String>,
    },
    /// List recent pipelines.
    Pipelines {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value = "20")]
        limit: u32,
    },
    /// Run a pipeline for a ref (branch/tag).
    PipelineRun {
        git_ref: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Print a repo file's contents (raw blob at --ref, default HEAD).
    File {
        path: String,
        #[arg(long)]
        git_ref: Option<String>,
        #[arg(long)]
        project: Option<String>,
    },
}

fn gitlab_provider(cfg: &pai_config::Config) -> Result<pai_connector_gitlab::RestGitLab> {
    let c = pai_connector_gitlab::GitLabConfig::load(&cfg.data_dir)?.ok_or_else(|| {
        Error::InvalidInput("no gitlab binding — run `pai gitlab configure`".into())
    })?;
    Ok(pai_connector_gitlab::RestGitLab::new(c))
}

/// `pai gitlab` — REST v4 connector ops.
pub(crate) async fn run(cmd: &GitLabCmd, cfg: &pai_config::Config) -> Result<()> {
    run_gitlab_cmds(cmd, cfg).await
}

async fn run_gitlab_cmds(cmd: &GitLabCmd, cfg: &pai_config::Config) -> Result<()> {
    use pai_connector_gitlab::GitLabProvider;
    match cmd {
        GitLabCmd::Configure { oauth } => {
            let read = read_prompt;
            let host = read("GitLab host", "https://gitlab.com")?;
            if host.is_empty() {
                return Err(Error::InvalidInput("host is required".into()));
            }
            let project = read("Default project (group/repo, empty = none)", "")?;
            let oauth_cfg = match oauth.as_deref() {
                None => None,
                Some(client_id) => {
                    if client_id.is_empty() {
                        return Err(Error::InvalidInput(
                            "client_id is required — register an OAuth app first".into(),
                        ));
                    }
                    Some(pai_connector_gitlab::oauth_config(&host, client_id))
                }
            };
            let c = pai_connector_gitlab::GitLabConfig {
                host: host.clone(),
                project: if project.is_empty() {
                    None
                } else {
                    Some(project)
                },
                oauth: oauth_cfg,
            };
            let norm = c.normalized_host();
            if let Some(oc) = &c.oauth {
                // Device-authorization flow: print the code, poll until
                // the user authorizes (or the grant expires).
                let grant = pai_oauth::device_flow(oc).await?;
                let tokens = wait_device_grant(oc, &grant).await?;
                let refresh = tokens.refresh_token.ok_or_else(|| {
                    Error::Provider("oauth grant returned no refresh_token".into())
                })?;
                c.save(&cfg.data_dir)?;
                if pai_connector_gitlab::store_refresh_token(&norm, &refresh) {
                    println!("\nrefresh token stored in OS keystore (gitlab-oauth:{norm})");
                } else {
                    return Err(Error::Other(
                        "keystore unavailable — cannot persist the refresh token".into(),
                    ));
                }
            } else {
                let token = rpassword::prompt_password("Personal access token (api scope): ")
                    .map_err(|e| Error::Other(e.to_string()))?;
                c.save(&cfg.data_dir)?;
                if token.is_empty() {
                    println!("no token stored — set PAI_GITLAB_TOKEN at run time");
                } else if pai_connector_gitlab::store_token(&norm, &token) {
                    println!("token stored in OS keystore (gitlab:{norm})");
                } else {
                    println!("keystore unavailable — set PAI_GITLAB_TOKEN at run time");
                }
            }
            println!("binding written to {}/gitlab.json", cfg.data_dir.display());
        }
        GitLabCmd::Status => {
            match pai_connector_gitlab::GitLabConfig::load(&cfg.data_dir)? {
                Some(c) => {
                    let host = c.normalized_host();
                    println!("{host}");
                    match &c.project {
                        Some(p) => println!("default project: {p}"),
                        None => println!("default project: (none)"),
                    }
                    match &c.oauth {
                        Some(o) => {
                            let has = pai_connector_gitlab::has_refresh_token(&host);
                            println!(
                                "auth: oauth2 ({}) — refresh token {}",
                                o.provider,
                                if has { "available" } else { "MISSING" }
                            );
                        }
                        None => {
                            let t = pai_connector_gitlab::resolve_token(&host).is_ok();
                            println!("auth: token — {}", if t { "available" } else { "MISSING" });
                        }
                    }
                    // Cheap reachability probe — reports who the token is.
                    match gitlab_provider(cfg)?.whoami().await {
                        Ok(u) => println!("user: {u}"),
                        Err(e) => println!("user: (unreachable — {e})"),
                    }
                }
                None => println!("not configured — run `pai gitlab configure`"),
            }
        }
        GitLabCmd::Projects { search, limit } => {
            let rows = gitlab_provider(cfg)?
                .projects(search.as_deref(), *limit)
                .await?;
            for p in &rows {
                println!("  {:>6}  {:<50} {}", p.id, p.path_with_namespace, p.name);
            }
            if rows.is_empty() {
                println!("no projects");
            }
        }
        GitLabCmd::Issues {
            project,
            state,
            search,
            label,
            limit,
        } => {
            let rows = gitlab_provider(cfg)?
                .issues(&pai_connector_gitlab::IssueQuery {
                    project: project.clone(),
                    state: state.clone(),
                    search: search.clone(),
                    labels: label.clone(),
                    limit: *limit,
                })
                .await?;
            for i in &rows {
                println!(
                    "  #{:<5} {:<8} {:<55} {}",
                    i.iid,
                    i.state,
                    i.title,
                    i.created_at.format("%Y-%m-%d")
                );
            }
            if rows.is_empty() {
                println!("no issues");
            }
        }
        GitLabCmd::Issue { iid, project } => {
            let i = gitlab_provider(cfg)?
                .issue(*iid, project.as_deref())
                .await?;
            println!(
                "#{}  {}  [{}]",
                i.summary.iid, i.summary.title, i.summary.state
            );
            println!(
                "author: {}  labels: {}",
                i.summary.author,
                i.summary.labels.join(", ")
            );
            println!("{}\n", i.summary.web_url);
            println!(
                "{}",
                i.description.unwrap_or_else(|| "(no description)".into())
            );
        }
        GitLabCmd::IssueNew {
            title,
            description,
            label,
            project,
        } => {
            let i = gitlab_provider(cfg)?
                .create_issue(&pai_connector_gitlab::NewIssue {
                    project: project.clone(),
                    title: title.clone(),
                    description: description.clone(),
                    labels: label.clone(),
                })
                .await?;
            println!("opened #{} — {}", i.iid, i.web_url);
        }
        GitLabCmd::Comment {
            kind,
            iid,
            body,
            project,
        } => {
            let target = match kind.as_str() {
                "issue" => pai_connector_gitlab::CommentTarget::Issue(*iid),
                "mr" | "merge_request" => pai_connector_gitlab::CommentTarget::MergeRequest(*iid),
                other => {
                    return Err(Error::InvalidInput(format!(
                        "bad kind {other:?} — issue|mr"
                    )))
                }
            };
            let n = gitlab_provider(cfg)?
                .comment(target, body, project.as_deref())
                .await?;
            println!("commented (note {})", n.id);
        }
        GitLabCmd::Mrs {
            project,
            state,
            search,
            limit,
        } => {
            let rows = gitlab_provider(cfg)?
                .merge_requests(&pai_connector_gitlab::MrQuery {
                    project: project.clone(),
                    state: state.clone(),
                    search: search.clone(),
                    limit: *limit,
                })
                .await?;
            for m in &rows {
                println!(
                    "  !{:<5} {:<8} {:<55} {} → {}",
                    m.iid, m.state, m.title, m.source_branch, m.target_branch
                );
            }
            if rows.is_empty() {
                println!("no merge requests");
            }
        }
        GitLabCmd::Mr { iid, project } => {
            let m = gitlab_provider(cfg)?
                .merge_request(*iid, project.as_deref())
                .await?;
            println!(
                "!{}  {}  [{}]",
                m.summary.iid, m.summary.title, m.summary.state
            );
            println!(
                "{} → {}  by {}",
                m.summary.source_branch, m.summary.target_branch, m.summary.author
            );
            if let Some(s) = &m.head_pipeline_status {
                println!("head pipeline: {s}");
            }
            println!("{}\n", m.summary.web_url);
            println!(
                "{}",
                m.description.unwrap_or_else(|| "(no description)".into())
            );
        }
        GitLabCmd::MrCreate {
            source_branch,
            title,
            target_branch,
            description,
            project,
        } => {
            let m = gitlab_provider(cfg)?
                .create_merge_request(&pai_connector_gitlab::NewMr {
                    project: project.clone(),
                    source_branch: source_branch.clone(),
                    target_branch: target_branch.clone(),
                    title: title.clone(),
                    description: description.clone(),
                })
                .await?;
            println!("opened !{} — {}", m.iid, m.web_url);
        }
        GitLabCmd::MrMerge { iid, project } => {
            let m = gitlab_provider(cfg)?
                .merge(*iid, project.as_deref())
                .await?;
            println!("merged !{} — {}", m.iid, m.title);
        }
        GitLabCmd::Pipelines { project, limit } => {
            let rows = gitlab_provider(cfg)?
                .pipelines(*limit, project.as_deref())
                .await?;
            for p in &rows {
                println!(
                    "  {:>8}  {:<10} {:<30} {}",
                    p.id,
                    p.status,
                    p.git_ref,
                    p.created_at.format("%Y-%m-%d %H:%M")
                );
            }
            if rows.is_empty() {
                println!("no pipelines");
            }
        }
        GitLabCmd::PipelineRun { git_ref, project } => {
            let p = gitlab_provider(cfg)?
                .trigger_pipeline(git_ref, project.as_deref())
                .await?;
            println!("pipeline {} → {} on {}", p.id, p.status, p.git_ref);
        }
        GitLabCmd::File {
            path,
            git_ref,
            project,
        } => {
            let text = gitlab_provider(cfg)?
                .repo_file(
                    path,
                    git_ref.as_deref().unwrap_or("HEAD"),
                    project.as_deref(),
                )
                .await?;
            println!("{text}");
        }
    }
    Ok(())
}
