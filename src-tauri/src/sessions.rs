//! Per-session token usage from local Claude Code JSONL transcripts.
//!
//! `list_active` scans the same JSONL files as `fallback_logs`, but returns
//! one `SessionSummary` per file rather than rolling aggregates.  It groups
//! files by *family* (the root top-level session + all its sub-agents) and only
//! processes families where at least one file has been modified within
//! `threshold_mins` minutes (caller-supplied, default 30).  Within a live family
//! the result is **pruned**: a node is included only if its own subtree contains
//! at least one active node (mtime within threshold).  Active nodes and their
//! ancestors are kept (tree stays connected); stale leaf sub-agents with no
//! active descendants are dropped.
//!
//! ## Node identity and parentage
//!
//! * **Top-level session** — `id` = file stem (UUID), `parent_id` = `None`.
//! * **Sub-agent** — `id` = agentId (stem after `agent-`), `parent_id` =
//!   the *issuing* node's id (the agent or session whose transcript issued the
//!   `Task`/`Agent` tool call that spawned this child).
//!
//! Two link tables per family, built by parsing **all** files in the family:
//!   1. `by_agent_id` — definitive once the child finishes (echoed in
//!      `tool_result`).  Issuer is the transcript whose `tool_result` echoes
//!      the child agentId.
//!   2. `by_prompt` — works while the child is still running (the child's
//!      first user message matches the issuing `Task` prompt verbatim or via
//!      prefix/contains heuristics).
//!
//! Grandchildren are handled because we parse **sub-agent files** too, not
//! only the root.  The issuer_node_id for a root file is its sessionId; for a
//! sub-agent file it is its own agentId.
//!
//! ## Project-name consistency
//!
//! Instead of using the raw `cwd` basename (which moves when a shell `cd`s
//! into a sub-folder), we re-encode the cwd's ancestors with
//! `encode_project_path` and match against the `projects/<encoded>` directory
//! name, recovering the true project root (e.g. `claude-overlay` rather than
//! `src-tauri`).  Sub-agents share the same `projects/<encoded>` dir as the
//! root, so all nodes in a family show the same project name.

use chrono::{DateTime, Duration, Utc};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::fallback_logs::{claude_dir, collect_jsonl_files, JsRecord};
use crate::model::SessionSummary;

// ── Internal data types ───────────────────────────────────────────────────────

/// Family-wide spawn links recovered by parsing every transcript in a family.
#[derive(Default)]
struct FamilyLinks {
    /// `(task_prompt, subagent_type, issuer_node_id)` — matched against a
    /// sub-agent's first user message to resolve type + parent while running.
    by_prompt: Vec<(String, String, String)>,
    /// `child_agent_id → (subagent_type, issuer_node_id)` — set after the
    /// child finishes (agentId echoed in the parent's `tool_result`).
    by_agent_id: HashMap<String, (String, String)>,
}

/// Per-file parsed statistics extracted while reading a transcript.
struct FileStats {
    project_cwd: Option<String>,
    model: Option<String>,
    last_active: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation: u64,
    cache_read: u64,
}

// ── Path helpers ──────────────────────────────────────────────────────────────

/// Return the mtime of `path` as a UTC `DateTime`, or `None` on any error.
fn file_mtime(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(|t| t.into())
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

/// Return the canonical family root path for any transcript file.
/// For a top-level session this is the file itself; for a sub-agent it is the
/// root session file (which may or may not exist on disk).
fn family_root_path(path: &Path) -> PathBuf {
    if subagent_agent_id(path).is_some() {
        parent_session_file(path).unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    }
}

// ── Project-name helpers ──────────────────────────────────────────────────────

/// Encode a path the way Claude Code names its `projects/<dir>` folder:
/// replace each `:`, `\`, `/`, `.` with `-`.
fn encode_project_path(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| match c {
            ':' | '\\' | '/' | '.' => '-',
            other => other,
        })
        .collect()
}

