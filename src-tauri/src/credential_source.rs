//! Reads the Claude OAuth credentials (read-only).
//!
//! Resolution order, first source with a **fresh** token wins:
//!   1. `$CLAUDE_CONFIG_DIR/.credentials.json`, else `~/.claude/.credentials.json`
//!      (Claude Code CLI login; `%USERPROFILE%` on Windows, `$HOME` elsewhere)
//!   2. **macOS only:** Keychain — generic password, service `Claude Code-credentials`
//!      (normal Claude Code macOS install stores the token here, not in a file)
//!   3. Claude Desktop's own login (see `desktop_credentials`): the encrypted token
//!      cache in its `config.json`. The CLI login always wins; there is deliberately
//!      no setting to pick a source.
//!
//! A source whose token is **expired**, missing or unparsable never ends the search:
//! the next source gets its turn (on macOS the Keychain is the store Claude Code
//! refreshes, so a stale file must not pin the app to an expired token). If every
//! token found is expired, the one that expires latest is returned, so the poller
//! still reports `AuthExpired`. A machine without Claude Desktop records no Desktop
//! attempt at all, so CLI-only users keep their existing diagnostics.
//!
//! The Keychain is read three ways, in order (see `read_keychain_json`):
//!   a. `/usr/bin/security find-generic-password` — the item's ACL is created by
//!      Claude Code and typically trusts only the process that wrote it, so an
//!      in-process read from an unrelated (and unsigned) app is refused or prompts.
//!      Going through Apple's `security` binary presents *its* code identity to the
//!      Security Server, which is what the item's ACL usually already trusts.
//!   b. In-process `SecItemCopyMatching` against the legacy (login) keychain.
//!   c. The same query against the data-protection keychain, where Apple steers new
//!      code and where the item would live after a future migration.
//!
//! Every failed attempt is recorded as a [`CredentialFailure`] carrying a closed
//! vocabulary of stage/reason codes plus the raw `OSStatus` or exit code. That split
//! is deliberate: raw OS error text embeds home-directory paths and account names, so
//! it goes to the local log but **never** to telemetry — only the codes travel. No log
//! line, error or test output ever carries a token, password or key.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{CLAUDE_CONFIG_DIR_ENV, CLAUDE_DIR_NAME, CREDENTIALS_FILENAME};
use crate::desktop_credentials::{self, KeyProvider, Os, SystemKeys};

/// Asked right before the first read of another app's secret (macOS: the
/// `Claude Safe Storage` Keychain item). Returns `false` when the user declined, which
/// skips that source. Shared with the poller's blocking task, hence `Send + Sync`.
pub type ConsentGate = Arc<dyn Fn() -> bool + Send + Sync>;

/// Service name used by Claude Code when writing to the macOS Keychain.
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Apple's keychain CLI. Absolute path on purpose — never resolved via `PATH`.
#[cfg(target_os = "macos")]
const SECURITY_CLI: &str = "/usr/bin/security";

/// How long `security` may run before it is killed. macOS 26 (Tahoe) can leave it
/// blocked on a SecurityAgent prompt that never appears; the poller must not hang.
#[cfg(target_os = "macos")]
const KEYCHAIN_CLI_TIMEOUT_SECS: u64 = 5;

// Security framework result codes we classify by name rather than by number.
#[cfg(target_os = "macos")]
const ERR_SEC_USER_CANCELED: i32 = -128;
#[cfg(target_os = "macos")]
const ERR_SEC_AUTH_FAILED: i32 = -25293;
#[cfg(target_os = "macos")]
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
#[cfg(target_os = "macos")]
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25308;
#[cfg(target_os = "macos")]
const ERR_SEC_MISSING_ENTITLEMENT: i32 = -34018;

// ── Diagnosis vocabulary ──────────────────────────────────────────────────────

/// Which credential source produced a failure.
///
/// The `as_str` values are a stable wire vocabulary: they are sent as telemetry
/// attributes and queried in Grafana, so renaming one is a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStage {
    /// Reading the credentials file off disk.
    FileRead,
    /// Parsing the credentials file's contents.
    FileParse,
    /// The file parsed, but its token is already expired.
    FileExpired,
    /// Keychain read via `/usr/bin/security`.
    KeychainCli,
    /// In-process Keychain read against the legacy (login) keychain.
    KeychainLegacy,
    /// In-process Keychain read against the data-protection keychain.
    KeychainDataProtection,
    /// Reading Claude Desktop's `config.json`.
    DesktopRead,
    /// Getting the key that protects Claude Desktop's token cache.
    DesktopKey,
    /// Decrypting Claude Desktop's token cache.
    DesktopDecrypt,
    /// Parsing the decrypted Claude Desktop token cache.
    DesktopParse,
    /// Claude Desktop's token parsed, but it is already expired.
    DesktopExpired,
    /// No credential source exists at all.
    NoSource,
}

