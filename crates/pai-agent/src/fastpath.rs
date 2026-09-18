//! Deterministic intent fast-path.
//!
//! Strong local intents — write/run/read/list/delete files, remember,
//! doc search, capability questions — are decided in *Rust* and executed
//! through the same permission/approval/audit/event gate as model-emitted
//! calls (`AgentRuntime::execute_tool`). The model is only ever asked for
//! file *contents*, never for the tool-call protocol — so these requests
//! work even when the model can't (or won't) emit valid JSON, and when
//! no model is reachable at all (scaffold fallback / pure-Rust intents).

use crate::{
    AgentDefinition, AgentEvent, AgentRuntime, ApprovalHandler, RunOutcome, ToolInvocation,
    ToolStep,
};
use pai_core::*;
use pai_inference::{AIRequest, InferenceProvider, ModelAction};
use std::sync::Arc;

/// What `classify` decided the user wants — high-confidence only;
/// anything ambiguous falls through to the model loop.
#[derive(Debug, Clone, PartialEq)]
pub enum FastIntent {
    /// "what can you do" / "can you write code" — a static, always-correct
    /// answer so the capability question never depends on the model.
    Capabilities,
    /// "remember that X" → memory.remember
    Remember { content: String },
    /// "list files" / "show workspace" → fs.list
    ListFiles { path: String },
    /// "read/show/cat <path>" → fs.read
    ReadFile { path: String },
    /// "delete/remove <path>" → fs.delete
    DeleteFile { path: String },
    /// "run/execute <script>" → shell.exec with the right interpreter
    RunFile { path: String },
    /// "write a python script that …" → codegen → fs.write (+shell.exec)
    WriteCode {
        lang: Option<&'static str>,
        filename: Option<String>,
        spec: String,
        run_after: bool,
    },
    /// "search my documents for X" → documents.search
    DocSearch { query: String },
}

const CAPABILITY_TEXT: &str = "I can chat, remember things you tell me, \
search your documents and email, manage GitLab issues and MRs — and write \
code: files land in your workspace/ folder (Settings → Storage). Reads \
run free; writes and shell commands ask for your approval first.\n\n\
Try: \"write a python script that prints hello\", \"list my workspace \
files\", or \"search my documents for sync\".";

const CODEGEN_SYSTEM: &str = "You are a code generator inside a personal \
AI. Reply with the complete file contents in a single fenced code block \
and nothing else — no explanation, no preamble.";

/// Language table: trigger words → (canonical lang, extension, runner
/// template). `runner` = how `shell.exec` invokes it; None means "just
/// write the file" (compiled/ambiguous languages don't auto-run).
const LANGS: &[(&[&str], &str, &str, Option<&str>)] = &[
    (&["python", "py"], "python", "py", Some("python {f}")),
    (&["rust"], "rust", "rs", None),
    (
        &["javascript", "node", "js"],
        "javascript",
        "js",
        Some("node {f}"),
    ),
    (&["typescript", "ts"], "typescript", "ts", None),
    (
        &["powershell"],
        "powershell",
        "ps1",
        Some("powershell -File {f}"),
    ),
    (
        &["bash", "shell script", "sh "],
        "bash",
        "sh",
        Some("sh {f}"),
    ),
    (&["html", "web page", "webpage"], "html", "html", None),
    (&["css"], "css", "css", None),
    (&["ruby"], "ruby", "rb", Some("ruby {f}")),
    (&["golang", "go "], "go", "go", Some("go run {f}")),
    (&["lua"], "lua", "lua", Some("lua {f}")),
    (&["php"], "php", "php", Some("php {f}")),
    (&["perl"], "perl", "pl", Some("perl {f}")),
    (&["java"], "java", "java", None),
    (&["kotlin"], "kotlin", "kt", None),
    (&["swift"], "swift", "swift", None),
    (&["sql"], "sql", "sql", None),
    (&["c++", "cpp"], "cpp", "cpp", None),
    (&["c "], "c", "c", None),
];

const CODE_NOUNS: &[&str] = &[
    "script", "program", "code", "function", "class", "module", "app", "file", "page", "website",
    "bot", "game", "tool", "utility",
];

const CODE_EXTS: &[&str] = &[
    "py", "rs", "js", "ts", "sh", "ps1", "html", "css", "rb", "go", "c", "cpp", "h", "java", "kt",
    "lua", "sql", "php", "pl", "swift", "json", "toml", "yaml", "yml", "xml", "md", "txt", "csv",
    "ini", "env",
];