/// Recover the project-root folder name (e.g. `"claude-overlay"`) for a
/// transcript file.
///
/// Finds the `projects/<encoded>` directory the transcript lives under, then
/// walks the real `cwd`'s ancestors until the encoded form matches `<encoded>`
/// (case-insensitive).  Returns the matching ancestor's basename.
///
/// This correctly handles sessions/sub-agents launched from a sub-folder
/// (e.g. `cwd = …/claude-overlay/src-tauri`) and returns the project root
/// (`claude-overlay`) rather than the sub-folder.
fn project_root_name(transcript_path: &Path, cwd: Option<&str>) -> Option<String> {
    // 1) Locate the encoded project dir: walk transcript ancestors until
    //    we find a directory whose parent is named "projects".
    let mut encoded_dir: Option<&Path> = None;
    let mut anc = transcript_path.parent();
    while let Some(dir) = anc {
        if dir
            .parent()
            .and_then(|p| p.file_name())
            .map_or(false, |n| n == "projects")
        {
            encoded_dir = Some(dir);
            break;
        }
        anc = dir.parent();
    }
    let encoded_name = encoded_dir?.file_name()?.to_str()?;

    // 2) Walk the cwd's ancestors; stop when the encoded form matches
    //    the encoded dir name (case-insensitive — Windows paths).
    let cwd = cwd?;
    let mut p: Option<&Path> = Some(Path::new(cwd));
    while let Some(dir) = p {
        if encode_project_path(dir).eq_ignore_ascii_case(encoded_name) {
            return dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());
        }
        p = dir.parent();
    }
    None
}

// ── Transcript parsing helpers ────────────────────────────────────────────────

/// Read the first user-message text from a transcript file (the Task prompt,
/// for a sub-agent).
fn first_user_prompt(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let content = v.get("message").and_then(|m| m.get("content"))?;
        if let Some(s) = content.as_str() {
            return Some(s.to_string());
        }
        if let Some(arr) = content.as_array() {
            for b in arr {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(s) = b.get("text").and_then(|t| t.as_str()) {
                        return Some(s.to_string());
                    }
                }
            }
        }
        // First user record reached but no plain text found.
        return None;
    }
    None
}

/// Parse token stats from a single transcript file.
fn parse_file_stats(path: &Path) -> FileStats {
    let mut stats = FileStats {
        project_cwd: None,
        model: None,
        last_active: None,
        input_tokens: 0,
        output_tokens: 0,
        cache_creation: 0,
        cache_read: 0,
    };

    let Ok(file) = fs::File::open(path) else {
        return stats;
    };
    let reader = BufReader::new(file);
    let mut last_ts: Option<DateTime<Utc>> = None;

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let Ok(record) = serde_json::from_str::<JsRecord>(&line) else {
            continue;
        };

        // Capture the FIRST working directory seen — later records may reflect
        // a sub-folder the shell cd'd into, which must not override the root.
        if stats.project_cwd.is_none() {
            if let Some(cwd) = &record.cwd {
                stats.project_cwd = Some(cwd.clone());
            }
        }

        // Only accumulate token counts from assistant messages with usage.
        let msg = match &record.message {
            Some(m) if m.role.as_deref() == Some("assistant") => m,
            _ => continue,
        };

        if let Some(m) = &msg.model {
            stats.model = Some(m.clone());
        }

        let Some(u) = &msg.usage else { continue };

        stats.input_tokens += u.input_tokens.unwrap_or(0);
        stats.output_tokens += u.output_tokens.unwrap_or(0);
        stats.cache_creation += u.cache_creation_input_tokens.unwrap_or(0);
        stats.cache_read += u.cache_read_input_tokens.unwrap_or(0);

        // Track the maximum timestamp seen across all assistant messages.
        if let Some(ts_str) = &record.timestamp {
            if let Ok(ts) = ts_str.parse::<DateTime<Utc>>() {
                if last_ts.map_or(true, |prev| ts > prev) {
                    last_ts = Some(ts);
                    stats.last_active = Some(ts_str.clone());
                }
            }
        }
    }

    stats
}

