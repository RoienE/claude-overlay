//! Reads the Claude Code OAuth credentials (read-only).
//!
//! Resolution order on **Windows / Linux**:
//!   1. `$CLAUDE_CONFIG_DIR/.credentials.json`  (if env var is set)
//!   2. `$USERPROFILE\.claude\.credentials.json`  (Windows default)
//!   3. `$HOME/.claude/.credentials.json`          (Linux fallback)
//!
//! Resolution order on **macOS**:
//!   1. `$CLAUDE_CONFIG_DIR/.credentials.json`  (if env var is set)
//!   2. `$HOME/.claude/.credentials.json`          (file fallback for SSH-style installs)
//!   3. macOS Keychain — generic password, service `Claude Code-credentials`
//!      (normal Claude Code macOS install stores the token here, not in a file)

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{CLAUDE_CONFIG_DIR_ENV, CLAUDE_DIR_NAME, CREDENTIALS_FILENAME};

/// Service name used by Claude Code when writing to the macOS Keychain.
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Raw shape of `.credentials.json` (partial — we only read what we need).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCredentials {
    pub claude_ai_oauth: Option<ClaudeOauthEntry>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeOauthEntry {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    /// Epoch milliseconds
    pub expires_at: Option<i64>,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
}

/// Resolved credentials ready for use.
#[derive(Debug, Clone)]
pub struct ResolvedCredentials {
    pub access_token: String,
    pub expires_at_ms: Option<i64>,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    pub is_expired: bool,
}

/// Resolve the path to `.credentials.json`, honouring `CLAUDE_CONFIG_DIR`.
pub fn credentials_path() -> Option<PathBuf> {
    // 1. Env-var override
    if let Ok(dir) = std::env::var(CLAUDE_CONFIG_DIR_ENV) {
        let p = PathBuf::from(dir).join(CREDENTIALS_FILENAME);
        if p.exists() {
            return Some(p);
        }
    }

    // 2. Windows: %USERPROFILE%\.claude\.credentials.json
    if let Ok(profile) = std::env::var("USERPROFILE") {
        let p = PathBuf::from(profile)
            .join(CLAUDE_DIR_NAME)
            .join(CREDENTIALS_FILENAME);
        if p.exists() {
            return Some(p);
        }
    }

    // 3. Unix: $HOME/.claude/.credentials.json
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home)
            .join(CLAUDE_DIR_NAME)
            .join(CREDENTIALS_FILENAME);
        if p.exists() {
            return Some(p);
        }
    }

    None
}

/// Parse a JSON string (from a credentials file or the Keychain) into
/// `ResolvedCredentials`, applying expiry detection.
///
/// `source` is a human-readable label used in error messages only — it must
/// never contain token values.
pub fn parse_credentials_json(json: &str, source: &str) -> Result<ResolvedCredentials> {
    let parsed: ClaudeCredentials = serde_json::from_str(json)
        .with_context(|| format!("Failed to parse credentials JSON from {source}"))?;

    let oauth = parsed
        .claude_ai_oauth
        .context("Missing 'claudeAiOauth' field in credentials")?;

    let access_token = oauth
        .access_token
        .context("Missing 'accessToken' in claudeAiOauth")?;

    if access_token.is_empty() {
        bail!("accessToken is empty in credentials from {source}");
    }

    let is_expired = oauth.expires_at.map_or(false, |exp| {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        now_ms > exp
    });

    Ok(ResolvedCredentials {
        access_token,
        expires_at_ms: oauth.expires_at,
        subscription_type: oauth.subscription_type,
        rate_limit_tier: oauth.rate_limit_tier,
        is_expired,
    })
}

/// Read the Keychain generic password for service `Claude Code-credentials`.
///
/// Returns `Ok(Some(json))` when the item is present and readable.
/// Returns `Ok(None)` when the item does not exist or access is denied
/// (caller should fall through to AuthExpired rather than aborting).
/// Returns `Err` only for unexpected / programmer errors that warrant logging.
#[cfg(target_os = "macos")]
fn read_keychain_json() -> Result<Option<String>> {
    use security_framework::passwords::get_generic_password;

    match get_generic_password(KEYCHAIN_SERVICE, "") {
        Ok(bytes) => {
            let json = String::from_utf8(bytes)
                .context("Keychain credential bytes are not valid UTF-8")?;
            let trimmed = json.trim().to_owned();
            if trimmed.is_empty() {
                log::warn!("Keychain item for '{}' is empty", KEYCHAIN_SERVICE);
                Ok(None)
            } else {
                Ok(Some(trimmed))
            }
        }
        Err(e) => {
            // errSecItemNotFound (-25300) and errSecUserCanceled (-128) / access-denied
            // are both "not available right now" — log at debug and return None so the
            // caller degrades gracefully to AuthExpired.
            log::debug!(
                "Keychain lookup for '{}' returned no item: {}",
                KEYCHAIN_SERVICE,
                e
            );
            Ok(None)
        }
    }
}