impl CredentialStage {
    /// Stable machine token for telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FileRead => "file_read",
            Self::FileParse => "file_parse",
            Self::FileExpired => "file_expired",
            Self::KeychainCli => "keychain_cli",
            Self::KeychainLegacy => "keychain_legacy",
            Self::KeychainDataProtection => "keychain_data_protection",
            Self::DesktopRead => "desktop_read",
            Self::DesktopKey => "desktop_key",
            Self::DesktopDecrypt => "desktop_decrypt",
            Self::DesktopParse => "desktop_parse",
            Self::DesktopExpired => "desktop_expired",
            Self::NoSource => "no_source",
        }
    }

    /// Human label for the log line and the UI.
    fn label(self) -> &'static str {
        match self {
            Self::FileRead | Self::FileParse | Self::FileExpired => "Credentials file",
            Self::KeychainCli => "Keychain (security tool)",
            Self::KeychainLegacy => "Keychain",
            Self::KeychainDataProtection => "Keychain (data protection)",
            Self::DesktopRead
            | Self::DesktopDecrypt
            | Self::DesktopParse
            | Self::DesktopExpired => "Claude Desktop login",
            Self::DesktopKey => "Claude Desktop key",
            Self::NoSource => "Credentials",
        }
    }
}

impl fmt::Display for CredentialStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a credential source failed.
///
/// Closed vocabulary on purpose — see the module header. Raw OS error strings are
/// never promoted into this type, so a `CredentialReason` is always safe to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialReason {
    /// The source does not exist (no file, no Keychain item).
    NotFound,
    /// The OS refused access (`errSecAuthFailed`, file permissions, denied ACL).
    PermissionDenied,
    /// A prompt was required that could not be shown (`errSecInteractionNotAllowed`).
    InteractionRequired,
    /// The read exceeded its deadline and was abandoned.
    Timeout,
    /// `/usr/bin/security` is not present.
    CliMissing,
    /// `/usr/bin/security` ran but failed for a reason we do not classify further.
    CliFailed,
    /// The secret is not valid UTF-8.
    InvalidUtf8,
    /// The payload is not valid JSON.
    MalformedJson,
    /// JSON parsed but has no `claudeAiOauth` object.
    MissingOauthField,
    /// `claudeAiOauth` has no `accessToken`.
    MissingToken,
    /// `accessToken` is present but empty.
    EmptyToken,
    /// The token parsed fine but its expiry has passed.
    Expired,
    /// The encrypted blob did not decrypt (wrong or rotated key, corrupt data).
    DecryptFailed,
    /// The blob uses an encryption scheme this build does not implement.
    UnsupportedFormat,
    /// The user declined the notice shown before reading another app's secret.
    Skipped,
    /// An I/O error that is none of the above.
    Unreadable,
    /// Anything we could not classify.
    Unknown,
}

impl CredentialReason {
    /// Stable machine token for telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::PermissionDenied => "permission_denied",
            Self::InteractionRequired => "interaction_required",
            Self::Timeout => "timeout",
            Self::CliMissing => "cli_missing",
            Self::CliFailed => "cli_failed",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::MalformedJson => "malformed_json",
            Self::MissingOauthField => "missing_oauth_field",
            Self::MissingToken => "missing_token",
            Self::EmptyToken => "empty_token",
            Self::Expired => "expired",
            Self::DecryptFailed => "decrypt_failed",
            Self::UnsupportedFormat => "unsupported_format",
            Self::Skipped => "skipped",
            Self::Unreadable => "unreadable",
            Self::Unknown => "unknown",
        }
    }

    /// Classify a file-system error without ever keeping its text (which embeds paths).
    pub(crate) fn from_io_kind(kind: std::io::ErrorKind) -> Self {
        match kind {
            std::io::ErrorKind::NotFound => Self::NotFound,
            std::io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            _ => Self::Unreadable,
        }
    }

    /// Human phrase completing the stage label, e.g. "Keychain access denied".
    fn phrase(self) -> &'static str {
        match self {
            Self::NotFound => "not found",
            Self::PermissionDenied => "access denied",
            Self::InteractionRequired => "needs a permission prompt that cannot be shown",
            Self::Timeout => "timed out",
            Self::CliMissing => "unavailable (/usr/bin/security is missing)",
            Self::CliFailed => "could not be read",
            Self::InvalidUtf8 => "returned unreadable data",
            Self::MalformedJson => "contains malformed JSON",
            Self::MissingOauthField => "is missing the claudeAiOauth field",
            Self::MissingToken => "is missing accessToken",
            Self::EmptyToken => "has an empty accessToken",
            Self::Expired => "token has expired",
            Self::DecryptFailed => "could not be decrypted",
            Self::UnsupportedFormat => "uses an unsupported encryption format",
            Self::Skipped => "skipped (restart the overlay to be asked again)",
            Self::Unreadable => "could not be read",
            Self::Unknown => "failed for an unknown reason",
        }
    }
}