/// Accumulate spawn-link information from one transcript into `links`.
///
/// `issuer_node_id` is the stable node id of the file being parsed:
///   - the root session's `sessionId` when parsing the root file, or
///   - the sub-agent's `agentId` when parsing a sub-agent file.
///
/// This means grandchildren (spawned by sub-agents) record their true parent
/// (the sub-agent) rather than the root session.
fn accumulate_links(path: &Path, issuer_node_id: &str, links: &mut FamilyLinks) {
    let Ok(file) = fs::File::open(path) else {
        return;
    };
    let reader = BufReader::new(file);

    // tool_use_id → (subagent_type, prompt) for Task/Agent tool-use blocks.
    let mut task_map: HashMap<String, (String, String)> = HashMap::new();
    // (tool_use_id, result_text) for tool_result blocks.
    let mut results: Vec<(String, String)> = Vec::new();

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
                    if name != "Task" && name != "Agent" {
                        continue;
                    }
                    let Some(st) = block
                        .get("input")
                        .and_then(|i| i.get("subagent_type"))
                        .and_then(|s| s.as_str())
                    else {
                        continue;
                    };
                    let prompt = block
                        .get("input")
                        .and_then(|i| i.get("prompt"))
                        .and_then(|p| p.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();

                    if let Some(id) = block.get("id").and_then(|i| i.as_str()) {
                        task_map.insert(id.to_string(), (st.to_string(), prompt.clone()));
                    }
                    if !prompt.is_empty() {
                        links.by_prompt.push((
                            prompt,
                            st.to_string(),
                            issuer_node_id.to_string(),
                        ));
                    }
                }
                Some("tool_result") => {
                    if let Some(id) = block.get("tool_use_id").and_then(|i| i.as_str()) {
                        let text =
                            block.get("content").map(|c| c.to_string()).unwrap_or_default();
                        results.push((id.to_string(), text));
                    }
                }
                _ => {}
            }
        }
    }

    // Match tool_results back to their task_map entry to record child agentIds.
    for (tool_use_id, text) in results {
        if let Some((st, _)) = task_map.get(&tool_use_id) {
            if let Some(aid) = extract_agent_id(&text) {
                links
                    .by_agent_id
                    .insert(aid, (st.clone(), issuer_node_id.to_string()));
            }
        }
    }
}

/// Build family-wide links by parsing every transcript in `family_files`.
/// Returns `(FamilyLinks, root_session_id)`.
fn build_family_links(family_root: &Path, family_files: &[PathBuf]) -> (FamilyLinks, String) {
    let mut links = FamilyLinks::default();

    let root_session_id = family_root
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    for path in family_files {
        if let Some(agent_id) = subagent_agent_id(path) {
            // Children spawned from this sub-agent file should record its agentId
            // as their issuer.
            accumulate_links(path, &agent_id, &mut links);
        } else {
            // Root (top-level) file: its children's issuer is the root sessionId.
            accumulate_links(path, &root_session_id, &mut links);
        }
    }

    (links, root_session_id)
}

/// Resolve a sub-agent's display name and the issuer node id (its true parent).
///
/// Returns `(agent_name, Option<issuer_node_id>)`.
/// `None` issuer means the link was not found in `links`; the caller applies
/// the fallback (`root_session_id`, or `None` when the root file is missing).
fn resolve_agent_name_and_parent(
    links: &FamilyLinks,
    agent_id: &str,
    sub_prompt: Option<&str>,
) -> (String, Option<String>) {
    // `by_agent_id` is definitive (set once the child completes).
    if let Some((st, issuer)) = links.by_agent_id.get(agent_id) {
        return (st.clone(), Some(issuer.clone()));
    }

    // `by_prompt` works while the child is still running.
    if let Some(sp) = sub_prompt {
        let sp = sp.trim();
        // 1) Exact match.
        if let Some((_, t, issuer)) = links.by_prompt.iter().find(|(p, _, _)| p == sp) {
            return (t.clone(), Some(issuer.clone()));
        }
        // 2) Prefix match (one is a prefix of the other, min 40 chars).
        if let Some((_, t, issuer)) = links.by_prompt.iter().find(|(p, _, _)| {
            let (long, short) = if p.len() >= sp.len() {
                (p.as_str(), sp)
            } else {
                (sp, p.as_str())
            };
            short.len() >= 40 && long.starts_with(short)
        }) {
            return (t.clone(), Some(issuer.clone()));
        }
        // 3) Contains match (one contains the other, min 60 chars).
        if let Some((_, t, issuer)) = links.by_prompt.iter().find(|(p, _, _)| {
            let (long, short) = if p.len() >= sp.len() {
                (p.as_str(), sp)
            } else {
                (sp, p.as_str())
            };
            short.len() >= 60 && long.contains(short)
        }) {
            return (t.clone(), Some(issuer.clone()));
        }
    }

    ("subagent".to_string(), None)
}

