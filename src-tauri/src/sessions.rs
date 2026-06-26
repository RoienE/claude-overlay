//! Per-session token usage from local Claude Code JSONL transcripts.
//!
//! `list_active` scans the same JSONL files as `fallback_logs`, but returns
//! one `SessionSummary` per file rather than rolling aggregates.  Only files
//! whose `mtime` falls within the last `ACTIVE_THRESHOLD_MINS` minutes are
//! considered, keeping the call cheap.
//!
//! Sub-agent transcripts live at `<project>/<parentSessionId>/subagents/
//! agent-<agentId>.jsonl`.  The sub-agent file does not record its own type,
//! so we recover it (e.g. "Explore", "developer-backend") from the parent
//! session: the parent's `Task`/`Agent` tool_use carries `subagent_type`, and
//! its tool_result text carries the matching `agentId`.

use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::fallback_logs::{claude_dir, collect_jsonl_files, JsRecord};
use crate::model::SessionSummary;

/// Files whose mtime is older than this many minutes are skipped.
const ACTIVE_THRESHOLD_MINS: i64 = 10;

/// Return session summaries for all JSONL files modified within the active
/// threshold, sorted by `last_active` descending (most recent first).
pub fn list_active() -> Vec<SessionSummary> {
    let Some(base) = claude_dir() else {
        return vec![];
    };

    let files = collect_jsonl_files(&base);
    let now = Utc::now();
    let cutoff = now - Duration::minutes(ACTIVE_THRESHOLD_MINS);

    // Parsed-once cache of parent sessions: parent path → (agentId → subagent type).
    let mut parent_cache: HashMap<PathBuf, HashMap<String, String>> = HashMap::new();
    let mut sessions: Vec<SessionSummary> = Vec::new();

    for path in &files {
        // Pre-filter: skip any file whose mtime is older than the threshold.
        if let Ok(meta) = fs::metadata(path) {
            if let Ok(modified) = meta.modified() {
                let modified_dt: DateTime<Utc> = modified.into();
                if modified_dt < cutoff {
                    continue;
                }
            }
        }

        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let Ok(file) = fs::File::open(path) else {
            continue;
        };
        let reader = BufReader::new(file);

        let mut input_tokens: u64 = 0;
        let mut output_tokens: u64 = 0;
        let mut cache_creation: u64 = 0;
        let mut cache_read: u64 = 0;
        let mut last_active: Option<String> = None;
        let mut last_ts: Option<DateTime<Utc>> = None;
        let mut model: Option<String> = None;
        let mut last_cwd: Option<String> = None;

        for line in reader.lines() {
            let Ok(line) = line else { continue };
            let Ok(record) = serde_json::from_str::<JsRecord>(&line) else {
                continue;
            };

            // Track the most recent working directory from any record.
            if let Some(cwd) = &record.cwd {
                last_cwd = Some(cwd.clone());
            }

            // Only accumulate token counts from assistant messages with usage.
            let msg = match &record.message {
                Some(m) if m.role.as_deref() == Some("assistant") => m,
                _ => continue,
            };

            // Track the model from the last assistant message seen.
            if let Some(m) = &msg.model {
                model = Some(m.clone());
            }

            let Some(u) = &msg.usage else { continue };

            input_tokens += u.input_tokens.unwrap_or(0);
            output_tokens += u.output_tokens.unwrap_or(0);
            cache_creation += u.cache_creation_input_tokens.unwrap_or(0);
            cache_read += u.cache_read_input_tokens.unwrap_or(0);

            // Track the maximum timestamp seen across all assistant messages.
            if let Some(ts_str) = &record.timestamp {
                if let Ok(ts) = ts_str.parse::<DateTime<Utc>>() {
                    if last_ts.map_or(true, |prev| ts > prev) {
                        last_ts = Some(ts);
                        last_active = Some(ts_str.clone());
                    }
                }
            }
        }

        // Derive project name from the last observed cwd, falling back to
        // the file's parent directory name when no cwd was recorded.
        let project = last_cwd
            .as_deref()
            .and_then(|c| Path::new(c).file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .or_else(|| {
                path.parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());

        // For sub-agent transcripts, resolve the agent type from the parent
        // session. Ordinary top-level sessions get `None`.
        let agent_name = subagent_agent_id(path).map(|agent_id| {
            parent_session_file(path)
                .map(|parent| {
                    parent_cache
                        .entry(parent.clone())
                        .or_insert_with(|| parent_agent_types(&parent))
                        .get(&agent_id)
                        .cloned()
                })
                .flatten()
                .unwrap_or_else(|| "subagent".to_string())
        });

        let total_tokens = input_tokens + output_tokens + cache_creation + cache_read;

        sessions.push(SessionSummary {
            session_id,
            project,
            agent_name,
            model,
            last_active: last_active.unwrap_or_default(),
            input_tokens,
            output_tokens,
            cache_creation,
            cache_read,
            total_tokens,
            active: true,
        });
    }

    // Most-recently-active session first.
    sessions.sort_by(|a, b| b.last_active.cmp(&a.last_active));

    sessions
}

/// If `path` is a sub-agent transcript (`.../subagents/agent-<id>.jsonl`),
/// return its `<id>`; otherwise `None`.
fn subagent_agent_id(path: &Path) -> Option<String> {
    let parent_name = path.parent()?.file_name()?.to_str()?;
    if parent_name != "subagents" {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    stem.strip_prefix("agent-").map(|s| s.to_string())
}

/// Given a sub-agent transcript path, resolve the parent session's JSONL path:
/// `.../<project>/<sessionId>/subagents/agent-<id>.jsonl` →
/// `.../<project>/<sessionId>.jsonl`.
fn parent_session_file(subagent_path: &Path) -> Option<PathBuf> {
    let subagents_dir = subagent_path.parent()?; // .../<sid>/subagents
    let session_dir = subagents_dir.parent()?; // .../<sid>
    let session_id = session_dir.file_name()?.to_str()?;
    let project_dir = session_dir.parent()?; // .../<project>
    Some(project_dir.join(format!("{session_id}.jsonl")))
}

/// Parse a parent session transcript into a map of `agentId → subagent_type`.
///
/// Links each `Task`/`Agent` tool_use (which carries `subagent_type`) to the
/// `agentId` echoed back in the corresponding tool_result text, via the shared
/// `tool_use_id`. Best-effort: anything that doesn't parse is skipped.
fn parent_agent_types(parent_path: &Path) -> HashMap<String, String> {
    let mut agent_map: HashMap<String, String> = HashMap::new();
    let Ok(file) = fs::File::open(parent_path) else {
        return agent_map;
    };
    let reader = BufReader::new(file);

    let mut task_map: HashMap<String, String> = HashMap::new(); // tool_use_id → type
    let mut results: Vec<(String, String)> = Vec::new(); // (tool_use_id, result text)

    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(content) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };

        for block in content {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") => {
                    let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    if name == "Task" || name == "Agent" {
                        if let (Some(id), Some(st)) = (
                            block.get("id").and_then(|i| i.as_str()),
                            block
                                .get("input")
                                .and_then(|i| i.get("subagent_type"))
                                .and_then(|s| s.as_str()),
                        ) {
                            task_map.insert(id.to_string(), st.to_string());
                        }
                    }
                }
                Some("tool_result") => {
                    if let Some(id) = block.get("tool_use_id").and_then(|i| i.as_str()) {
                        let text = block.get("content").map(|c| c.to_string()).unwrap_or_default();
                        results.push((id.to_string(), text));
                    }
                }
                _ => {}
            }
        }
    }

    for (tool_use_id, text) in results {
        if let Some(st) = task_map.get(&tool_use_id) {
            if let Some(aid) = extract_agent_id(&text) {
                agent_map.insert(aid, st.clone());
            }
        }
    }

    agent_map
}

/// Extract the first `agentId` value (an alphanumeric run) from result text
/// such as `"agentId: ad8fe3bd855c6c543 (use SendMessage ...)"`.
fn extract_agent_id(text: &str) -> Option<String> {
    let idx = text.find("agentId")?;
    let rest = &text[idx + "agentId".len()..];
    let id: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_alphanumeric())
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}
