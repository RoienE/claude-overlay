//! Reads the Claude Desktop OAuth login (read-only).
//!
//! Claude Desktop is an Electron app. Its sign-in lives in `config.json` inside its
//! data folder, under `oauth:tokenCacheV2` (the live grant) and the legacy
//! `oauth:tokenCache`. Each value is a base64 blob encrypted with Electron
//! `safeStorage` (Chromium OSCrypt):
//!
//! | OS      | Blob prefix | Cipher                      | Key                                        |
//! |---------|-------------|-----------------------------|--------------------------------------------|
//! | Windows | `v10`       | AES-256-GCM                 | `Local State` -> DPAPI-wrapped key         |
//! | macOS   | `v10`       | AES-128-CBC, PBKDF2 1003    | Keychain `Claude Safe Storage` password    |
//! | Linux   | `v11`       | AES-128-CBC, PBKDF2 1       | Secret Service password (`secret-tool`)    |
//! | Linux   | `v10`       | AES-128-CBC, PBKDF2 1       | Chromium's hardcoded fallback `peanuts`    |
//!
//! The crypto helpers compile on every platform (only key *retrieval* is gated by
//! OS), so the CI jobs for all three platforms run the same decrypt tests.
//!
//! Hard rules, same as the CLI sources:
//! * **Read-only.** The refresh token is never used or sent: refreshing rotates it
//!   and could sign Claude Desktop out. A stale token simply waits for Desktop to
//!   refresh it, and the file is re-read on every poll to pick that up.
//! * **Nothing sensitive is logged.** Codes go to telemetry; the local log gets
//!   source names and field *names* only — never a token, password, key or path
//!   inside the data folder's contents.
//!
//! The decrypted cache's shape is not documented, so the parser is deliberately
//! tolerant (see [`parse_token_cache`]).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aes::Aes128;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use serde_json::Value;
use sha1::Sha1;

use crate::config::CLAUDE_DESKTOP_DIR_ENV;
use crate::credential_source::{
    is_expired, CredentialFailure, CredentialReason, CredentialStage, ResolvedCredentials,
};

/// Token caches inside `config.json`, in preference order. V1 is only a fallback: it
/// may hold a dead grant without the `user:profile` scope.
const TOKEN_CACHE_KEYS: [&str; 2] = ["oauth:tokenCacheV2", "oauth:tokenCache"];

const CONFIG_FILE: &str = "config.json";

/// PBKDF2 salt Chromium uses for OSCrypt on macOS and Linux.
const OSCRYPT_SALT: &[u8] = b"saltysalt";
const MAC_PBKDF2_ROUNDS: u32 = 1003;
const LINUX_PBKDF2_ROUNDS: u32 = 1;
/// Chromium's AES-CBC IV: sixteen spaces.
const OSCRYPT_IV: [u8; 16] = [b' '; 16];

const GCM_NONCE_LEN: usize = 12;
const GCM_TAG_LEN: usize = 16;

/// Chromium's fallback password when no keyring exists on Linux (`v10` blobs).
/// Public knowledge, hardcoded in Chromium itself — not a secret of this app.
#[cfg(not(any(windows, target_os = "macos")))]
const LINUX_FALLBACK_PASSWORD: &[u8] = b"peanuts";

/// Keychain service holding the OSCrypt password for Claude Desktop.
#[cfg(target_os = "macos")]
const MAC_KEYCHAIN_SERVICE: &str = "Claude Safe Storage";

/// Longer than the 5 s CLI read: here the user is expected to answer the macOS
/// permission prompt before `security` returns.
#[cfg(target_os = "macos")]
const MAC_KEYCHAIN_TIMEOUT_SECS: u64 = 60;

/// `secret-tool` attribute values Claude Desktop may store its key under.
#[cfg(not(any(windows, target_os = "macos")))]
const LINUX_SECRET_APPLICATIONS: [&str; 2] = ["Claude", "claude"];

#[cfg(not(any(windows, target_os = "macos")))]
const SECRET_TOOL_TIMEOUT_SECS: u64 = 5;

// ── Platform + data folder ────────────────────────────────────────────────────

/// Which platform's OSCrypt scheme applies. A value rather than `cfg` so the same
/// decrypt logic compiles (and is tested) everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Os {
    Windows,
    Mac,
    Linux,
}

impl Os {
    pub(crate) const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Mac
        } else {
            Self::Linux
        }
    }
}

/// Claude Desktop's data folder, if it exists.
///
/// `CLAUDE_DESKTOP_DIR` overrides the per-platform default. Returning `None` for a
/// missing folder is what keeps CLI-only users free of any Desktop diagnostics.
pub fn desktop_dir() -> Option<PathBuf> {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    default_dir(&env, Os::current()).filter(|dir| dir.is_dir())
}

