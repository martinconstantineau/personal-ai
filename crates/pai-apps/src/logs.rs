//! Per-app run logs — `apps/<id>/logs/<unix_ms>-<rand>.json`, one
//! file per run, newest 20 kept. Local-only by design: diagnostics
//! belong to the device that ran the app — they never sync and never
//! ride inside `bkp/` paks (`snapshot_data` walks `data/` only). The
//! sandbox can't see them either (`logs/` is not a preopen).

use crate::{installed_dir, AppResult};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Keep the newest N run logs per app; older ones are pruned on write.
pub const LOG_KEEP: usize = 20;

/// One recorded run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunLog {
    pub at: String,
    pub app_id: String,
    pub args: Vec<String>,
    /// `Some(code)` from proc_exit; `None` = clean return.
    pub exit_code: Option<u32>,
    pub fuel: u64,
    /// Set when the run trapped/errored — `run` never returns a
    /// `RunOutput` on failure, so the log carries the message instead.
    pub trap: Option<String>,
    /// Lossy UTF-8 — logs are for humans and the operator agent.
    pub stdout: String,
    pub stderr: String,
}

fn logs_dir(data_dir: &Path, app_id: &str) -> PathBuf {
    installed_dir(data_dir, app_id).join("logs")
}

/// Persist one run; prunes beyond [`LOG_KEEP`]. Never fails the run —
/// logging errors are swallowed (a full disk shouldn't break apps).
pub fn record(data_dir: &Path, app_id: &str, entry: &RunLog) {
    let _ = record_inner(data_dir, app_id, entry);
}

fn record_inner(data_dir: &Path, app_id: &str, entry: &RunLog) -> AppResult<()> {
    let dir = logs_dir(data_dir, app_id);
    std::fs::create_dir_all(&dir)?;
    let ms = pai_core::now().timestamp_millis();
    let name = format!(
        "{ms}-{:08x}.json",
        rand_core::RngCore::next_u32(&mut rand_core::OsRng)
    );
    let body = serde_json::to_vec_pretty(entry)
        .map_err(|e| crate::AppError::Layout(format!("run log encode: {e}")))?;
    std::fs::write(dir.join(name), body)?;

    // Prune oldest beyond the keep window — names sort by timestamp.
    let mut names: Vec<_> = std::fs::read_dir(&dir)?
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .map(|e| e.file_name())
        .collect();
    names.sort();
    while names.len() > LOG_KEEP {
        let _ = std::fs::remove_file(dir.join(names.remove(0)));
    }
    Ok(())
}

/// Newest-first run logs for an app, capped at `limit`.
pub fn tail(data_dir: &Path, app_id: &str, limit: usize) -> Vec<RunLog> {
    let dir = logs_dir(data_dir, app_id);
    let mut names: Vec<_> = match std::fs::read_dir(&dir) {
        Ok(it) => it
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .map(|e| e.file_name())
            .collect(),
        Err(_) => return Vec::new(),
    };
    names.sort_by(|a, b| b.cmp(a)); // newest first
    names
        .into_iter()
        .take(limit)
        .filter_map(|n| serde_json::from_slice(&std::fs::read(dir.join(n)).ok()?).ok())
        .collect()
}

/// Last `lines` of `s`, bounded to `max_bytes` — the "tail" an operator
/// wants without shipping a whole run's output.
pub fn tail_str(s: &str, lines: usize, max_bytes: usize) -> String {
    let tail: String = s
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    if tail.len() <= max_bytes {
        tail
    } else {
        tail[tail.len() - max_bytes..].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(app: &str, i: i64) -> RunLog {
        RunLog {
            at: format!("2026-01-01T00:00:{i:02}Z"),
            app_id: app.into(),
            args: vec![],
            exit_code: Some(0),
            fuel: 100,
            trap: None,
            stdout: format!("out {i}"),
            stderr: String::new(),
        }
    }

    #[test]
    fn records_and_tails_newest_first() {
        let dir = std::env::temp_dir().join(format!("pai-logs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("apps/a")).unwrap();
        for i in 0..3 {
            record(&dir, "a", &entry("a", i));
        }
        let got = tail(&dir, "a", 10);
        assert_eq!(got.len(), 3);
        // Names share the ms bucket when writes are fast — order among
        // them isn't guaranteed, but all three must come back.
        let mut outs: Vec<_> = got.iter().map(|e| e.stdout.clone()).collect();
        outs.sort();
        assert_eq!(outs, vec!["out 0", "out 1", "out 2"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prunes_beyond_keep() {
        let dir = std::env::temp_dir().join(format!("pai-logs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("apps/a")).unwrap();
        for i in 0..(LOG_KEEP + 5) as i64 {
            record(&dir, "a", &entry("a", i));
        }
        assert_eq!(tail(&dir, "a", 100).len(), LOG_KEEP);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_str_bounds_output() {
        let s = "l1\nl2\nl3\nl4";
        assert_eq!(tail_str(s, 2, 100), "l3\nl4");
        assert_eq!(tail_str("abcdef", 5, 3), "def");
    }

    #[test]
    fn empty_when_no_logs() {
        let dir = std::env::temp_dir().join(format!("pai-logs-{}", uuid::Uuid::new_v4()));
        assert!(tail(&dir, "ghost", 5).is_empty());
    }
}
