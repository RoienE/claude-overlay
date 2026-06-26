//! Fallback data source: aggregate token usage from local Claude Code JSONL transcripts.
//!
//! Claude Code writes one JSONL file per session to:
//!   Windows: %USERPROFILE%\.claude\projects\**\*.jsonl
//!   Linux:   ~/.claude/projects/**/*.jsonl
//!
//! This source cannot produce authoritative utilization % (no plan caps), but it
//! shows "tokens used this session / this week" when the API is unreachable.

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use crate::config::CLAUDE_CONFIG_DIR_ENV;
use crate::model::FallbackUsage;

/// Find the `.claude` directory, honouring `CLAUDE_CONFIG_DIR`.
pub(crate) fn claude_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(CLAUDE_CONFIG_DIR_ENV) {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        let p = PathBuf::from(profile).join(".claude");
        if p.is_dir() {
            return Some(p);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home).join(".claude");
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

/// Partial shape of a JSONL transcript record.
#[derive(Debug, Deserialize)]
pub(crate) struct JsRecord {
    pub(crate) timestamp: Option<String>,
    pub(crate) message: Option<JsMessage>,
    /// Working directory recorded at the top level of each event (additive field).
    #[serde(default)]
    pub(crate) cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JsMessage {
    pub(crate) role: Option<String>,
    pub(crate) usage: Option<JsUsage>,
    /// Model name recorded on assistant messages (additive field).
    #[serde(default)]
    pub(crate) model: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JsUsage {
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    pub(crate) cache_creation_input_tokens: Option<u64>,
    pub(crate) cache_read_input_tokens: Option<u64>,
}

/// Recursively collect all `.jsonl` files under the `projects/` subdirectory.
pub(crate) fn collect_jsonl_files(base: &PathBuf) -> Vec<PathBuf> {
    let projects_dir = base.join("projects");
    if !projects_dir.is_dir() {
        return vec![];
    }
    let mut files = Vec::new();
    collect_recursive(&projects_dir, &mut files);
    files
}

fn collect_recursive(dir: &PathBuf, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_recursive(&path, out);
        } else if path.extension().map_or(false, |e| e == "jsonl") {
            out.push(path);
        }
    }
}

/// Aggregate token usage from JSONL files for rolling 5h and 7d windows.
pub fn aggregate() -> FallbackUsage {
    let Some(base) = claude_dir() else {
        return FallbackUsage::default();
    };

    let files = collect_jsonl_files(&base);
    let now = Utc::now();
    let cutoff_5h = now - Duration::hours(5);
    let cutoff_7d = now - Duration::days(7);

    let mut usage = FallbackUsage::default();

    for file in files {
        // Quick file-level check: if the file was last modified > 7 days ago, skip entirely.
        if let Ok(meta) = fs::metadata(&file) {
            if let Ok(modified) = meta.modified() {
                let modified_dt: DateTime<Utc> = modified.into();
                if modified_dt < cutoff_7d {
                    continue;
                }
            }
        }

        aggregate_file(&file, &cutoff_5h, &cutoff_7d, &mut usage);
    }

    usage
}

fn aggregate_file(
    path: &PathBuf,
    cutoff_5h: &DateTime<Utc>,
    cutoff_7d: &DateTime<Utc>,
    out: &mut FallbackUsage,
) {
    let Ok(file) = fs::File::open(path) else {
        return;
    };
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<JsRecord>(&line) else {
            continue;
        };

        let timestamp: Option<DateTime<Utc>> = record
            .timestamp
            .as_deref()
            .and_then(|s| s.parse().ok());

        let Some(ts) = timestamp else {
            continue;
        };

        // Only count assistant messages with usage data.
        let msg = match &record.message {
            Some(m) if m.role.as_deref() == Some("assistant") => m,
            _ => continue,
        };

        let u = match &msg.usage {
            Some(u) => u,
            None => continue,
        };

        let in_tok = u.input_tokens.unwrap_or(0);
        let out_tok = u.output_tokens.unwrap_or(0);
        let cache_create = u.cache_creation_input_tokens.unwrap_or(0);
        let cache_read = u.cache_read_input_tokens.unwrap_or(0);

        if &ts >= cutoff_7d {
            out.input_tokens_7d += in_tok;
            out.output_tokens_7d += out_tok;
            out.cache_creation_7d += cache_create;
            out.cache_read_7d += cache_read;
        }

        if &ts >= cutoff_5h {
            out.input_tokens_5h += in_tok;
            out.output_tokens_5h += out_tok;
            out.cache_creation_5h += cache_create;
            out.cache_read_5h += cache_read;
        }
    }
}

/// Convert a `FallbackUsage` into approximate `QuotaWindow` list for the UI.
/// Utilization is shown relative to rough known maximums (not authoritative).
pub fn to_quota_windows(usage: &FallbackUsage) -> Vec<crate::model::QuotaWindow> {
    // Very rough token caps as reference points for display only.
    // These are NOT plan limits — just a scale reference.
    const ROUGH_5H_CAP: f32 = 100_000.0;
    const ROUGH_7D_CAP: f32 = 1_000_000.0;

    let total_5h = (usage.input_tokens_5h + usage.output_tokens_5h) as f32;
    let total_7d = (usage.input_tokens_7d + usage.output_tokens_7d) as f32;

    let util_5h = (total_5h / ROUGH_5H_CAP * 100.0).min(100.0);
    let util_7d = (total_7d / ROUGH_7D_CAP * 100.0).min(100.0);

    vec![
        crate::model::QuotaWindow {
            key: "five_hour".to_string(),
            label: "5-hour session (est.)".to_string(),
            utilization: util_5h,
            resets_at: None,
        },
        crate::model::QuotaWindow {
            key: "seven_day".to_string(),
            label: "Weekly (est.)".to_string(),
            utilization: util_7d,
            resets_at: None,
        },
    ]
}