impl fmt::Display for CredentialReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One failed credential-source attempt.
///
/// Contains only codes — never a path, account name, error string or token — so the
/// whole struct can be handed to telemetry as-is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialFailure {
    pub stage: CredentialStage,
    pub reason: CredentialReason,
    /// macOS `OSStatus` when the failure came from the Security framework.
    pub os_status: Option<i32>,
    /// Process exit code when the failure came from the `security` CLI.
    pub exit_code: Option<i32>,
}

impl CredentialFailure {
    pub fn new(stage: CredentialStage, reason: CredentialReason) -> Self {
        Self {
            stage,
            reason,
            os_status: None,
            exit_code: None,
        }
    }

    #[cfg(target_os = "macos")]
    fn with_os_status(mut self, code: i32) -> Self {
        self.os_status = Some(code);
        self
    }

    #[cfg(target_os = "macos")]
    fn with_exit_code(mut self, code: Option<i32>) -> Self {
        self.exit_code = code;
        self
    }
}

impl fmt::Display for CredentialFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.stage.label(), self.reason.phrase())?;
        if let Some(code) = self.os_status {
            write!(f, " (OSStatus {code})")?;
        }
        if let Some(code) = self.exit_code {
            write!(f, " (exit {code})")?;
        }
        Ok(())
    }
}

/// Every credential source failed. Carries the full attempt trail, newest last.
#[derive(Debug, Clone)]
pub struct CredentialError {
    pub attempts: Vec<CredentialFailure>,
}

impl CredentialError {
    /// The failure worth showing and reporting: the deepest source tried, i.e. the
    /// last attempt. Never panics — the trail is never constructed empty.
    pub fn primary(&self) -> CredentialFailure {
        self.attempts.last().copied().unwrap_or_else(|| {
            CredentialFailure::new(CredentialStage::NoSource, CredentialReason::NotFound)
        })
    }
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.attempts.is_empty() {
            return f.write_str("no credential source available");
        }
        let trail: Vec<String> = self.attempts.iter().map(ToString::to_string).collect();
        f.write_str(&trail.join("; "))
    }
}

impl std::error::Error for CredentialError {}

// ── Credential shapes ─────────────────────────────────────────────────────────

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

// ── Parsing ───────────────────────────────────────────────────────────────────

/// Parse credential JSON into `ResolvedCredentials`, reporting failures as codes.
///
/// This is the real implementation; `parse_credentials_json` wraps it for callers
/// that want a human-readable `anyhow` error instead.
pub fn parse_classified(json: &str) -> std::result::Result<ResolvedCredentials, CredentialReason> {
    let parsed: ClaudeCredentials =
        serde_json::from_str(json).map_err(|_| CredentialReason::MalformedJson)?;

    let oauth = parsed
        .claude_ai_oauth
        .ok_or(CredentialReason::MissingOauthField)?;

    let access_token = oauth.access_token.ok_or(CredentialReason::MissingToken)?;

    if access_token.is_empty() {
        return Err(CredentialReason::EmptyToken);
    }

    Ok(ResolvedCredentials {
        access_token,
        expires_at_ms: oauth.expires_at,
        subscription_type: oauth.subscription_type,
        rate_limit_tier: oauth.rate_limit_tier,
        is_expired: is_expired(oauth.expires_at),
    })
}

/// Whether an epoch-millisecond expiry has passed. No expiry means "not expired".
pub(crate) fn is_expired(expires_at_ms: Option<i64>) -> bool {
    expires_at_ms.is_some_and(|exp| {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        now_ms > exp
    })
}

/// Parse a JSON string (from a credentials file or the Keychain) into
/// `ResolvedCredentials`, applying expiry detection.
///
/// `source` is a human-readable label used in error messages only — it must
/// never contain token values.
pub fn parse_credentials_json(json: &str, source: &str) -> Result<ResolvedCredentials> {
    parse_classified(json).map_err(|reason| match reason {
        CredentialReason::MalformedJson => {
            anyhow!("Failed to parse credentials JSON from {source}")
        }
        CredentialReason::MissingOauthField => {
            anyhow!("Missing 'claudeAiOauth' field in credentials from {source}")
        }
        CredentialReason::MissingToken => {
            anyhow!("Missing 'accessToken' in claudeAiOauth from {source}")
        }
        CredentialReason::EmptyToken => {
            anyhow!("accessToken is empty in credentials from {source}")
        }
        other => anyhow!("Credentials from {source} unusable: {other}"),
    })
}

// ── macOS Keychain ────────────────────────────────────────────────────────────