/// Pure resolution of the folder path (existence is checked by the caller), so every
/// platform's layout can be tested without touching the environment.
fn default_dir(env: &dyn Fn(&str) -> Option<String>, os: Os) -> Option<PathBuf> {
    if let Some(dir) = env(CLAUDE_DESKTOP_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    match os {
        Os::Windows => env("APPDATA").map(|d| PathBuf::from(d).join("Claude")),
        Os::Mac => env("HOME").map(|h| {
            PathBuf::from(h)
                .join("Library")
                .join("Application Support")
                .join("Claude")
        }),
        Os::Linux => env("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|d| d.join("Claude")),
    }
}

// ── Encrypted blob handling ───────────────────────────────────────────────────

/// Which secret protects a blob. Identifies what [`KeyProvider`] must fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyKind {
    /// Windows: the raw 32-byte AES key, unwrapped from `Local State` with DPAPI.
    Dpapi,
    /// macOS: the `Claude Safe Storage` Keychain password.
    MacKeychain,
    /// Linux: the Secret Service password (`v11`).
    LinuxKeyring,
    /// Linux: Chromium's hardcoded fallback password (`v10`).
    LinuxFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cipher {
    /// `nonce(12) || ciphertext || tag(16)` with a raw 256-bit key.
    Gcm,
    /// PKCS7-padded AES-128-CBC with a key derived from a password.
    Cbc { rounds: u32 },
}

/// Supplies the secret behind a blob. A trait so tests inject a fake and never touch
/// a real Keychain, DPAPI or Secret Service.
pub(crate) trait KeyProvider {
    /// The raw key ([`KeyKind::Dpapi`]) or the password to derive one from.
    fn secret(&self, kind: KeyKind) -> Result<Vec<u8>, CredentialFailure>;
    /// Drop anything cached, because a decrypt failed (the key may have rotated).
    fn forget(&self);
}

/// Split a blob into its scheme and payload. Anything but the scheme this OS writes
/// (for example Chrome's app-bound `v20`) is `UnsupportedFormat`.
fn classify(os: Os, blob: &[u8]) -> Result<(KeyKind, Cipher, &[u8]), CredentialReason> {
    if blob.len() < 3 {
        return Err(CredentialReason::UnsupportedFormat);
    }
    let (prefix, payload) = blob.split_at(3);
    let cbc = |rounds| Cipher::Cbc { rounds };
    match (os, prefix) {
        (Os::Windows, b"v10") => Ok((KeyKind::Dpapi, Cipher::Gcm, payload)),
        (Os::Mac, b"v10") => Ok((KeyKind::MacKeychain, cbc(MAC_PBKDF2_ROUNDS), payload)),
        (Os::Linux, b"v11") => Ok((KeyKind::LinuxKeyring, cbc(LINUX_PBKDF2_ROUNDS), payload)),
        (Os::Linux, b"v10") => Ok((KeyKind::LinuxFallback, cbc(LINUX_PBKDF2_ROUNDS), payload)),
        _ => Err(CredentialReason::UnsupportedFormat),
    }
}

fn decrypt_gcm(key: &[u8], payload: &[u8]) -> Result<Vec<u8>, CredentialReason> {
    if payload.len() < GCM_NONCE_LEN + GCM_TAG_LEN {
        return Err(CredentialReason::DecryptFailed);
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CredentialReason::DecryptFailed)?;
    let (nonce, ciphertext) = payload.split_at(GCM_NONCE_LEN);
    cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| CredentialReason::DecryptFailed)
}

/// Derive the AES-128 key Chromium builds from an OSCrypt password.
fn derive_cbc_key(password: &[u8], rounds: u32) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<Sha1>(password, OSCRYPT_SALT, rounds, &mut key);
    key
}

fn decrypt_cbc(key: &[u8], payload: &[u8]) -> Result<Vec<u8>, CredentialReason> {
    let cipher = cbc::Decryptor::<Aes128>::new_from_slices(key, &OSCRYPT_IV)
        .map_err(|_| CredentialReason::DecryptFailed)?;
    cipher
        .decrypt_padded_vec_mut::<Pkcs7>(payload)
        .map_err(|_| CredentialReason::DecryptFailed)
}

/// Decode and decrypt one cache value. A failed decrypt makes the provider forget
/// its cached key so the next poll fetches a fresh one.
fn decrypt_blob(
    os: Os,
    encoded: &str,
    keys: &dyn KeyProvider,
) -> Result<Vec<u8>, CredentialFailure> {
    let stage = CredentialStage::DesktopDecrypt;
    let blob = STANDARD
        .decode(encoded.trim())
        .map_err(|_| CredentialFailure::new(stage, CredentialReason::DecryptFailed))?;
    let (kind, cipher, payload) =
        classify(os, &blob).map_err(|reason| CredentialFailure::new(stage, reason))?;

    // Key failures already carry their own stage (`desktop_key`).
    let secret = keys.secret(kind)?;

    let plaintext = match cipher {
        Cipher::Gcm => decrypt_gcm(&secret, payload),
        Cipher::Cbc { rounds } => decrypt_cbc(&derive_cbc_key(&secret, rounds), payload),
    };
    plaintext.map_err(|reason| {
        keys.forget();
        CredentialFailure::new(stage, reason)
    })
}

// ── Token cache parsing ───────────────────────────────────────────────────────

/// One usable grant found in the decrypted cache. Never `Debug`-printable with its
/// token: the manual impl redacts it.
struct Candidate {
    token: String,
    expires_at_ms: Option<i64>,
    has_profile_scope: bool,
}

impl std::fmt::Debug for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Candidate")
            .field("token", &"[redacted]")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("has_profile_scope", &self.has_profile_scope)
            .finish()
    }
}

/// Epoch seconds below this are far in the past as milliseconds (before 2001), so a
/// value this small is treated as seconds.
const SECONDS_THRESHOLD: i64 = 1_000_000_000_000;

fn normalize_expiry_ms(raw: i64) -> i64 {
    if raw < SECONDS_THRESHOLD {
        raw.saturating_mul(1000)
    } else {
        raw
    }
}

fn expiry_of(entry: &serde_json::Map<String, Value>) -> Option<i64> {
    let value = ["expiresAt", "expires_at"]
        .iter()
        .find_map(|name| entry.get(*name))?;
    let raw = value
        .as_i64()
        .or_else(|| value.as_f64().map(|f| f as i64))?;
    Some(normalize_expiry_ms(raw))
}