fn strip_prefix_any<'a>(text: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes.iter().find_map(|p| text.strip_prefix(p))
}

/// Tokens that look like paths: contain a separator or a `.ext`.
fn path_like(tok: &str) -> bool {
    tok.contains('/') || tok.contains('\\') || {
        tok.rsplit_once('.')
            .map(|(_, e)| {
                !e.is_empty() && e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric())
            })
            .unwrap_or(false)
    }
}

/// Last path-ish token in the text, punctuation-stripped.
fn extract_path(text: &str) -> Option<String> {
    text.split_whitespace()
        .rfind(|t| path_like(t.trim_matches(|c: char| "\"'`.,;:()[]{}".contains(c))))
        .map(|t| {
            t.trim_matches(|c: char| "\"'`.,;:()[]{}".contains(c))
                .to_string()
        })
}

/// A bare word after "named|called|as" — filename without extension.
fn named_word(lower: &str) -> Option<String> {
    for kw in ["named ", "called ", " as "] {
        if let Some(pos) = lower.find(kw) {
            let rest = &lower[pos + kw.len()..];
            let word: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
                .collect();
            if !word.is_empty() {
                return Some(word);
            }
        }
    }
    None
}

fn detect_lang(lower: &str) -> Option<&'static str> {
    LANGS
        .iter()
        .find(|(words, _, _, _)| words.iter().any(|w| lower.contains(w)))
        .map(|(_, lang, _, _)| *lang)
}

fn lang_row(
    lang: &str,
) -> Option<&'static (
    &'static [&'static str],
    &'static str,
    &'static str,
    Option<&'static str>,
)> {
    LANGS.iter().find(|(_, l, _, _)| *l == lang)
}

/// Runner command for a script path, or None when we can't sensibly run
/// it (compiled languages, unknown exts, shell scripts on Windows).
pub fn runner_for(path: &str) -> Option<String> {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_lowercase())?;
    let tmpl = match ext.as_str() {
        "py" => "python {f}",
        "js" => "node {f}",
        "rb" => "ruby {f}",
        "lua" => "lua {f}",
        "php" => "php {f}",
        "pl" => "perl {f}",
        "ps1" => "powershell -File {f}",
        "bat" | "cmd" => "call {f}",
        "exe" => "{f}",
        "sh" => {
            if cfg!(windows) {
                return None; // no sh on a stock Windows shell
            }
            "sh {f}"
        }
        _ => return None,
    };
    Some(tmpl.replace("{f}", path))
}

/// A filename with a known code extension sitting in the text.
fn code_filename(text: &str) -> Option<String> {
    text.split_whitespace()
        .filter_map(|t| {
            let t = t.trim_matches(|c: char| "\"'`.,;:()[]{}".contains(c));
            let (stem, ext) = t.rsplit_once('.')?;
            if !stem.is_empty()
                && stem.chars().all(|c| {
                    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/' || c == '\\'
                })
                && CODE_EXTS.contains(&ext.to_lowercase().as_str())
            {
                Some(t.to_string())
            } else {
                None
            }
        })
        .next()
}