/// Map a Security framework `OSStatus` onto our reason vocabulary.
#[cfg(target_os = "macos")]
fn classify_os_status(code: i32) -> CredentialReason {
    match code {
        ERR_SEC_ITEM_NOT_FOUND => CredentialReason::NotFound,
        ERR_SEC_AUTH_FAILED | ERR_SEC_USER_CANCELED | ERR_SEC_MISSING_ENTITLEMENT => {
            CredentialReason::PermissionDenied
        }
        ERR_SEC_INTERACTION_NOT_ALLOWED => CredentialReason::InteractionRequired,
        _ => CredentialReason::Unknown,
    }
}

/// Try every Keychain mechanism in turn, recording each failure.
///
/// Returns the raw credential JSON from the first mechanism that yields one.
#[cfg(target_os = "macos")]
fn read_keychain_json(attempts: &mut Vec<CredentialFailure>) -> Option<String> {
    match keychain_via_cli() {
        Ok(json) => {
            log::info!("Keychain item read via {SECURITY_CLI}");
            return Some(json);
        }
        Err(failure) => attempts.push(failure),
    }

    for stage in [
        CredentialStage::KeychainLegacy,
        CredentialStage::KeychainDataProtection,
    ] {
        match keychain_via_framework(stage) {
            Ok(json) => {
                log::info!("Keychain item read in-process ({})", stage.as_str());
                return Some(json);
            }
            Err(failure) => attempts.push(failure),
        }
    }

    None
}

/// Read the Claude Code Keychain item via the `security` binary.
#[cfg(target_os = "macos")]
fn keychain_via_cli() -> std::result::Result<String, CredentialFailure> {
    security_find_password(
        KEYCHAIN_SERVICE,
        CredentialStage::KeychainCli,
        KEYCHAIN_CLI_TIMEOUT_SECS,
    )
}

/// Read a generic-password item by service name by shelling out to Apple's `security`
/// binary.
///
/// Read-only (`find-generic-password`), never writes or modifies the item. The
/// secret arrives on stdout — never via argv — and is never logged. `stage` labels
/// any failure; `timeout_secs` bounds the wait (the Claude Desktop read allows longer
/// than the CLI one, because there the user is expected to answer a prompt).
#[cfg(target_os = "macos")]
pub(crate) fn security_find_password(
    service: &str,
    stage: CredentialStage,
    timeout_secs: u64,
) -> std::result::Result<String, CredentialFailure> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = match Command::new(SECURITY_CLI)
        .args(["find-generic-password", "-s", service, "-w"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::warn!("{SECURITY_CLI} is not present");
            return Err(CredentialFailure::new(stage, CredentialReason::CliMissing));
        }
        Err(e) => {
            log::warn!("Could not run {SECURITY_CLI}: {e}");
            return Err(CredentialFailure::new(stage, CredentialReason::CliFailed));
        }
    };

    // macOS 26 (Tahoe) can leave `security` blocked on a SecurityAgent prompt that
    // never appears. Bound the wait and kill it, so a wedged Keychain cannot stall
    // the poller — the in-process mechanisms still get their turn afterwards.
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    log::warn!("{SECURITY_CLI} timed out after {timeout_secs}s");
                    return Err(CredentialFailure::new(stage, CredentialReason::Timeout));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                log::warn!("Waiting on {SECURITY_CLI} failed: {e}");
                return Err(CredentialFailure::new(stage, CredentialReason::CliFailed));
            }
        }
    };

    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        // stderr names the reason and never the secret, so it is safe to log locally.
        let stderr = stderr.trim();
        log::warn!("{SECURITY_CLI} find-generic-password failed ({status}): {stderr}");
        let reason = if stderr.contains("could not be found") {
            CredentialReason::NotFound
        } else {
            CredentialReason::CliFailed
        };
        return Err(CredentialFailure::new(stage, reason).with_exit_code(status.code()));
    }

    // Deliberately a strict UTF-8 read: a lossy conversion would corrupt the token
    // rather than fail, and a mangled token is worse than falling through.
    let mut secret = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        if let Err(e) = pipe.read_to_string(&mut secret) {
            log::warn!("{SECURITY_CLI} returned unreadable data: {e}");
            return Err(CredentialFailure::new(stage, CredentialReason::InvalidUtf8));
        }
    }

    let trimmed = secret.trim();
    if trimmed.is_empty() {
        log::warn!("{SECURITY_CLI} returned an empty secret for '{service}'");
        return Err(CredentialFailure::new(stage, CredentialReason::NotFound));
    }

    Ok(trimmed.to_owned())
}