/// True when the granted scopes include `user:profile`. Scopes come from the map key
/// (`"<install>:<user>::<scope> <scope>"`) or from a `scope` / `scopes` field, as a
/// space/comma separated string or an array.
fn has_profile_scope(key: Option<&str>, entry: &serde_json::Map<String, Value>) -> bool {
    let is_profile = |scope: &str| scope == "user:profile";
    let split = |text: &str| -> bool {
        text.split(|c: char| c.is_whitespace() || c == ',')
            .any(is_profile)
    };

    if let Some((_, scopes)) = key.and_then(|k| k.split_once("::")) {
        if split(scopes) {
            return true;
        }
    }
    ["scope", "scopes"]
        .iter()
        .filter_map(|name| entry.get(*name))
        .any(|value| match value {
            Value::String(text) => split(text),
            Value::Array(items) => items.iter().filter_map(Value::as_str).any(is_profile),
            _ => false,
        })
}

fn parse_entry(key: Option<&str>, entry: &serde_json::Map<String, Value>) -> Option<Candidate> {
    let token = ["token", "accessToken", "access_token"]
        .iter()
        .find_map(|name| entry.get(*name)?.as_str())
        .filter(|t| !t.is_empty())?;
    Some(Candidate {
        token: token.to_owned(),
        expires_at_ms: expiry_of(entry),
        has_profile_scope: has_profile_scope(key, entry),
    })
}

/// Parse a decrypted token cache into its usable grants.
///
/// Tolerant on purpose, because the shape comes from other projects' observations
/// rather than a spec: the root is normally a map of entries, but a single entry
/// object is accepted too, and each entry may name its token `token`, `accessToken`
/// or `access_token`. Only field *names* are logged, to make a future shape change
/// diagnosable without exposing a value.
fn parse_token_cache(plaintext: &str) -> Result<Vec<Candidate>, CredentialReason> {
    let root: Value =
        serde_json::from_str(plaintext).map_err(|_| CredentialReason::MalformedJson)?;
    let root = root.as_object().ok_or(CredentialReason::MalformedJson)?;

    let candidates: Vec<Candidate> = if let Some(single) = parse_entry(None, root) {
        vec![single]
    } else {
        root.iter()
            .filter_map(|(key, value)| parse_entry(Some(key), value.as_object()?))
            .collect()
    };

    if candidates.is_empty() {
        let mut names: Vec<&str> = root
            .values()
            .filter_map(Value::as_object)
            .flat_map(|entry| entry.keys().map(String::as_str))
            .collect();
        names.sort_unstable();
        names.dedup();
        log::warn!(
            "Claude Desktop token cache has {} entries but none with a usable token; entry fields: {:?}",
            root.len(),
            names
        );
        return Err(CredentialReason::MissingToken);
    }
    Ok(candidates)
}

/// Prefer a grant with `user:profile` (the usage endpoint needs it), then the one
/// that expires latest. An unknown expiry ranks below any known one.
fn pick_best(candidates: Vec<Candidate>) -> Option<Candidate> {
    candidates
        .into_iter()
        .max_by_key(|c| (c.has_profile_scope, c.expires_at_ms.unwrap_or(i64::MIN)))
}

// ── Reading a data folder ─────────────────────────────────────────────────────

/// Look for a Claude Desktop login in `dir`, recording every failed attempt.
///
/// Returns the best grant from `oauth:tokenCacheV2`, falling back to the legacy
/// `oauth:tokenCache` only when V2 is missing, undecryptable or empty. The result may
/// be expired; the caller decides what that means. `config.json` is read afresh on
/// every call because Desktop rewrites it whenever it refreshes the token.
pub(crate) fn read_from_dir(
    dir: &Path,
    os: Os,
    keys: &dyn KeyProvider,
    attempts: &mut Vec<CredentialFailure>,
) -> Option<ResolvedCredentials> {
    let config_path = dir.join(CONFIG_FILE);
    log::debug!(
        "Reading Claude Desktop login from {}",
        config_path.display()
    );

    let raw = match std::fs::read_to_string(&config_path) {
        Ok(raw) => raw,
        Err(e) => {
            log::warn!("Failed to read {}: {e}", config_path.display());
            attempts.push(CredentialFailure::new(
                CredentialStage::DesktopRead,
                CredentialReason::from_io_kind(e.kind()),
            ));
            return None;
        }
    };
    let config: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => {
            log::warn!("Claude Desktop config.json is not valid JSON");
            attempts.push(CredentialFailure::new(
                CredentialStage::DesktopParse,
                CredentialReason::MalformedJson,
            ));
            return None;
        }
    };

    let mut found_any_cache = false;
    for cache_key in TOKEN_CACHE_KEYS {
        let Some(encoded) = config.get(cache_key).and_then(Value::as_str) else {
            continue;
        };
        found_any_cache = true;

        let plaintext = match decrypt_blob(os, encoded, keys) {
            Ok(bytes) => bytes,
            Err(failure) => {
                log::warn!("Claude Desktop {cache_key}: {failure}");
                attempts.push(failure);
                continue;
            }
        };
        let candidates = match std::str::from_utf8(&plaintext) {
            Ok(text) => parse_token_cache(text),
            Err(_) => Err(CredentialReason::InvalidUtf8),
        };
        match candidates.map(pick_best) {
            Ok(Some(best)) => {
                log::debug!("Claude Desktop login read from {cache_key}");
                return Some(ResolvedCredentials {
                    is_expired: is_expired(best.expires_at_ms),
                    access_token: best.token,
                    expires_at_ms: best.expires_at_ms,
                    // Not stored by Desktop; plan detection falls back to the profile.
                    subscription_type: None,
                    rate_limit_tier: None,
                });
            }
            Ok(None) => {
                attempts.push(CredentialFailure::new(
                    CredentialStage::DesktopParse,
                    CredentialReason::MissingToken,
                ));
            }
            Err(reason) => {
                log::warn!("Claude Desktop {cache_key} unusable: {}", reason.as_str());
                attempts.push(CredentialFailure::new(
                    CredentialStage::DesktopParse,
                    reason,
                ));
            }
        }
    }

    if !found_any_cache {
        // Folder exists but nobody is signed in (or Desktop uses a layout we do not know).
        log::warn!("Claude Desktop config.json has no token cache");
        attempts.push(CredentialFailure::new(
            CredentialStage::DesktopRead,
            CredentialReason::NotFound,
        ));
    }
    None
}