/// Extract the first `agentId` value from result text such as
/// `"agentId: ad8fe3bd855c6c543 (use SendMessage ...)"`.
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

// ── Public API ────────────────────────────────────────────────────────────────

/// Compute the set of node ids to include for a family, applying the subtree-prune rule.
///
/// `nodes` is a slice of `(node_id, parent_id, active)` triples for all nodes in a
/// single family.  A node is included iff its own subtree contains at least one active
/// node (i.e. the node itself is active, or it is an ancestor of an active node).
///
/// Ancestor walks are capped at 20 hops to guard against cycles in parent links.
/// The walk also stops when a parent id is not present in the family node set.
pub(crate) fn compute_included(nodes: &[(String, Option<String>, bool)]) -> HashSet<String> {
    const MAX_ANCESTOR_HOPS: usize = 20;

    // Build id → parent_id lookup for the family.
    let parent_map: HashMap<&str, Option<&str>> = nodes
        .iter()
        .map(|(id, pid, _)| (id.as_str(), pid.as_deref()))
        .collect();

    let mut include: HashSet<String> = HashSet::new();

    for (id, _, active) in nodes {
        if !active {
            continue;
        }
        // Seed the active node itself.
        include.insert(id.clone());
        // Walk its ancestor chain upward, inserting each ancestor.
        let mut current = id.as_str();
        for _ in 0..MAX_ANCESTOR_HOPS {
            let parent = match parent_map.get(current) {
                Some(Some(p)) => *p,
                _ => break, // no parent or parent is None (root node)
            };
            if !parent_map.contains_key(parent) {
                break; // parent not in this family — stop
            }
            include.insert(parent.to_string());
            current = parent;
        }
    }

    include
}