/// Read the Keychain item in-process via `SecItemCopyMatching`.
///
/// `stage` selects the store: [`CredentialStage::KeychainLegacy`] queries the login
/// keychain (where Claude Code writes today), [`CredentialStage::KeychainDataProtection`]
/// the modern data-protection keychain.
#[cfg(target_os = "macos")]
fn keychain_via_framework(stage: CredentialStage) -> std::result::Result<String, CredentialFailure> {
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit, SearchResult};

    // Query by service only — deliberately NOT constraining `kSecAttrAccount`.
    // Claude Code stores the item under the macOS username, while
    // `passwords::get_generic_password` always pins `kSecAttrAccount`, so an empty
    // account string matches nothing (the empty string is a literal, not a wildcard).
    // This mirrors `security find-generic-password -s "Claude Code-credentials" -w`,
    // and still matches an item written with an empty account.
    let mut options = ItemSearchOptions::new();
    options
        .class(ItemClass::generic_password())
        .service(KEYCHAIN_SERVICE)
        .load_data(true)
        .limit(Limit::Max(1));

    if stage == CredentialStage::KeychainDataProtection {
        // Requires the crate's OSX_10_15 feature to emit kSecUseDataProtectionKeychain.
        options.ignore_legacy_keychains();
    }

    let results = match options.search() {
        Ok(results) => results,
        Err(e) => {
            let code = e.code();
            log::warn!(
                "In-process Keychain lookup ({}) for service '{}' failed (OSStatus {}): {}",
                stage.as_str(),
                KEYCHAIN_SERVICE,
                code,
                e
            );
            return Err(
                CredentialFailure::new(stage, classify_os_status(code)).with_os_status(code)
            );
        }
    };

    let Some(bytes) = results.into_iter().find_map(|result| match result {
        SearchResult::Data(bytes) => Some(bytes),
        _ => None,
    }) else {
        log::warn!(
            "Keychain item for '{}' ({}) returned no data",
            KEYCHAIN_SERVICE,
            stage.as_str()
        );
        return Err(CredentialFailure::new(stage, CredentialReason::NotFound));
    };

    let json = match String::from_utf8(bytes) {
        Ok(json) => json,
        Err(_) => {
            log::warn!("Keychain credential bytes are not valid UTF-8");
            return Err(CredentialFailure::new(stage, CredentialReason::InvalidUtf8));
        }
    };

    let trimmed = json.trim();
    if trimmed.is_empty() {
        log::warn!("Keychain item for '{}' is empty", KEYCHAIN_SERVICE);
        return Err(CredentialFailure::new(stage, CredentialReason::NotFound));
    }

    Ok(trimmed.to_owned())
}

// ── Resolution ────────────────────────────────────────────────────────────────

/// Where each credential source lives. Injectable so tests drive the real resolution
/// logic against temp dirs and fake keys, never the user's own files or Keychain.
pub(crate) struct Sources<'a> {
    /// The Claude Code CLI credentials file, if one exists.
    pub cli_file: Option<PathBuf>,
    /// macOS Keychain reader: returns the raw JSON and records its own failures.
    pub keychain: &'a dyn Fn(&mut Vec<CredentialFailure>) -> Option<String>,
    /// Claude Desktop's data folder, if it is installed.
    pub desktop_dir: Option<PathBuf>,
    /// Supplies the key that protects Claude Desktop's token cache.
    pub desktop_keys: &'a dyn KeyProvider,
}

/// Keep whichever expired token expires latest: if nothing fresh turns up it is the
/// closest to being valid, and it is what the poller reports as `AuthExpired`.
fn keep_latest_expiry(slot: &mut Option<ResolvedCredentials>, candidate: ResolvedCredentials) {
    let candidate_wins = match slot {
        Some(current) => candidate.expires_at_ms > current.expires_at_ms,
        None => true,
    };
    if candidate_wins {
        *slot = Some(candidate);
    }
}