/// Classify high-confidence local intents. Order matters: capability and
/// remember checks first, then write-code (verb + code noun), then the
/// path-verbs, then doc search. Anything uncertain → None → model loop.
pub fn classify(text: &str) -> Option<FastIntent> {
    let t = text.trim();
    if t.is_empty() || t.starts_with('/') {
        return None;
    }
    let lower = t.to_lowercase();

    // --- capability questions: deterministic answer, no model needed
    if [
        "what can you do",
        "what are your capabilities",
        "what do you do",
        "can you write code",
        "can you code",
        "do you write code",
        "help",
    ]
    .iter()
    .any(|p| lower.trim_end_matches(['?', '.', '!']) == *p)
    {
        return Some(FastIntent::Capabilities);
    }

    // --- remember that X
    if let Some(rest) = strip_prefix_any(&lower, &["remember that ", "remember "]) {
        let content = rest.trim().trim_end_matches('.').trim();
        if !content.is_empty() {
            let content = t[t.len() - rest.len()..]
                .trim()
                .trim_end_matches('.')
                .to_string();
            return Some(FastIntent::Remember { content });
        }
    }

    // --- doc search before file ops ("search my documents for X")
    for p in [
        "search my documents for ",
        "search documents for ",
        "search docs for ",
        "search my docs for ",
        "find in my documents ",
        "find in documents ",
    ] {
        if let Some(rest) = lower.strip_prefix(p) {
            let q = rest.trim();
            if !q.is_empty() {
                return Some(FastIntent::DocSearch {
                    query: t[t.len() - rest.len()..].trim().to_string(),
                });
            }
        }
    }

    // --- write code: verb + (code noun | filename.ext)
    let has_write_verb = [
        "write ",
        "create ",
        "make ",
        "generate ",
        "scaffold ",
        "build ",
        "code ",
    ]
    .iter()
    .any(|v| {
        lower.starts_with(v) || lower.contains(&format!(" {}", v.trim_end())) && lower.contains(v)
    });
    let has_code_noun = CODE_NOUNS
        .iter()
        .any(|n| lower.contains(&format!(" {n}")) || lower.contains(&format!("{n} ")))
        || lower.contains("script")
        || lower.contains("code");
    let filename = code_filename(t);
    if has_write_verb && (has_code_noun || filename.is_some()) {
        // "write a letter/email/summary" stays with the model — a code
        // noun or a code-extension filename must be present.
        let lang = detect_lang(&lower);
        let fname = filename.or_else(|| {
            named_word(&lower).map(|w| {
                if w.contains('.') {
                    w
                } else {
                    let ext = lang.and_then(|l| lang_row(l).map(|r| r.2)).unwrap_or("txt");
                    format!("{w}.{ext}")
                }
            })
        });
        let run_after = [
            " and run",
            " then run",
            " and execute",
            " run it",
            " and test",
            " then execute",
        ]
        .iter()
        .any(|p| lower.contains(p));
        return Some(FastIntent::WriteCode {
            lang,
            filename: fname,
            spec: t.to_string(),
            run_after,
        });
    }

    // --- list workspace/files
    if [
        "list files",
        "list my files",
        "list the files",
        "show files",
        "show my files",
        "ls",
        "list workspace",
        "show workspace",
        "list my workspace",
        "show my workspace",
        "what files",
        "what's in my workspace",
        "what is in my workspace",
        "list directory",
        "list the directory",
    ]
    .iter()
    .any(|p| lower.starts_with(p))
    {
        let path = extract_path(t).unwrap_or_else(|| ".".into());
        return Some(FastIntent::ListFiles { path });
    }

    // --- run/execute <path>
    for v in ["run ", "execute "] {
        if let Some(rest) = lower.strip_prefix(v) {
            if let Some(p) = extract_path(&t[t.len() - rest.len()..]) {
                if runner_for(&p).is_some() {
                    return Some(FastIntent::RunFile { path: p });
                }
            }
        }
    }

    // --- read/show/cat <path>
    for v in [
        "read ",
        "show ",
        "cat ",
        "open ",
        "print ",
        "display ",
        "what's in ",
        "what is in ",
    ] {
        if let Some(rest) = lower.strip_prefix(v) {
            if let Some(p) = extract_path(&t[t.len() - rest.len()..]) {
                return Some(FastIntent::ReadFile { path: p });
            }
        }
    }

    // --- delete/remove <path>
    for v in ["delete ", "remove ", "rm "] {
        if let Some(rest) = lower.strip_prefix(v) {
            if let Some(p) = extract_path(&t[t.len() - rest.len()..]) {
                return Some(FastIntent::DeleteFile { path: p });
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Orchestration — runs on AgentRuntime so it can call the private
// execute_tool gate (same permissions/approvals/audit/events as the
// model loop).
// ---------------------------------------------------------------------------

fn extract_code(text: &str) -> Option<String> {
    // First fenced block wins; a reply with no fence at all is taken as
    // raw code when it doesn't start with prose-y JSON.
    if let Some(start) = text.find("```") {
        let after = &text[start + 3..];
        // skip an optional language tag on the opening fence
        let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = &after[body_start..];
        if let Some(end) = body.find("```") {
            let code = body[..end].trim();
            if !code.is_empty() {
                return Some(code.to_string());
            }
        }
    }
    let trimmed = text.trim();
    if !trimmed.is_empty()
        && !trimmed.starts_with('{')
        && !trimmed.starts_with('[')
        && trimmed.lines().count() > 1
        && !trimmed.to_lowercase().starts_with("sure")
        && !trimmed.to_lowercase().starts_with("here")
    {
        return Some(trimmed.to_string());
    }
    None
}

/// Starter scaffold when the model is unreachable or returns prose —
/// the file still gets written; the answer says what happened.
fn scaffold(lang: Option<&str>) -> String {
    match lang {
        Some("python") => {
            "def main():\n    print(\"Hello from Personal AI\")\n\n\nif __name__ == \"__main__\":\n    main()\n".into()
        }
        Some("rust") => "fn main() {\n    println!(\"Hello from Personal AI\");\n}\n".into(),
        Some("javascript") => "console.log(\"Hello from Personal AI\");\n".into(),
        Some("bash") => "#!/bin/sh\necho \"Hello from Personal AI\"\n".into(),
        Some("powershell") => "Write-Output \"Hello from Personal AI\"\n".into(),
        Some("html") => {
            "<!doctype html>\n<html>\n  <body>\n    <h1>Hello from Personal AI</h1>\n  </body>\n</html>\n".into()
        }
        Some("go") => {
            "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"Hello from Personal AI\")\n}\n".into()
        }
        Some("ruby") => "puts \"Hello from Personal AI\"\n".into(),
        Some("lua") => "print(\"Hello from Personal AI\")\n".into(),
        Some("php") => "<?php echo \"Hello from Personal AI\\n\";\n".into(),
        Some("perl") => "print \"Hello from Personal AI\\n\";\n".into(),
        Some("java") => {
            "public class Main {\n    public static void main(String[] args) {\n        System.out.println(\"Hello from Personal AI\");\n    }\n}\n".into()
        }
        _ => "# generated by Personal AI\n".into(),
    }
}

fn default_filename(lang: Option<&str>) -> String {
    let ext = lang.and_then(|l| lang_row(l).map(|r| r.2)).unwrap_or("txt");
    format!("main.{ext}")
}

/// Pull the tool output value out of an observation message.
fn obs_output(msg: &Message) -> (serde_json::Value, bool) {
    msg.content
        .iter()
        .find_map(|c| match c {
            Content::ToolResult {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| (serde_json::json!({"error": "no result"}), true))
}

fn user_msg(text: &str, conv: ConversationId) -> Message {
    Message {
        id: MessageId::new(),
        conversation: conv,
        role: Role::User,
        created_at: now(),
        content: vec![Content::text(text)],
        trust: TrustLevel::User,
    }
}

impl AgentRuntime {
    /// Run one decided tool call through the model-loop gate and return
    /// the observation message (also persisted by the caller).
    #[allow(clippy::too_many_arguments)]
    async fn exec_once(
        &self,
        def: &AgentDefinition,
        run: &AgentRun,
        conv: ConversationId,
        name: &'static str,
        arguments: serde_json::Value,
        approval: &dyn ApprovalHandler,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
        mem_scope: Option<ConversationId>,
    ) -> Message {
        let call_id = ToolCallId::new();
        let ToolStep::Observation(msg) = self
            .execute_tool(ToolInvocation {
                def,
                run,
                call_id: &call_id,
                name,
                arguments,
                approval,
                emit,
                conv,
                memory_scope: mem_scope,
            })
            .await;
        msg
    }

    fn persist_msg(&self, run: &AgentRun, conv: ConversationId, msg: &Message) {
        if let (Some(p), true) = (&self.persistence, run.conversation.is_some()) {
            let _ = p.conversations.append(msg);
            let _ = conv; // conv already encoded in msg
        }
    }

    /// Execute a [`FastIntent`]: direct tools run through `execute_tool`;
    /// WriteCode asks the model for *file contents only* (freeform —
    /// never the tool protocol), writes through fs.write, optionally
    /// runs the result through shell.exec.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn fast_path(
        &self,
        def: &AgentDefinition,
        run: &AgentRun,
        conv: ConversationId,
        intent: FastIntent,
        provider: &Arc<dyn InferenceProvider>,
        approval: &dyn ApprovalHandler,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
        mem_scope: Option<ConversationId>,
    ) -> Result<RunOutcome> {
        emit(AgentEvent::Step { index: 0 });
        let answer = match intent {
            FastIntent::Capabilities => CAPABILITY_TEXT.to_string(),

            FastIntent::Remember { content } => {
                let msg = self
                    .exec_once(
                        def,
                        run,
                        conv,
                        "memory.remember",
                        serde_json::json!({"content": content, "memory_type": "semantic"}),
                        approval,
                        emit,
                        mem_scope,
                    )
                    .await;
                self.persist_msg(run, conv, &msg);
                let (_v, is_err) = obs_output(&msg);
                if is_err {
                    "Couldn't store that memory — denied or unavailable.".into()
                } else {
                    format!("Remembered: {content}")
                }
            }

            FastIntent::ListFiles { path } => {
                let msg = self
                    .exec_once(
                        def,
                        run,
                        conv,
                        "fs.list",
                        serde_json::json!({"path": path}),
                        approval,
                        emit,
                        mem_scope,
                    )
                    .await;
                self.persist_msg(run, conv, &msg);
                let (v, is_err) = obs_output(&msg);
                if is_err {
                    format!(
                        "Couldn't list `{path}`: {}",
                        v["error"].as_str().unwrap_or("error")
                    )
                } else {
                    let entries = v["entries"].as_array().cloned().unwrap_or_default();
                    if entries.is_empty() {
                        format!("`{path}` is empty — nothing in the workspace yet.")
                    } else {
                        let lines: Vec<String> = entries
                            .iter()
                            .map(|e| {
                                let n = e["name"].as_str().unwrap_or("?");
                                if e["dir"].as_bool() == Some(true) {
                                    format!("{n}/")
                                } else {
                                    format!("{n} ({} B)", e["size"].as_u64().unwrap_or(0))
                                }
                            })
                            .collect();
                        format!("`{path}` — {} entries:\n{}", lines.len(), lines.join("\n"))
                    }
                }
            }

            FastIntent::ReadFile { path } => {
                let msg = self
                    .exec_once(
                        def,
                        run,
                        conv,
                        "fs.read",
                        serde_json::json!({"path": path}),
                        approval,
                        emit,
                        mem_scope,
                    )
                    .await;
                self.persist_msg(run, conv, &msg);
                let (v, is_err) = obs_output(&msg);
                if is_err {
                    format!(
                        "Couldn't read `{path}`: {}",
                        v["error"].as_str().unwrap_or("error")
                    )
                } else {
                    let content = v["content"].as_str().unwrap_or("");
                    let note = if v["truncated"].as_bool() == Some(true) {
                        "\n\n(truncated at 64 KiB)"
                    } else {
                        ""
                    };
                    format!("`{path}`:\n```\n{content}\n```{note}")
                }
            }

            FastIntent::DeleteFile { path } => {
                let msg = self
                    .exec_once(
                        def,
                        run,
                        conv,
                        "fs.delete",
                        serde_json::json!({"path": path}),
                        approval,
                        emit,
                        mem_scope,
                    )
                    .await;
                self.persist_msg(run, conv, &msg);
                let (v, is_err) = obs_output(&msg);
                if is_err {
                    format!(
                        "Couldn't delete `{path}`: {}",
                        v["error"].as_str().unwrap_or("error")
                    )
                } else {
                    format!("Deleted `{path}`.")
                }
            }

            FastIntent::RunFile { path } => {
                let cmd = runner_for(&path).unwrap();
                let msg = self
                    .exec_once(
                        def,
                        run,
                        conv,
                        "shell.exec",
                        serde_json::json!({"command": cmd}),
                        approval,
                        emit,
                        mem_scope,
                    )
                    .await;
                self.persist_msg(run, conv, &msg);
                let (v, is_err) = obs_output(&msg);
                if is_err {
                    format!(
                        "Couldn't run `{path}`: {}",
                        v["error"].as_str().unwrap_or("error")
                    )
                } else {
                    let code = v["exit_code"].as_i64().unwrap_or(-1);
                    let out = v["stdout"].as_str().unwrap_or("").trim();
                    let err = v["stderr"].as_str().unwrap_or("").trim();
                    let mut s = format!("`{cmd}` → exit {code}");
                    if !out.is_empty() {
                        s += &format!("\n```\n{out}\n```");
                    }
                    if !err.is_empty() {
                        s += &format!("\nstderr:\n```\n{err}\n```");
                    }
                    s
                }
            }

            FastIntent::DocSearch { query } => {
                let msg = self
                    .exec_once(
                        def,
                        run,
                        conv,
                        "documents.search",
                        serde_json::json!({"query": query}),
                        approval,
                        emit,
                        mem_scope,
                    )
                    .await;
                self.persist_msg(run, conv, &msg);
                let (v, is_err) = obs_output(&msg);
                if is_err {
                    format!(
                        "Couldn't search documents: {}",
                        v["error"].as_str().unwrap_or("error")
                    )
                } else {
                    let results = v["results"].as_array().cloned().unwrap_or_default();
                    if results.is_empty() {
                        format!("No documents matched `{query}`.")
                    } else {
                        let lines: Vec<String> = results
                            .iter()
                            .take(5)
                            .enumerate()
                            .map(|(i, r)| {
                                let title = r["title"].as_str().unwrap_or("untitled");
                                let page = r["page"]
                                    .as_u64()
                                    .map(|p| format!(" p.{p}"))
                                    .unwrap_or_default();
                                format!("[D{}] {title}{page}", i + 1)
                            })
                            .collect();
                        format!(
                            "{} hit(s) for `{query}`:\n{}",
                            results.len(),
                            lines.join("\n")
                        )
                    }
                }
            }

            FastIntent::WriteCode {
                lang,
                filename,
                spec,
                run_after,
            } => {
                self.write_code(
                    def, run, conv, lang, filename, &spec, run_after, provider, approval, emit,
                    mem_scope,
                )
                .await
            }
        };

        // Persist + stream the answer exactly like a model Final does.
        if let (Some(p), true) = (&self.persistence, run.conversation.is_some()) {
            let _ = p.conversations.append(&Message {
                id: MessageId::new(),
                conversation: conv,
                role: Role::Assistant,
                created_at: now(),
                content: vec![Content::text(&answer)],
                trust: TrustLevel::Generated,
            });
        }
        emit(AgentEvent::TextDelta {
            text: answer.clone(),
        });
        Ok(self.finish_run(run.clone(), Some(answer), RunState::Completed, emit))
    }

    /// WriteCode orchestration: freeform codegen → extract block →
    /// fs.write → optional shell.exec. Provider failure degrades to a
    /// scaffold rather than a dead run.
    #[allow(clippy::too_many_arguments)]
    async fn write_code(
        &self,
        def: &AgentDefinition,
        run: &AgentRun,
        conv: ConversationId,
        lang: Option<&'static str>,
        filename: Option<String>,
        spec: &str,
        run_after: bool,
        provider: &Arc<dyn InferenceProvider>,
        approval: &dyn ApprovalHandler,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
        mem_scope: Option<ConversationId>,
    ) -> String {
        // 1) Freeform generation — the model writes *contents*, never
        // the tool protocol. `system` bypasses protocol_prompt.
        let gen = provider
            .generate(&AIRequest {
                system: Some(CODEGEN_SYSTEM.into()),
                messages: vec![user_msg(spec, conv)],
                tools: vec![],
                model: def.model.clone(),
                require_structured: false,
                ..Default::default()
            })
            .await;

        let (code, scaffolded) = match gen {
            Ok(resp) => {
                let text = resp
                    .action
                    .and_then(|a| match a {
                        ModelAction::Final { content } => Some(content),
                        _ => None,
                    })
                    .unwrap_or(resp.text);
                match extract_code(&text) {
                    Some(c) => (c, false),
                    None => (scaffold(lang), true),
                }
            }
            Err(_) => (scaffold(lang), true),
        };

        let fname = filename.unwrap_or_else(|| default_filename(lang));

        // 2) fs.write through the permission gate.
        let msg = self
            .exec_once(
                def,
                run,
                conv,
                "fs.write",
                serde_json::json!({"path": fname, "content": code}),
                approval,
                emit,
                mem_scope,
            )
            .await;
        self.persist_msg(run, conv, &msg);
        let (v, is_err) = obs_output(&msg);
        if is_err {
            return format!(
                "Couldn't write `{fname}`: {}",
                v["error"].as_str().unwrap_or("error")
            );
        }

        let mut answer = format!("Wrote `{fname}` ({} bytes)", code.len());
        if scaffolded {
            answer += " — the model didn't return usable code, so it's a starter scaffold";
        }

        // 3) Optional run — only for languages with a known runner.
        if run_after {
            if let Some(cmd) = lang
                .and_then(|l| lang_row(l).and_then(|r| r.3))
                .map(|t| t.replace("{f}", &fname))
            {
                let msg = self
                    .exec_once(
                        def,
                        run,
                        conv,
                        "shell.exec",
                        serde_json::json!({"command": cmd}),
                        approval,
                        emit,
                        mem_scope,
                    )
                    .await;
                self.persist_msg(run, conv, &msg);
                let (v, is_err) = obs_output(&msg);
                if is_err {
                    answer +=
                        &format!("\n\nRun failed: {}", v["error"].as_str().unwrap_or("error"));
                } else {
                    let code = v["exit_code"].as_i64().unwrap_or(-1);
                    let out = v["stdout"].as_str().unwrap_or("").trim();
                    answer += &format!("\n\n`{cmd}` → exit {code}");
                    if !out.is_empty() {
                        answer += &format!("\n```\n{out}\n```");
                    }
                }
            } else {
                answer += "\n\n(no auto-run for this language — the file is ready in workspace/)";
            }
        }
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_write_code() {
        match classify("write a python script that prints hello") {
            Some(FastIntent::WriteCode {
                lang, run_after, ..
            }) => {
                assert_eq!(lang, Some("python"));
                assert!(!run_after);
            }
            other => panic!("expected WriteCode, got {other:?}"),
        }
        match classify("create a rust program named hello and run it") {
            Some(FastIntent::WriteCode {
                lang,
                filename,
                run_after,
                ..
            }) => {
                assert_eq!(lang, Some("rust"));
                assert_eq!(filename.as_deref(), Some("hello.rs"));
                assert!(run_after);
            }
            other => panic!("expected WriteCode, got {other:?}"),
        }
        match classify("write a config.json for me") {
            Some(FastIntent::WriteCode { filename, .. }) => {
                assert_eq!(filename.as_deref(), Some("config.json"));
            }
            other => panic!("expected WriteCode, got {other:?}"),
        }
    }

    #[test]
    fn write_verb_without_code_noun_falls_through() {
        // "write a letter/email/summary" is prose work — the model loop
        // should get it, not the code fast-path.
        assert_eq!(classify("write a letter to my landlord"), None);
        assert_eq!(classify("write an email to the team"), None);
        assert_eq!(classify("summarize this paragraph"), None);
    }

    #[test]
    fn classifies_file_ops() {
        assert_eq!(
            classify("list files"),
            Some(FastIntent::ListFiles { path: ".".into() })
        );
        assert_eq!(
            classify("what's in my workspace"),
            Some(FastIntent::ListFiles { path: ".".into() })
        );
        assert_eq!(
            classify("read main.py"),
            Some(FastIntent::ReadFile {
                path: "main.py".into()
            })
        );
        assert_eq!(
            classify("delete old.log"),
            Some(FastIntent::DeleteFile {
                path: "old.log".into()
            })
        );
        assert_eq!(
            classify("run test.py"),
            Some(FastIntent::RunFile {
                path: "test.py".into()
            })
        );
        // No path → no file intent.
        assert_eq!(classify("read my email"), None);
        assert_eq!(classify("run the report"), None);
    }

    #[test]
    fn classifies_remember_and_search() {
        match classify("remember that my birthday is May 5") {
            Some(FastIntent::Remember { content }) => {
                assert_eq!(content, "my birthday is May 5");
            }
            other => panic!("expected Remember, got {other:?}"),
        }
        match classify("search my documents for sync protocol") {
            Some(FastIntent::DocSearch { query }) => {
                assert_eq!(query, "sync protocol");
            }
            other => panic!("expected DocSearch, got {other:?}"),
        }
    }

    #[test]
    fn classifies_capability_questions() {
        assert_eq!(classify("what can you do?"), Some(FastIntent::Capabilities));
        assert_eq!(
            classify("can you write code?"),
            Some(FastIntent::Capabilities)
        );
    }

    #[test]
    fn runner_for_known_scripts() {
        assert_eq!(runner_for("x.py").as_deref(), Some("python x.py"));
        assert_eq!(runner_for("x.js").as_deref(), Some("node x.js"));
        assert_eq!(runner_for("a.docx"), None);
        assert_eq!(runner_for("noext"), None);
    }

    #[test]
    fn extracts_fenced_code() {
        let text = "Here you go:\n```python\nprint('hi')\n```\nDone!";
        assert_eq!(extract_code(text).as_deref(), Some("print('hi')"));
        // JSON-looking or single-line prose isn't code.
        assert_eq!(extract_code("{\"a\":1}"), None);
        assert_eq!(extract_code("short answer"), None);
        // Multi-line unfenced code is accepted.
        assert_eq!(
            extract_code("def f():\n    return 1").as_deref(),
            Some("def f():\n    return 1")
        );
    }
}