/// Read and parse the credentials, returning a `ResolvedCredentials`.
///
/// On Windows / Linux: tries the credentials file only.
/// On macOS: tries the credentials file first (SSH-style fallback), then the
/// macOS Keychain (normal install path).
///
/// Returns `Err` if no credential source is found or the token is absent/invalid.
pub fn read_credentials() -> Result<ResolvedCredentials> {
    // --- File path (all platforms) ---
    if let Some(path) = credentials_path() {
        log::debug!("Reading credentials from file: {}", path.display());
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read credentials file: {}", path.display()))?;
        return parse_credentials_json(&raw, &path.display().to_string());
    }

    // --- macOS Keychain fallback ---
    #[cfg(target_os = "macos")]
    {
        log::debug!(
            "No credentials file found; trying Keychain service '{}'",
            KEYCHAIN_SERVICE
        );
        match read_keychain_json()? {
            Some(json) => {
                return parse_credentials_json(&json, "macOS Keychain");
            }
            None => {
                log::warn!(
                    "Keychain service '{}' not found or not accessible. \
                     Is Claude Code installed and logged in?",
                    KEYCHAIN_SERVICE
                );
            }
        }
    }

    bail!(
        "Claude credentials not found. \
         Is Claude Code installed and logged in? \
         On macOS, ensure the app has Keychain access."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── existing file-path tests (unchanged) ──────────────────────────────

    #[test]
    fn parses_valid_credentials() {
        let json = r#"{
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-test",
                "refreshToken": "sk-ant-ort01-test",
                "expiresAt": 9999999999999,
                "subscriptionType": "max",
                "rateLimitTier": "max_20x"
            }
        }"#;
        let creds: ClaudeCredentials = serde_json::from_str(json).unwrap();
        let oauth = creds.claude_ai_oauth.unwrap();
        assert_eq!(oauth.access_token.unwrap(), "sk-ant-oat01-test");
        assert_eq!(oauth.subscription_type.unwrap(), "max");
        assert_eq!(oauth.rate_limit_tier.unwrap(), "max_20x");
    }

    #[test]
    fn missing_access_token_is_handled() {
        let json = r#"{ "claudeAiOauth": {} }"#;
        let creds: ClaudeCredentials = serde_json::from_str(json).unwrap();
        let oauth = creds.claude_ai_oauth.unwrap();
        assert!(oauth.access_token.is_none());
    }

    #[test]
    fn missing_oauth_field_is_handled() {
        let json = r#"{}"#;
        let creds: ClaudeCredentials = serde_json::from_str(json).unwrap();
        assert!(creds.claude_ai_oauth.is_none());
    }

    #[test]
    fn expired_token_detected() {
        // expiresAt in the past (epoch ms 1000 = 1970-01-01T00:00:01Z)
        let json = r#"{
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-test",
                "expiresAt": 1000
            }
        }"#;
        let creds: ClaudeCredentials = serde_json::from_str(json).unwrap();
        let oauth = creds.claude_ai_oauth.unwrap();
        let expires_at = oauth.expires_at.unwrap();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        assert!(now_ms > expires_at, "Token with epoch 1000ms should be expired");
    }

    // ── shared parse_credentials_json tests (cover the Keychain code path) ─

    /// The JSON shape returned by `security find-generic-password` / the Keychain
    /// is identical to the file format — this test exercises the shared parse path
    /// with a Keychain-shaped input (including leading/trailing whitespace that
    /// `read_keychain_json` trims).
    #[test]
    fn parse_credentials_json_valid_keychain_shape() {
        let json = r#"  {
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-keychain",
                "refreshToken": "sk-ant-ort01-keychain",
                "expiresAt": 9999999999999,
                "subscriptionType": "pro",
                "rateLimitTier": "pro_5x"
            }
        }  "#;
        let resolved = parse_credentials_json(json.trim(), "macOS Keychain").unwrap();
        assert_eq!(resolved.access_token, "sk-ant-oat01-keychain");
        assert_eq!(resolved.subscription_type.unwrap(), "pro");
        assert_eq!(resolved.rate_limit_tier.unwrap(), "pro_5x");
        assert!(!resolved.is_expired);
    }

    #[test]
    fn parse_credentials_json_expired_token() {
        let json = r#"{
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-expired",
                "expiresAt": 1000
            }
        }"#;
        let resolved = parse_credentials_json(json, "macOS Keychain").unwrap();
        assert_eq!(resolved.access_token, "sk-ant-oat01-expired");
        assert!(resolved.is_expired, "Token with past expiresAt should be marked expired");
    }

    #[test]
    fn parse_credentials_json_missing_oauth_field() {
        let json = r#"{}"#;
        let err = parse_credentials_json(json, "macOS Keychain").unwrap_err();
        assert!(
            err.to_string().contains("claudeAiOauth"),
            "Error should mention the missing field; got: {err}"
        );
    }

    #[test]
    fn parse_credentials_json_empty_access_token() {
        let json = r#"{ "claudeAiOauth": { "accessToken": "" } }"#;
        let err = parse_credentials_json(json, "macOS Keychain").unwrap_err();
        assert!(
            err.to_string().contains("empty"),
            "Error should mention empty token; got: {err}"
        );
    }

    #[test]
    fn parse_credentials_json_missing_access_token() {
        let json = r#"{ "claudeAiOauth": {} }"#;
        let err = parse_credentials_json(json, "macOS Keychain").unwrap_err();
        assert!(
            err.to_string().contains("accessToken"),
            "Error should mention missing accessToken; got: {err}"
        );
    }

    #[test]
    fn parse_credentials_json_no_expires_at_is_not_expired() {
        let json = r#"{
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-no-expiry"
            }
        }"#;
        let resolved = parse_credentials_json(json, "macOS Keychain").unwrap();
        assert!(!resolved.is_expired, "Token with no expiresAt should not be marked expired");
        assert!(resolved.expires_at_ms.is_none());
    }
}