/// Walk the sources in order; see the module header for the contract.
pub(crate) fn resolve(sources: &Sources) -> std::result::Result<ResolvedCredentials, CredentialError> {
    let mut attempts: Vec<CredentialFailure> = Vec::new();

    // Tokens that parsed but are already expired. Held back so a fresher source gets a
    // chance first; the latest-expiring one is returned if none is found (the poller
    // turns it into the same AuthExpired state it always did).
    let mut stale_creds: Option<ResolvedCredentials> = None;

    // --- CLI credentials file (all platforms) ---
    if let Some(path) = &sources.cli_file {
        log::debug!("Reading credentials from file: {}", path.display());
        match std::fs::read_to_string(path) {
            Ok(raw) => match parse_classified(&raw) {
                Ok(creds) if !creds.is_expired => return Ok(creds),
                Ok(creds) => {
                    log::warn!(
                        "Credentials file {} holds an expired token; looking for a fresher source",
                        path.display()
                    );
                    attempts.push(CredentialFailure::new(
                        CredentialStage::FileExpired,
                        CredentialReason::Expired,
                    ));
                    keep_latest_expiry(&mut stale_creds, creds);
                }
                Err(reason) => {
                    // A malformed file must not shadow the other sources (the Keychain
                    // on macOS, Claude Desktop everywhere).
                    log::warn!(
                        "Credentials file {} unusable: {}",
                        path.display(),
                        reason.as_str()
                    );
                    attempts.push(CredentialFailure::new(CredentialStage::FileParse, reason));
                }
            },
            Err(e) => {
                let reason = CredentialReason::from_io_kind(e.kind());
                log::warn!("Failed to read credentials file {}: {e}", path.display());
                attempts.push(CredentialFailure::new(CredentialStage::FileRead, reason));
            }
        }
    }

    // --- macOS Keychain (the reader is a no-op elsewhere) ---
    if let Some(json) = (sources.keychain)(&mut attempts) {
        match parse_classified(&json) {
            Ok(creds) if !creds.is_expired => return Ok(creds),
            Ok(creds) => {
                log::warn!("Keychain token is expired");
                attempts.push(CredentialFailure::new(
                    CredentialStage::KeychainLegacy,
                    CredentialReason::Expired,
                ));
                // A live-but-expired Keychain token still beats a stale file one.
                keep_latest_expiry(&mut stale_creds, creds);
            }
            Err(reason) => {
                log::warn!("Keychain credentials unusable: {}", reason.as_str());
                attempts.push(CredentialFailure::new(
                    CredentialStage::KeychainLegacy,
                    reason,
                ));
            }
        }
    }

    // --- Claude Desktop (last: the CLI login always wins) ---
    if let Some(dir) = &sources.desktop_dir {
        if let Some(creds) = desktop_credentials::read_from_dir(
            dir,
            Os::current(),
            sources.desktop_keys,
            &mut attempts,
        ) {
            if !creds.is_expired {
                log::info!("Using the Claude Desktop login");
                return Ok(creds);
            }
            log::warn!("Claude Desktop token is expired; waiting for Desktop to refresh it");
            attempts.push(CredentialFailure::new(
                CredentialStage::DesktopExpired,
                CredentialReason::Expired,
            ));
            keep_latest_expiry(&mut stale_creds, creds);
        }
    }

    if let Some(creds) = stale_creds {
        return Ok(creds);
    }

    if attempts.is_empty() {
        attempts.push(CredentialFailure::new(
            CredentialStage::NoSource,
            CredentialReason::NotFound,
        ));
    }

    let error = CredentialError { attempts };
    log::warn!(
        "Claude credentials not found. Is Claude Code or Claude Desktop installed and logged in? Attempts: {error}"
    );

    Err(error)
}

/// The macOS Keychain source for the real app.
#[cfg(target_os = "macos")]
fn read_system_keychain(attempts: &mut Vec<CredentialFailure>) -> Option<String> {
    log::debug!("Trying Keychain service '{}'", KEYCHAIN_SERVICE);
    read_keychain_json(attempts)
}