/// Return session summaries for nodes in *live* families (families where at
/// least one transcript was modified within `threshold_mins`).
///
/// Within each live family the result is pruned: a node is included only if
/// its own subtree contains at least one active node.  Active nodes and their
/// ancestors are kept (tree stays connected); stale leaf sub-agents with no
/// active descendants are dropped.  Results are sorted by `last_active` descending.
pub fn list_active(threshold_mins: i64) -> Vec<SessionSummary> {
    let Some(base) = claude_dir() else {
        return vec![];
    };

    let files = collect_jsonl_files(&base);
    let now = Utc::now();
    let cutoff = now - Duration::minutes(threshold_mins);

    // Group every transcript file by its family root path.
    let mut families: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for path in &files {
        let root = family_root_path(path);
        families.entry(root).or_default().push(path.clone());
    }

    // Cache of (FamilyLinks, root_session_id) keyed by family root path.
    let mut family_links_cache: HashMap<PathBuf, (FamilyLinks, String)> = HashMap::new();
    let mut sessions: Vec<SessionSummary> = Vec::new();

    for (family_root, family_files) in &families {
        // Skip families with no recently-modified files.
        let family_is_live = family_files
            .iter()
            .any(|p| file_mtime(p).map(|dt| dt >= cutoff).unwrap_or(false));
        if !family_is_live {
            continue;
        }

        // Build or retrieve family-wide links for this family.
        if !family_links_cache.contains_key(family_root) {
            let entry = build_family_links(family_root, family_files);
            family_links_cache.insert(family_root.clone(), entry);
        }
        let (links, root_session_id) = family_links_cache.get(family_root).unwrap();

        // The root file exists only if it appears in the family_files list
        // (i.e. collect_jsonl_files found it on disk).
        let root_exists = family_files.iter().any(|p| p == family_root);
        // Fallback parent_id for sub-agents whose true issuer cannot be
        // resolved: use root_session_id when the root exists, else None
        // (orphan → treated as root, per plan edge-case rule).
        let fallback_parent_id: Option<String> =
            if root_exists && !root_session_id.is_empty() {
                Some(root_session_id.clone())
            } else {
                None
            };

        // Build all node records for this family first, then prune.
        let mut family_summaries: Vec<SessionSummary> = Vec::new();

        for path in family_files {
            let file_is_active = file_mtime(path)
                .map(|dt| dt >= cutoff)
                .unwrap_or(false);

            let stats = parse_file_stats(path);
            let session_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();

            let (node_id, parent_id, agent_name) =
                if let Some(agent_id) = subagent_agent_id(path) {
                    let sub_prompt = first_user_prompt(path);
                    let (name, resolved_parent) = resolve_agent_name_and_parent(
                        links,
                        &agent_id,
                        sub_prompt.as_deref(),
                    );
                    let parent_id =
                        resolved_parent.or_else(|| fallback_parent_id.clone());
                    (agent_id, parent_id, Some(name))
                } else {
                    // Top-level session: id = sessionId, no parent.
                    (session_id.clone(), None, None)
                };

            let project = project_root_name(path, stats.project_cwd.as_deref())
                .or_else(|| {
                    stats
                        .project_cwd
                        .as_deref()
                        .and_then(|c| Path::new(c).file_name())
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| "unknown".to_string());

            let total_tokens = stats.input_tokens
                + stats.output_tokens
                + stats.cache_creation
                + stats.cache_read;

            family_summaries.push(SessionSummary {
                session_id,
                id: node_id,
                parent_id,
                project,
                agent_name,
                model: stats.model,
                last_active: stats.last_active.unwrap_or_default(),
                input_tokens: stats.input_tokens,
                output_tokens: stats.output_tokens,
                cache_creation: stats.cache_creation,
                cache_read: stats.cache_read,
                total_tokens,
                active: file_is_active,
            });
        }

        // Prune: keep only nodes whose subtree contains at least one active node.
        let include_tuples: Vec<(String, Option<String>, bool)> = family_summaries
            .iter()
            .map(|s| (s.id.clone(), s.parent_id.clone(), s.active))
            .collect();
        let include = compute_included(&include_tuples);

        for summary in family_summaries {
            if include.contains(&summary.id) {
                sessions.push(summary);
            }
        }
    }

    // Most-recently-active session first.
    sessions.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    sessions
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── encode_project_path ──────────────────────────────────────────────────

    #[test]
    fn encode_typical_windows_path() {
        let path = Path::new(r"C:\Users\Ro11-ROG-DEV\source\repos\claude-overlay");
        assert_eq!(
            encode_project_path(path),
            "C--Users-Ro11-ROG-DEV-source-repos-claude-overlay"
        );
    }

    #[test]
    fn encode_path_with_dots() {
        // dots are also replaced
        let path = Path::new(r"C:\repos\my.project");
        assert_eq!(encode_project_path(path), "C--repos-my-project");
    }

    // ── project_root_name ────────────────────────────────────────────────────

    /// Sub-agent in a subfolder cwd → project root name returned.
    ///
    /// Synthetic transcript:
    ///   `C:\Users\test\.claude\projects\C--A-B-claude-overlay\<sid>\subagents\agent-x.jsonl`
    /// with `cwd = C:\A\B\claude-overlay\src-tauri`
    ///
    /// Expected: `"claude-overlay"`.
    #[test]
    fn project_root_name_subagent_in_subfolder() {
        let transcript = Path::new(
            r"C:\Users\test\.claude\projects\C--A-B-claude-overlay\abc123\subagents\agent-x.jsonl",
        );
        let result =
            project_root_name(transcript, Some(r"C:\A\B\claude-overlay\src-tauri"));
        assert_eq!(result, Some("claude-overlay".to_string()));
    }

    /// Root session with cwd exactly at the project root.
    #[test]
    fn project_root_name_root_session_at_root() {
        let transcript = Path::new(
            r"C:\Users\test\.claude\projects\C--A-B-claude-overlay\abc123.jsonl",
        );
        let result = project_root_name(transcript, Some(r"C:\A\B\claude-overlay"));
        assert_eq!(result, Some("claude-overlay".to_string()));
    }

    /// No cwd → `None`.
    #[test]
    fn project_root_name_no_cwd_returns_none() {
        let transcript = Path::new(
            r"C:\Users\test\.claude\projects\C--A-B-claude-overlay\abc123.jsonl",
        );
        assert_eq!(project_root_name(transcript, None), None);
    }

    /// cwd that doesn't match any ancestor of the encoded dir → `None`.
    #[test]
    fn project_root_name_unrelated_cwd_returns_none() {
        let transcript = Path::new(
            r"C:\Users\test\.claude\projects\C--A-B-claude-overlay\abc123.jsonl",
        );
        // cwd is under a completely different tree
        let result = project_root_name(transcript, Some(r"D:\other\project"));
        assert_eq!(result, None);
    }

    // ── subagent_agent_id ────────────────────────────────────────────────────

    #[test]
    fn subagent_agent_id_returns_id_for_agent_file() {
        let path = Path::new(r"C:\proj\sid\subagents\agent-xyz789abc.jsonl");
        assert_eq!(
            subagent_agent_id(path),
            Some("xyz789abc".to_string())
        );
    }

    #[test]
    fn subagent_agent_id_returns_none_for_root_session() {
        let path = Path::new(r"C:\proj\abc123.jsonl");
        assert_eq!(subagent_agent_id(path), None);
    }

    // ── family_root_path ─────────────────────────────────────────────────────

    #[test]
    fn family_root_path_for_subagent_is_parent_session() {
        let path = Path::new(r"C:\proj\sid123\subagents\agent-aaa.jsonl");
        let root = family_root_path(path);
        assert_eq!(root, PathBuf::from(r"C:\proj\sid123.jsonl"));
    }

    #[test]
    fn family_root_path_for_root_session_is_itself() {
        let path = Path::new(r"C:\proj\sid123.jsonl");
        let root = family_root_path(path);
        assert_eq!(root, PathBuf::from(r"C:\proj\sid123.jsonl"));
    }

    // ── id / parentId: top-level vs sub-agent ─────────────────────────────────

    /// A top-level session has no parent in the links and falls through to
    /// the `None` branch in `list_active` (not going through
    /// `resolve_agent_name_and_parent` at all).  We test the sub-agent path.
    #[test]
    fn top_level_node_yields_none_parent_from_empty_links() {
        // An unrecognised agent_id with no links and no prompt → ("subagent", None).
        let links = FamilyLinks::default();
        let (name, parent) = resolve_agent_name_and_parent(&links, "some-id", None);
        assert_eq!(name, "subagent");
        assert_eq!(parent, None);
    }

    // ── grandchild parentId via by_agent_id ──────────────────────────────────

    /// Root spawns sub-agent "aaa"; sub-agent "aaa" spawns grandchild "bbb".
    /// The grandchild's `parentId` must resolve to "aaa", not the root session.
    #[test]
    fn grandchild_parent_id_is_subagent_not_root() {
        let mut links = FamilyLinks::default();
        // bbb was spawned by aaa (issuer = "aaa")
        links.by_agent_id.insert(
            "bbb".to_string(),
            ("worker".to_string(), "aaa".to_string()),
        );
        // aaa was spawned by the root (issuer = "root-session-uuid")
        links.by_agent_id.insert(
            "aaa".to_string(),
            ("orchestrator".to_string(), "root-session-uuid".to_string()),
        );

        let (name, parent) = resolve_agent_name_and_parent(&links, "bbb", None);
        assert_eq!(name, "worker");
        // Must be the sub-agent "aaa", NOT the root session id.
        assert_eq!(parent, Some("aaa".to_string()));
    }

    // ── parentId via by_prompt (child still running) ──────────────────────────

    #[test]
    fn child_parent_resolved_via_exact_prompt_match() {
        let mut links = FamilyLinks::default();
        let prompt =
            "Implement the backend portion of the approved plan. Follow conventions exactly.";
        links.by_prompt.push((
            prompt.to_string(),
            "developer-backend".to_string(),
            "root-session-uuid".to_string(),
        ));

        let (name, parent) =
            resolve_agent_name_and_parent(&links, "child-id", Some(prompt));
        assert_eq!(name, "developer-backend");
        assert_eq!(parent, Some("root-session-uuid".to_string()));
    }

    // ── missing-root orphan → parentId None ──────────────────────────────────

    #[test]
    fn missing_root_orphan_gets_none_parent_id() {
        // When the root file is missing, fallback_parent_id is None.
        // An unresolved sub-agent therefore gets parent_id = None.
        let links = FamilyLinks::default();
        let fallback_parent_id: Option<String> = None; // root_exists = false

        let (_name, resolved_parent) =
            resolve_agent_name_and_parent(&links, "orphan-id", None);
        let parent_id = resolved_parent.or_else(|| fallback_parent_id.clone());
        assert_eq!(parent_id, None);
    }

    // ── when root exists, unresolved child falls back to root session id ──────

    #[test]
    fn unresolved_child_falls_back_to_root_session_id() {
        let links = FamilyLinks::default();
        let fallback_parent_id: Option<String> = Some("root-session-uuid".to_string());

        let (_name, resolved_parent) =
            resolve_agent_name_and_parent(&links, "unknown-child", None);
        let parent_id = resolved_parent.or_else(|| fallback_parent_id.clone());
        assert_eq!(parent_id, Some("root-session-uuid".to_string()));
    }

    // ── extract_agent_id ─────────────────────────────────────────────────────

    #[test]
    fn extract_agent_id_from_typical_result_text() {
        let text = r#""agentId: ad8fe3bd855c6c543 (use SendMessage tool to communicate)""#;
        assert_eq!(
            extract_agent_id(text),
            Some("ad8fe3bd855c6c543".to_string())
        );
    }

    #[test]
    fn extract_agent_id_none_when_absent() {
        assert_eq!(extract_agent_id("no agent identifier here"), None);
    }

    #[test]
    fn extract_agent_id_empty_run_returns_none() {
        // "agentId:" followed by non-alphanumeric only
        assert_eq!(extract_agent_id("agentId: !!!"), None);
    }

    // ── compute_included (prune / subtree logic) ─────────────────────────────

    fn node(id: &str, parent: Option<&str>, active: bool) -> (String, Option<String>, bool) {
        (id.to_string(), parent.map(|s| s.to_string()), active)
    }

    #[test]
    fn prune_root_stale_suba_active_both_included() {
        // root(stale) → subA(active): both must be included (root is ancestor).
        let nodes = vec![node("root", None, false), node("subA", Some("root"), true)];
        let include = compute_included(&nodes);
        assert!(include.contains("root"), "root (stale ancestor) must be included");
        assert!(include.contains("subA"), "subA (active) must be included");
        assert_eq!(include.len(), 2);
    }

    #[test]
    fn prune_stale_sibling_leaf_excluded() {
        // root(stale) → subA(active), subB(stale leaf): subB excluded.
        let nodes = vec![
            node("root", None, false),
            node("subA", Some("root"), true),
            node("subB", Some("root"), false),
        ];
        let include = compute_included(&nodes);
        assert!(include.contains("root"), "root must be included (ancestor of active subA)");
        assert!(include.contains("subA"), "subA (active) must be included");
        assert!(!include.contains("subB"), "subB (stale leaf) must be excluded");
        assert_eq!(include.len(), 2);
    }

    #[test]
    fn prune_active_grandchild_pulls_in_all_ancestors() {
        // root(stale) → subA(stale) → grandchild(active): all three included.
        let nodes = vec![
            node("root", None, false),
            node("subA", Some("root"), false),
            node("grandchild", Some("subA"), true),
        ];
        let include = compute_included(&nodes);
        assert!(include.contains("root"), "root must be included (ancestor)");
        assert!(include.contains("subA"), "subA must be included (ancestor)");
        assert!(include.contains("grandchild"), "grandchild (active) must be included");
        assert_eq!(include.len(), 3);
    }

    #[test]
    fn prune_all_stale_yields_empty_include() {
        // Family with every node stale: empty include set.
        let nodes = vec![
            node("root", None, false),
            node("subA", Some("root"), false),
            node("subB", Some("root"), false),
        ];
        let include = compute_included(&nodes);
        assert!(include.is_empty(), "all-stale family must yield empty include set");
    }

    #[test]
    fn prune_cycle_in_parent_links_does_not_loop_forever() {
        // A → B → A (cycle): must terminate within hop cap.
        let nodes = vec![node("A", Some("B"), true), node("B", Some("A"), false)];
        // Just verify it returns without hanging; both reachable via the cycle.
        let include = compute_included(&nodes);
        assert!(include.contains("A"), "active node A must be included");
        assert!(include.contains("B"), "ancestor B (reachable within hop cap) must be included");
    }
}