// ── Key retrieval (the only OS-gated part) ────────────────────────────────────

/// In-memory key cache so the Keychain / keyring is not hit on every poll. Holds the
/// secret in process memory only; emptied by [`KeyProvider::forget`] when a decrypt
/// fails.
static KEY_CACHE: Mutex<Vec<(KeyKind, Vec<u8>)>> = Mutex::new(Vec::new());

fn cache_lock() -> std::sync::MutexGuard<'static, Vec<(KeyKind, Vec<u8>)>> {
    KEY_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The production [`KeyProvider`]: DPAPI / Keychain / `secret-tool`, cached.
pub(crate) struct SystemKeys<'a> {
    /// Desktop data folder (Windows reads `Local State` from it).
    dir: &'a Path,
    /// Consent gate, called only before the macOS Keychain read.
    gate: &'a dyn Fn() -> bool,
}

impl<'a> SystemKeys<'a> {
    pub(crate) fn new(dir: Option<&'a Path>, gate: &'a dyn Fn() -> bool) -> Self {
        Self {
            dir: dir.unwrap_or_else(|| Path::new("")),
            gate,
        }
    }
}

impl KeyProvider for SystemKeys<'_> {
    fn secret(&self, kind: KeyKind) -> Result<Vec<u8>, CredentialFailure> {
        if let Some((_, secret)) = cache_lock().iter().find(|(k, _)| *k == kind) {
            return Ok(secret.clone());
        }
        let secret = fetch_secret(kind, self.dir, self.gate)?;
        cache_lock().push((kind, secret.clone()));
        Ok(secret)
    }

    fn forget(&self) {
        cache_lock().clear();
    }
}

fn key_failure(reason: CredentialReason) -> CredentialFailure {
    CredentialFailure::new(CredentialStage::DesktopKey, reason)
}