/// Read and parse the credentials.
///
/// Order: CLI file, then (macOS) the CLI Keychain item, then Claude Desktop. `gate` is
/// asked before the one read that makes macOS show a Keychain prompt (Claude Desktop's
/// `Claude Safe Storage` key); other platforms never call it.
///
/// On failure returns the full trail of attempts, so the caller can log it locally,
/// show the deepest reason in the UI and report the codes to telemetry.
pub fn read_credentials(gate: &dyn Fn() -> bool) -> std::result::Result<ResolvedCredentials, CredentialError> {
    let desktop_dir = desktop_credentials::desktop_dir();
    let keys = SystemKeys::new(desktop_dir.as_deref(), gate);

    #[cfg(target_os = "macos")]
    let keychain: &dyn Fn(&mut Vec<CredentialFailure>) -> Option<String> = &read_system_keychain;
    #[cfg(not(target_os = "macos"))]
    let keychain: &dyn Fn(&mut Vec<CredentialFailure>) -> Option<String> = &|_| None;

    resolve(&Sources {
        cli_file: credentials_path(),
        keychain,
        desktop_dir: desktop_dir.clone(),
        desktop_keys: &keys,
    })
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

    // ── diagnosis vocabulary ───────────────────────────────────────

    #[test]
    fn parse_classified_maps_each_failure_shape_to_its_reason() {
        let cases: &[(&str, CredentialReason)] = &[
            ("not json at all", CredentialReason::MalformedJson),
            ("{}", CredentialReason::MissingOauthField),
            (r#"{ "claudeAiOauth": {} }"#, CredentialReason::MissingToken),
            (
                r#"{ "claudeAiOauth": { "accessToken": "" } }"#,
                CredentialReason::EmptyToken,
            ),
        ];
        for (json, expected) in cases {
            let reason = parse_classified(json).unwrap_err();
            assert_eq!(reason, *expected, "wrong reason for input: {json}");
        }
    }

    #[test]
    fn parse_classified_accepts_a_valid_token() {
        let json = r#"{ "claudeAiOauth": { "accessToken": "sk-ant-oat01-x", "expiresAt": 9999999999999 } }"#;
        let creds = parse_classified(json).unwrap();
        assert_eq!(creds.access_token, "sk-ant-oat01-x");
        assert!(!creds.is_expired);
    }

    /// The wire vocabulary is queried in Grafana: every code must be stable, unique
    /// and free of characters that would need escaping.
    #[test]
    fn stage_and_reason_codes_are_unique_snake_case() {
        let stages = [
            CredentialStage::FileRead,
            CredentialStage::FileParse,
            CredentialStage::FileExpired,
            CredentialStage::KeychainCli,
            CredentialStage::KeychainLegacy,
            CredentialStage::KeychainDataProtection,
            CredentialStage::DesktopRead,
            CredentialStage::DesktopKey,
            CredentialStage::DesktopDecrypt,
            CredentialStage::DesktopParse,
            CredentialStage::DesktopExpired,
            CredentialStage::NoSource,
        ];
        let reasons = [
            CredentialReason::NotFound,
            CredentialReason::PermissionDenied,
            CredentialReason::InteractionRequired,
            CredentialReason::Timeout,
            CredentialReason::CliMissing,
            CredentialReason::CliFailed,
            CredentialReason::InvalidUtf8,
            CredentialReason::MalformedJson,
            CredentialReason::MissingOauthField,
            CredentialReason::MissingToken,
            CredentialReason::EmptyToken,
            CredentialReason::Expired,
            CredentialReason::DecryptFailed,
            CredentialReason::UnsupportedFormat,
            CredentialReason::Skipped,
            CredentialReason::Unreadable,
            CredentialReason::Unknown,
        ];

        let mut seen = std::collections::HashSet::new();
        for code in stages.iter().map(|s| s.as_str()) {
            assert!(seen.insert(code), "duplicate stage code '{code}'");
        }
        let mut seen = std::collections::HashSet::new();
        for code in reasons.iter().map(|r| r.as_str()) {
            assert!(seen.insert(code), "duplicate reason code '{code}'");
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "reason code '{code}' is not snake_case ascii"
            );
        }
    }

    /// Privacy guard: a failure must be describable without leaking a path, an
    /// account name or a token — it carries codes only.
    #[test]
    fn failure_display_contains_only_codes_and_labels() {
        let failure = CredentialFailure::new(
            CredentialStage::KeychainLegacy,
            CredentialReason::PermissionDenied,
        );
        let rendered = failure.to_string();
        assert_eq!(rendered, "Keychain access denied");
        assert!(!rendered.contains('/'), "rendered failure must not contain a path");
    }

    #[test]
    fn credential_error_primary_is_the_deepest_attempt() {
        let error = CredentialError {
            attempts: vec![
                CredentialFailure::new(CredentialStage::FileRead, CredentialReason::NotFound),
                CredentialFailure::new(
                    CredentialStage::KeychainCli,
                    CredentialReason::Timeout,
                ),
            ],
        };
        assert_eq!(error.primary().stage, CredentialStage::KeychainCli);
        assert_eq!(error.primary().reason, CredentialReason::Timeout);
        // The trail keeps every attempt so the log shows the whole search.
        assert!(error.to_string().contains("Credentials file not found"));
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn credential_error_primary_never_panics_when_empty() {
        let error = CredentialError { attempts: vec![] };
        assert_eq!(error.primary().stage, CredentialStage::NoSource);
    }

    #[test]
    fn desktop_failures_render_without_paths_or_secrets() {
        let failure = CredentialFailure::new(CredentialStage::DesktopKey, CredentialReason::Skipped);
        assert_eq!(
            failure.to_string(),
            "Claude Desktop key skipped (restart the overlay to be asked again)"
        );
        let failure =
            CredentialFailure::new(CredentialStage::DesktopDecrypt, CredentialReason::DecryptFailed);
        assert_eq!(failure.to_string(), "Claude Desktop login could not be decrypted");
    }

    // ── resolution order ───────────────────────────────────────────────────
    //
    // These drive `resolve` with temp dirs and a fake key provider, so they never read
    // the real `~/.claude`, Claude Desktop folder, Keychain or environment.

    mod resolution {
        use super::*;
        use crate::desktop_credentials::test_support::{encrypt_blob, FakeKeys, TEST_SECRET};
        use std::path::Path;

        const FAR_FUTURE_MS: i64 = 4_000_000_000_000;

        fn unique_dir(label: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "claude-overlay-resolve-{label}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn write_cli_file(dir: &Path, token: &str, expires_at: i64) -> PathBuf {
            let path = dir.join(CREDENTIALS_FILENAME);
            std::fs::write(
                &path,
                format!(r#"{{"claudeAiOauth":{{"accessToken":"{token}","expiresAt":{expires_at}}}}}"#),
            )
            .unwrap();
            path
        }

        fn write_desktop(dir: &Path, token: &str, expires_at: i64) {
            let cache = format!(
                r#"{{"i:u::user:profile":{{"token":"{token}","expiresAt":{expires_at}}}}}"#
            );
            let config = serde_json::json!({
                "oauth:tokenCacheV2": encrypt_blob(Os::current(), &TEST_SECRET, &cache)
            });
            std::fs::write(dir.join("config.json"), config.to_string()).unwrap();
        }

        fn run(
            cli_file: Option<PathBuf>,
            desktop_dir: Option<PathBuf>,
        ) -> std::result::Result<ResolvedCredentials, CredentialError> {
            let keys = FakeKeys::new(&TEST_SECRET);
            resolve(&Sources {
                cli_file,
                keychain: &|_| None,
                desktop_dir,
                desktop_keys: &keys,
            })
        }

        #[test]
        fn missing_cli_file_falls_through_to_desktop() {
            let desktop = unique_dir("fallthrough-desktop");
            write_desktop(&desktop, "desktop-tok", FAR_FUTURE_MS);
            let missing = unique_dir("fallthrough-cli").join("absent.json");

            let creds = run(Some(missing), Some(desktop.clone())).unwrap();
            assert_eq!(creds.access_token, "desktop-tok");
            let _ = std::fs::remove_dir_all(&desktop);
        }

        #[test]
        fn malformed_cli_file_falls_through_to_desktop() {
            let cli_dir = unique_dir("malformed-cli");
            let cli = cli_dir.join(CREDENTIALS_FILENAME);
            std::fs::write(&cli, "{ nope").unwrap();
            let desktop = unique_dir("malformed-desktop");
            write_desktop(&desktop, "desktop-tok", FAR_FUTURE_MS);

            let creds = run(Some(cli), Some(desktop.clone())).unwrap();
            assert_eq!(creds.access_token, "desktop-tok");
            let _ = std::fs::remove_dir_all(&desktop);
            let _ = std::fs::remove_dir_all(&cli_dir);
        }

        #[test]
        fn fresh_cli_token_beats_desktop() {
            let cli_dir = unique_dir("cli-wins-cli");
            let cli = write_cli_file(&cli_dir, "cli-tok", FAR_FUTURE_MS);
            let desktop = unique_dir("cli-wins-desktop");
            write_desktop(&desktop, "desktop-tok", FAR_FUTURE_MS);

            let creds = run(Some(cli), Some(desktop.clone())).unwrap();
            assert_eq!(creds.access_token, "cli-tok");
            let _ = std::fs::remove_dir_all(&desktop);
            let _ = std::fs::remove_dir_all(&cli_dir);
        }

        #[test]
        fn expired_cli_token_loses_to_fresh_desktop_token() {
            let cli_dir = unique_dir("expired-cli");
            let cli = write_cli_file(&cli_dir, "cli-old", 1000);
            let desktop = unique_dir("expired-desktop");
            write_desktop(&desktop, "desktop-fresh", FAR_FUTURE_MS);

            let creds = run(Some(cli), Some(desktop.clone())).unwrap();
            assert_eq!(creds.access_token, "desktop-fresh");
            assert!(!creds.is_expired);
            let _ = std::fs::remove_dir_all(&desktop);
            let _ = std::fs::remove_dir_all(&cli_dir);
        }

        #[test]
        fn when_everything_is_expired_the_latest_expiry_is_returned() {
            let cli_dir = unique_dir("all-expired-cli");
            let cli = write_cli_file(&cli_dir, "cli-older", 1000);
            let desktop = unique_dir("all-expired-desktop");
            write_desktop(&desktop, "desktop-newer", 2000);

            let creds = run(Some(cli), Some(desktop.clone())).unwrap();
            assert!(creds.is_expired);
            assert_eq!(creds.access_token, "desktop-newer");
            let _ = std::fs::remove_dir_all(&desktop);
            let _ = std::fs::remove_dir_all(&cli_dir);
        }

        #[test]
        fn no_desktop_folder_records_no_desktop_attempt() {
            let missing = unique_dir("no-desktop-cli").join("absent.json");
            let error = run(Some(missing), None).unwrap_err();
            assert_eq!(error.attempts.len(), 1);
            assert_eq!(error.attempts[0].stage, CredentialStage::FileRead);
            assert!(error
                .attempts
                .iter()
                .all(|a| !a.stage.as_str().starts_with("desktop_")));

            // With no source at all the trail is the NoSource marker, as before.
            let error = run(None, None).unwrap_err();
            assert_eq!(error.primary().stage, CredentialStage::NoSource);
        }

        #[test]
        fn desktop_failure_is_the_primary_when_it_is_the_deepest() {
            let desktop = unique_dir("desktop-fails");
            // Folder exists but holds no config.json.
            let error = run(None, Some(desktop.clone())).unwrap_err();
            assert_eq!(error.primary().stage, CredentialStage::DesktopRead);
            let _ = std::fs::remove_dir_all(&desktop);
        }
    }
}