/// Fetch the secret for `kind` from the OS. Arms for other platforms' kinds fall
/// through to `UnsupportedFormat`, which `classify` already rules out in practice.
fn fetch_secret(
    kind: KeyKind,
    dir: &Path,
    gate: &dyn Fn() -> bool,
) -> Result<Vec<u8>, CredentialFailure> {
    // Each platform uses a different subset of these.
    let _ = (dir, gate);
    match kind {
        #[cfg(windows)]
        KeyKind::Dpapi => windows::dpapi_key(dir),
        #[cfg(target_os = "macos")]
        KeyKind::MacKeychain => {
            // Only reached when a Desktop blob exists and the key is not cached, so a
            // CLI-only user never sees the notice.
            if !gate() {
                return Err(key_failure(CredentialReason::Skipped));
            }
            crate::credential_source::security_find_password(
                MAC_KEYCHAIN_SERVICE,
                CredentialStage::DesktopKey,
                MAC_KEYCHAIN_TIMEOUT_SECS,
            )
            .map(String::into_bytes)
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        KeyKind::LinuxKeyring => linux::keyring_password(),
        #[cfg(not(any(windows, target_os = "macos")))]
        KeyKind::LinuxFallback => Ok(LINUX_FALLBACK_PASSWORD.to_vec()),
        _ => Err(key_failure(CredentialReason::UnsupportedFormat)),
    }
}

#[cfg(windows)]
mod windows {
    use super::*;

    /// Prefix Chromium puts on the DPAPI-wrapped key in `Local State`.
    const DPAPI_PREFIX: &[u8] = b"DPAPI";

    /// Pull the DPAPI-wrapped key out of `Local State`'s JSON (pure, for tests).
    pub(super) fn wrapped_key_from_local_state(json: &str) -> Result<Vec<u8>, CredentialReason> {
        let state: Value =
            serde_json::from_str(json).map_err(|_| CredentialReason::MalformedJson)?;
        let encoded = state
            .pointer("/os_crypt/encrypted_key")
            .and_then(Value::as_str)
            .ok_or(CredentialReason::NotFound)?;
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| CredentialReason::MalformedJson)?;
        bytes
            .strip_prefix(DPAPI_PREFIX)
            .map(<[u8]>::to_vec)
            .ok_or(CredentialReason::UnsupportedFormat)
    }

    pub(super) fn dpapi_key(dir: &Path) -> Result<Vec<u8>, CredentialFailure> {
        let path = dir.join("Local State");
        let json = std::fs::read_to_string(&path).map_err(|e| {
            log::warn!("Failed to read Claude Desktop Local State: {e}");
            key_failure(CredentialReason::from_io_kind(e.kind()))
        })?;
        let wrapped = wrapped_key_from_local_state(&json).map_err(|reason| {
            log::warn!(
                "Claude Desktop Local State has no usable key: {}",
                reason.as_str()
            );
            key_failure(reason)
        })?;
        unprotect(&wrapped).ok_or_else(|| {
            log::warn!("DPAPI could not unwrap the Claude Desktop key");
            key_failure(CredentialReason::DecryptFailed)
        })
    }

    /// `CryptUnprotectData` for the current user. `None` on any failure.
    fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
        use std::ptr::null_mut;
        use winapi::ctypes::c_void;
        use winapi::um::dpapi::CryptUnprotectData;
        use winapi::um::winbase::LocalFree;
        use winapi::um::wincrypt::DATA_BLOB;

        let mut input = DATA_BLOB {
            cbData: u32::try_from(data.len()).ok()?,
            pbData: data.as_ptr().cast_mut(),
        };
        let mut output = DATA_BLOB {
            cbData: 0,
            pbData: null_mut(),
        };
        // SAFETY: `input` points at `data`, which outlives the call and is only read.
        // On success `output.pbData` is a LocalAlloc'd buffer of `cbData` bytes that we
        // copy out of and then release with LocalFree; on failure it is untouched.
        unsafe {
            let ok = CryptUnprotectData(
                &mut input,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                0,
                &mut output,
            );
            if ok == 0 || output.pbData.is_null() {
                return None;
            }
            let key = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
            LocalFree(output.pbData.cast::<c_void>());
            Some(key)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn local_state_key_requires_dpapi_prefix() {
            let wrapped = STANDARD.encode([b"DPAPI".as_slice(), &[1, 2, 3]].concat());
            let json = format!(r#"{{"os_crypt":{{"encrypted_key":"{wrapped}"}}}}"#);
            assert_eq!(wrapped_key_from_local_state(&json).unwrap(), vec![1, 2, 3]);

            let other = STANDARD.encode(b"v20abc");
            let json = format!(r#"{{"os_crypt":{{"encrypted_key":"{other}"}}}}"#);
            assert_eq!(
                wrapped_key_from_local_state(&json).unwrap_err(),
                CredentialReason::UnsupportedFormat
            );
        }

        #[test]
        fn local_state_without_key_is_not_found() {
            assert_eq!(
                wrapped_key_from_local_state("{}").unwrap_err(),
                CredentialReason::NotFound
            );
            assert_eq!(
                wrapped_key_from_local_state("nope").unwrap_err(),
                CredentialReason::MalformedJson
            );
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod linux {
    use super::*;
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Look the password up with `secret-tool` (libsecret), trying each attribute
    /// spelling Claude Desktop may use. KWallet-only systems have no Secret Service,
    /// so they end as a recorded failure — a known limitation, not a crash.
    pub(super) fn keyring_password() -> Result<Vec<u8>, CredentialFailure> {
        let mut last = key_failure(CredentialReason::NotFound);
        for application in LINUX_SECRET_APPLICATIONS {
            match lookup(application) {
                Ok(secret) => return Ok(secret),
                Err(failure) => last = failure,
            }
        }
        Err(last)
    }

    fn lookup(application: &str) -> Result<Vec<u8>, CredentialFailure> {
        let mut child = match Command::new("secret-tool")
            .args(["lookup", "application", application])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::warn!("secret-tool is not installed");
                return Err(key_failure(CredentialReason::CliMissing));
            }
            Err(e) => {
                log::warn!("Could not run secret-tool: {e}");
                return Err(key_failure(CredentialReason::CliFailed));
            }
        };

        // A locked keyring can block on an unlock prompt; bound the wait.
        let deadline = Instant::now() + Duration::from_secs(SECRET_TOOL_TIMEOUT_SECS);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    log::warn!("secret-tool timed out after {SECRET_TOOL_TIMEOUT_SECS}s");
                    return Err(key_failure(CredentialReason::Timeout));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => {
                    log::warn!("Waiting on secret-tool failed: {e}");
                    return Err(key_failure(CredentialReason::CliFailed));
                }
            }
        };

        // secret-tool exits 1 with no output when nothing matches.
        let mut secret = Vec::new();
        if let Some(mut pipe) = child.stdout.take() {
            let _ = pipe.read_to_end(&mut secret);
        }
        while secret.last().is_some_and(|b| b.is_ascii_whitespace()) {
            secret.pop();
        }
        if !status.success() || secret.is_empty() {
            let mut failure = key_failure(CredentialReason::NotFound);
            failure.exit_code = status.code();
            return Err(failure);
        }
        Ok(secret)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Encrypting counterparts of the decrypt helpers, shared with the resolution tests in
/// `credential_source`. Synthetic data only.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use cbc::cipher::BlockEncryptMut;

    /// Deterministic nonce: fine for tests, never used for real data.
    const TEST_NONCE: [u8; GCM_NONCE_LEN] = [7; GCM_NONCE_LEN];

    /// A 32-byte secret that works both as a raw AES-256 key and as a password.
    pub(crate) const TEST_SECRET: [u8; 32] = [42; 32];

    /// A [`KeyProvider`] that returns a fixed secret and counts `forget` calls.
    pub(crate) struct FakeKeys {
        pub secret: Vec<u8>,
        pub forgotten: std::cell::Cell<u32>,
    }

    impl FakeKeys {
        pub(crate) fn new(secret: &[u8]) -> Self {
            Self {
                secret: secret.to_vec(),
                forgotten: std::cell::Cell::new(0),
            }
        }
    }

    impl KeyProvider for FakeKeys {
        fn secret(&self, _kind: KeyKind) -> Result<Vec<u8>, CredentialFailure> {
            Ok(self.secret.clone())
        }
        fn forget(&self) {
            self.forgotten.set(self.forgotten.get() + 1);
        }
    }

    /// Encrypt `plaintext` the way Claude Desktop does on `os`, returning the base64
    /// blob stored in `config.json`.
    pub(crate) fn encrypt_blob(os: Os, secret: &[u8], plaintext: &str) -> String {
        let mut blob = Vec::new();
        match os {
            Os::Windows => {
                blob.extend_from_slice(b"v10");
                let cipher = Aes256Gcm::new_from_slice(secret).unwrap();
                blob.extend_from_slice(&TEST_NONCE);
                blob.extend(
                    cipher
                        .encrypt(Nonce::from_slice(&TEST_NONCE), plaintext.as_bytes())
                        .unwrap(),
                );
            }
            Os::Mac | Os::Linux => {
                let (prefix, rounds) = match os {
                    Os::Mac => (&b"v10"[..], MAC_PBKDF2_ROUNDS),
                    _ => (&b"v11"[..], LINUX_PBKDF2_ROUNDS),
                };
                blob.extend_from_slice(prefix);
                blob.extend(encrypt_cbc(
                    &derive_cbc_key(secret, rounds),
                    plaintext.as_bytes(),
                ));
            }
        }
        STANDARD.encode(blob)
    }

    pub(crate) fn encrypt_cbc(key: &[u8], plaintext: &[u8]) -> Vec<u8> {
        cbc::Encryptor::<Aes128>::new_from_slices(key, &OSCRYPT_IV)
            .unwrap()
            .encrypt_padded_vec_mut::<Pkcs7>(plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    const ALL_OS: [Os; 3] = [Os::Windows, Os::Mac, Os::Linux];

    fn unique_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "claude-overlay-desktop-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_config(dir: &Path, config: &Value) {
        std::fs::write(dir.join(CONFIG_FILE), config.to_string()).unwrap();
    }

    // ── crypto ───────────────────────────────────────────────────────────────

    #[test]
    fn round_trips_every_platform_scheme() {
        for os in ALL_OS {
            let keys = FakeKeys::new(&TEST_SECRET);
            let blob = encrypt_blob(os, &TEST_SECRET, "hello desktop");
            let plain = decrypt_blob(os, &blob, &keys).unwrap();
            assert_eq!(plain, b"hello desktop", "round trip failed for {os:?}");
            assert_eq!(keys.forgotten.get(), 0);
        }
    }

    #[test]
    fn cbc_round_trips_with_one_and_1003_rounds() {
        for rounds in [1, 1003] {
            let key = derive_cbc_key(b"a password", rounds);
            let sealed = encrypt_cbc(&key, b"payload");
            assert_eq!(decrypt_cbc(&key, &sealed).unwrap(), b"payload");
        }
        assert_ne!(
            derive_cbc_key(b"a password", 1),
            derive_cbc_key(b"a password", 1003),
            "round count must change the derived key"
        );
    }

    /// Known PBKDF2-HMAC-SHA1 vector (RFC 6070, 2 iterations): guards the primitive
    /// choice independently of the round-trip tests.
    #[test]
    fn pbkdf2_matches_rfc6070_vector() {
        let mut out = [0u8; 20];
        pbkdf2::pbkdf2_hmac::<Sha1>(b"password", b"salt", 2, &mut out);
        assert_eq!(
            out,
            [
                0xea, 0x6c, 0x01, 0x4d, 0xc7, 0x2d, 0x6f, 0x8c, 0xcd, 0x1e, 0xd9, 0x2a, 0xce, 0x1d,
                0x41, 0xf0, 0xd8, 0xde, 0x89, 0x57
            ]
        );
    }

    #[test]
    fn wrong_key_is_decrypt_failed_and_forgets_the_key() {
        for os in ALL_OS {
            let blob = encrypt_blob(os, &TEST_SECRET, "secret stuff");
            let keys = FakeKeys::new(&[9u8; 32]);
            let failure = decrypt_blob(os, &blob, &keys).unwrap_err();
            assert_eq!(failure.stage, CredentialStage::DesktopDecrypt, "{os:?}");
            assert_eq!(failure.reason, CredentialReason::DecryptFailed, "{os:?}");
            assert_eq!(keys.forgotten.get(), 1, "key must be dropped for {os:?}");
        }
    }

    #[test]
    fn unknown_prefix_is_unsupported_format() {
        let keys = FakeKeys::new(&TEST_SECRET);
        let blob = STANDARD.encode(b"v20 whatever follows");
        for os in ALL_OS {
            let failure = decrypt_blob(os, &blob, &keys).unwrap_err();
            assert_eq!(
                failure.reason,
                CredentialReason::UnsupportedFormat,
                "{os:?}"
            );
        }
        let short = STANDARD.encode(b"v1");
        assert_eq!(
            decrypt_blob(Os::Windows, &short, &keys).unwrap_err().reason,
            CredentialReason::UnsupportedFormat
        );
    }

    #[test]
    fn other_platforms_scheme_is_unsupported() {
        // A Windows-style v10 GCM blob means nothing to the Linux/macOS path and vice
        // versa; v11 only exists on Linux.
        assert!(classify(Os::Windows, b"v11xxxx").is_err());
        assert!(classify(Os::Mac, b"v11xxxx").is_err());
        assert!(classify(Os::Linux, b"v10xxxx").is_ok());
        assert!(classify(Os::Linux, b"v11xxxx").is_ok());
    }

    #[test]
    fn truncated_or_garbage_ciphertext_is_decrypt_failed() {
        let keys = FakeKeys::new(&TEST_SECRET);
        let gcm_short = STANDARD.encode([b"v10".as_slice(), &[1u8; 10]].concat());
        assert_eq!(
            decrypt_blob(Os::Windows, &gcm_short, &keys)
                .unwrap_err()
                .reason,
            CredentialReason::DecryptFailed
        );
        let cbc_unaligned = STANDARD.encode([b"v10".as_slice(), &[1u8; 17]].concat());
        assert_eq!(
            decrypt_blob(Os::Mac, &cbc_unaligned, &keys)
                .unwrap_err()
                .reason,
            CredentialReason::DecryptFailed
        );
        assert_eq!(
            decrypt_blob(Os::Mac, "!!not base64!!", &keys)
                .unwrap_err()
                .reason,
            CredentialReason::DecryptFailed
        );
    }

    // ── parsing ──────────────────────────────────────────────────────────────

    #[test]
    fn parses_map_keyed_entries_and_reads_scopes_from_the_key() {
        let json = r#"{
            "inst:user::user:inference user:profile": { "token": "tok-a", "expiresAt": 9999999999999, "refreshToken": "never-used" },
            "inst:user::user:inference": { "token": "tok-b", "expiresAt": 9999999999999 }
        }"#;
        let candidates = parse_token_cache(json).unwrap();
        assert_eq!(candidates.len(), 2);
        let best = pick_best(candidates).unwrap();
        assert_eq!(best.token, "tok-a", "the user:profile entry must win");
        assert!(best.has_profile_scope);
    }

    #[test]
    fn latest_expiry_wins_among_equal_scopes() {
        let json = r#"{
            "a::user:profile": { "token": "older", "expiresAt": 1700000000000 },
            "b::user:profile": { "token": "newer", "expiresAt": 1800000000000 },
            "c::user:profile": { "token": "unknown-expiry" }
        }"#;
        let best = pick_best(parse_token_cache(json).unwrap()).unwrap();
        assert_eq!(best.token, "newer");
    }

    #[test]
    fn profile_scope_beats_a_later_expiry_without_it() {
        let json = r#"{
            "a::user:inference": { "token": "later", "expiresAt": 1900000000000 },
            "b::user:profile": { "token": "profiled", "expiresAt": 1700000000000 }
        }"#;
        let best = pick_best(parse_token_cache(json).unwrap()).unwrap();
        assert_eq!(best.token, "profiled");
    }

    #[test]
    fn accepts_alternate_field_names_and_scope_fields() {
        for (field, scope_field) in [
            ("token", r#""scope": "user:inference user:profile""#),
            ("accessToken", r#""scopes": ["user:profile"]"#),
            ("access_token", r#""scope": "user:profile,user:inference""#),
        ] {
            let json = format!(r#"{{ "k": {{ "{field}": "tok", {scope_field} }} }}"#);
            let best = pick_best(parse_token_cache(&json).unwrap()).unwrap();
            assert_eq!(best.token, "tok", "field {field}");
            assert!(best.has_profile_scope, "scope field for {field}");
        }
    }

    #[test]
    fn accepts_a_single_entry_at_the_root() {
        let best = pick_best(parse_token_cache(r#"{ "accessToken": "solo" }"#).unwrap()).unwrap();
        assert_eq!(best.token, "solo");
    }

    #[test]
    fn expires_at_in_seconds_is_converted_to_milliseconds() {
        let json = r#"{ "k": { "token": "t", "expiresAt": 1800000000 } }"#;
        let best = pick_best(parse_token_cache(json).unwrap()).unwrap();
        assert_eq!(best.expires_at_ms, Some(1_800_000_000_000));

        let json = r#"{ "k": { "token": "t", "expiresAt": 1800000000000 } }"#;
        let best = pick_best(parse_token_cache(json).unwrap()).unwrap();
        assert_eq!(best.expires_at_ms, Some(1_800_000_000_000));
    }

    #[test]
    fn empty_or_tokenless_cache_is_missing_token() {
        for json in [
            r#"{}"#,
            r#"{ "k": { "refreshToken": "x" } }"#,
            r#"{ "k": { "token": "" } }"#,
        ] {
            assert_eq!(
                parse_token_cache(json).err(),
                Some(CredentialReason::MissingToken),
                "{json}"
            );
        }
        assert_eq!(
            parse_token_cache("not json").err(),
            Some(CredentialReason::MalformedJson)
        );
        assert_eq!(
            parse_token_cache("[1,2]").err(),
            Some(CredentialReason::MalformedJson)
        );
    }

    #[test]
    fn candidate_debug_output_redacts_the_token() {
        let c = Candidate {
            token: "super-secret-value".into(),
            expires_at_ms: None,
            has_profile_scope: false,
        };
        assert!(!format!("{c:?}").contains("super-secret-value"));
    }

    // ── data folder ──────────────────────────────────────────────────────────

    #[test]
    fn default_dir_per_platform_and_override() {
        let env = |name: &str| match name {
            "APPDATA" => Some("/appdata".to_string()),
            "HOME" => Some("/home/u".to_string()),
            _ => None,
        };
        assert_eq!(
            default_dir(&env, Os::Windows),
            Some(PathBuf::from("/appdata").join("Claude"))
        );
        assert_eq!(
            default_dir(&env, Os::Mac),
            Some(
                PathBuf::from("/home/u")
                    .join("Library")
                    .join("Application Support")
                    .join("Claude")
            )
        );
        assert_eq!(
            default_dir(&env, Os::Linux),
            Some(PathBuf::from("/home/u").join(".config").join("Claude"))
        );

        let xdg = |name: &str| match name {
            "XDG_CONFIG_HOME" => Some("/xdg".to_string()),
            "HOME" => Some("/home/u".to_string()),
            _ => None,
        };
        assert_eq!(
            default_dir(&xdg, Os::Linux),
            Some(PathBuf::from("/xdg").join("Claude"))
        );

        let overridden =
            |name: &str| (name == CLAUDE_DESKTOP_DIR_ENV).then(|| "/custom".to_string());
        for os in ALL_OS {
            assert_eq!(default_dir(&overridden, os), Some(PathBuf::from("/custom")));
        }
        assert_eq!(default_dir(&|_: &str| None, Os::Windows), None);
    }

    // ── reading a folder ─────────────────────────────────────────────────────

    const FAR_FUTURE_MS: i64 = 4_000_000_000_000;

    fn cache_json(token: &str, expires_at: i64) -> String {
        format!(
            r#"{{ "inst:user::user:inference user:profile": {{ "token": "{token}", "expiresAt": {expires_at}, "refreshToken": "rt" }} }}"#
        )
    }

    #[test]
    fn reads_a_v2_login_from_a_folder() {
        let dir = unique_dir("v2");
        let os = Os::current();
        let blob = encrypt_blob(os, &TEST_SECRET, &cache_json("tok-v2", FAR_FUTURE_MS));
        write_config(
            &dir,
            &serde_json::json!({ "oauth:tokenCacheV2": blob, "other": 1 }),
        );

        let keys = FakeKeys::new(&TEST_SECRET);
        let mut attempts = Vec::new();
        let creds = read_from_dir(&dir, os, &keys, &mut attempts).unwrap();
        assert_eq!(creds.access_token, "tok-v2");
        assert!(!creds.is_expired);
        assert!(creds.subscription_type.is_none() && creds.rate_limit_tier.is_none());
        assert!(attempts.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_is_preferred_over_v1() {
        let dir = unique_dir("v2-over-v1");
        let os = Os::current();
        write_config(
            &dir,
            &serde_json::json!({
                "oauth:tokenCacheV2": encrypt_blob(os, &TEST_SECRET, &cache_json("from-v2", FAR_FUTURE_MS)),
                "oauth:tokenCache": encrypt_blob(os, &TEST_SECRET, &cache_json("from-v1", FAR_FUTURE_MS)),
            }),
        );
        let mut attempts = Vec::new();
        let creds = read_from_dir(&dir, os, &FakeKeys::new(&TEST_SECRET), &mut attempts).unwrap();
        assert_eq!(creds.access_token, "from-v2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v1_is_the_fallback_when_v2_is_missing_or_unusable() {
        let os = Os::current();
        let v1 = encrypt_blob(os, &TEST_SECRET, &cache_json("from-v1", FAR_FUTURE_MS));

        // V2 absent.
        let dir = unique_dir("v1-only");
        write_config(&dir, &serde_json::json!({ "oauth:tokenCache": v1.clone() }));
        let mut attempts = Vec::new();
        let creds = read_from_dir(&dir, os, &FakeKeys::new(&TEST_SECRET), &mut attempts).unwrap();
        assert_eq!(creds.access_token, "from-v1");
        let _ = std::fs::remove_dir_all(&dir);

        // V2 present but empty of usable entries: the failure is recorded, V1 still used.
        let dir = unique_dir("v1-after-empty-v2");
        let empty_v2 = encrypt_blob(os, &TEST_SECRET, "{}");
        write_config(
            &dir,
            &serde_json::json!({ "oauth:tokenCacheV2": empty_v2, "oauth:tokenCache": v1 }),
        );
        let mut attempts = Vec::new();
        let creds = read_from_dir(&dir, os, &FakeKeys::new(&TEST_SECRET), &mut attempts).unwrap();
        assert_eq!(creds.access_token, "from-v1");
        assert_eq!(
            attempts,
            vec![CredentialFailure::new(
                CredentialStage::DesktopParse,
                CredentialReason::MissingToken
            )]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_cache_is_a_parse_failure() {
        let dir = unique_dir("empty");
        let os = Os::current();
        write_config(
            &dir,
            &serde_json::json!({ "oauth:tokenCacheV2": encrypt_blob(os, &TEST_SECRET, "{}") }),
        );
        let mut attempts = Vec::new();
        assert!(read_from_dir(&dir, os, &FakeKeys::new(&TEST_SECRET), &mut attempts).is_none());
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].stage, CredentialStage::DesktopParse);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_expired_login_is_returned_flagged_expired() {
        let dir = unique_dir("expired");
        let os = Os::current();
        write_config(
            &dir,
            &serde_json::json!({ "oauth:tokenCacheV2": encrypt_blob(os, &TEST_SECRET, &cache_json("old", 1000)) }),
        );
        let mut attempts = Vec::new();
        let creds = read_from_dir(&dir, os, &FakeKeys::new(&TEST_SECRET), &mut attempts).unwrap();
        assert!(creds.is_expired);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_cacheless_config_records_a_read_failure() {
        let dir = unique_dir("no-config");
        let os = Os::current();
        let keys = FakeKeys::new(&TEST_SECRET);

        let mut attempts = Vec::new();
        assert!(read_from_dir(&dir, os, &keys, &mut attempts).is_none());
        assert_eq!(
            attempts,
            vec![CredentialFailure::new(
                CredentialStage::DesktopRead,
                CredentialReason::NotFound
            )]
        );

        write_config(&dir, &serde_json::json!({ "something": "else" }));
        let mut attempts = Vec::new();
        assert!(read_from_dir(&dir, os, &keys, &mut attempts).is_none());
        assert_eq!(attempts[0].stage, CredentialStage::DesktopRead);

        std::fs::write(dir.join(CONFIG_FILE), "{ not json").unwrap();
        let mut attempts = Vec::new();
        assert!(read_from_dir(&dir, os, &keys, &mut attempts).is_none());
        assert_eq!(attempts[0].reason, CredentialReason::MalformedJson);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undecryptable_cache_records_decrypt_failed() {
        let dir = unique_dir("wrong-key");
        let os = Os::current();
        write_config(
            &dir,
            &serde_json::json!({ "oauth:tokenCacheV2": encrypt_blob(os, &TEST_SECRET, &cache_json("t", FAR_FUTURE_MS)) }),
        );
        let keys = FakeKeys::new(&[1u8; 32]);
        let mut attempts = Vec::new();
        assert!(read_from_dir(&dir, os, &keys, &mut attempts).is_none());
        assert_eq!(attempts[0].reason, CredentialReason::DecryptFailed);
        assert_eq!(keys.forgotten.get(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
