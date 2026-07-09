//! Adaptive polling loop.
//!
//! Orchestrates: credential reading → usage API call → plan detection →
//! snapshot emission. Implements the full adaptive-interval scheme from §5.3
//! of the plan, plus OS idle/lock detection on Windows.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;

use chrono::Utc;
use log::{debug, error, info, warn};

use crate::config::{
    AUTH_RETRY_SECS, FAST_EXTRA_COUNT, IDLE_PAUSE, MAX_BACKOFF, POLL_ERROR, POLL_FAST,
    POLL_FAST_EXTRA, POLL_INTERVAL, PROFILE_POLL_SECS,
};
use crate::credential_source::read_credentials;
use crate::fallback_logs;
use crate::model::{ApiResult, Plan, Profile, SourceStatus, UsageSnapshot};
use crate::plan_detector::{detect_from_profile, resolve_plan};
use crate::telemetry::Telemetry;
use crate::usage_client::{build_client, fetch_profile, fetch_usage};

/// Message type sent from the poller to the Tauri event emitter.
pub type SnapshotSender = mpsc::UnboundedSender<UsageSnapshot>;

/// A shared wake handle: callers call `notify_one()` to interrupt the poller's inter-poll sleep.
/// Kept outside the `Mutex` so we never hold a `MutexGuard` across the `.await` on `notified()`.
pub type RefreshNotify = Arc<tokio::sync::Notify>;

/// Shared state the poller and window_ctl commands read/write.
#[derive(Debug, Default)]
pub struct PollerState {
    pub last_snapshot: Option<UsageSnapshot>,
    pub plan_override: Option<Plan>,
    pub refresh_requested: bool,
    pub rate_limited_until: Option<std::time::Instant>,
}

pub type SharedPollerState = Arc<Mutex<PollerState>>;

/// OS-level idle/lock detection (Windows-specific; always returns false on other platforms).
fn is_system_idle_or_locked() -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::mem::size_of;
        use winapi::um::winuser::{GetLastInputInfo, LASTINPUTINFO};

        // Check idle time via GetLastInputInfo
        let mut info = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        unsafe {
            if GetLastInputInfo(&mut info) == 0 {
                return false;
            }
        }
        let idle_ms = {
            use winapi::um::sysinfoapi::GetTickCount;
            let tick = unsafe { GetTickCount() };
            tick.wrapping_sub(info.dwTime)
        };
        let idle_secs = idle_ms / 1000;

        if IDLE_PAUSE > 0 && idle_secs >= IDLE_PAUSE as u32 {
            return true;
        }

        // Check session lock state
        // We use WTSQuerySessionInformation — simplify: check if a "LogonUI" process is running
        // as a quick proxy for "locked". For robustness, rely on idle time only.
        false
    }

    #[cfg(not(target_os = "windows"))]
    false
}

/// Race a timed sleep against a manual-refresh wake signal.
/// Returns as soon as the timer expires *or* `notify.notify_one()` is called, whichever comes
/// first. Because `Notify` stores one permit when no waiter is parked, a notify that fires just
/// before we park here is not lost — `notified()` returns immediately in that case.
async fn wait_or_wake(secs: u64, notify: &tokio::sync::Notify) {
    tokio::select! {
        _ = sleep(Duration::from_secs(secs)) => {}
        _ = notify.notified() => {}
    }
}

/// The main polling loop. Run in a detached Tokio task.
pub async fn run(
    sender: SnapshotSender,
    state: SharedPollerState,
    notify: RefreshNotify,
    telemetry: Telemetry,
) {
    let client = match build_client() {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to build HTTP client: {}", e);
            return;
        }
    };

    // Emit initial loading snapshot.
    let _ = sender.send(UsageSnapshot::loading());

    // State for adaptive polling.
    let mut current_interval = POLL_INTERVAL;
    let mut backoff_secs: u64 = POLL_INTERVAL;
    let mut consecutive_errors: u32 = 0;
    let mut fast_extra_remaining: u32 = 0;
    let mut last_utilization: Vec<(String, f32)> = Vec::new();
    let mut cached_profile: Option<Profile> = None;
    let mut last_profile_fetch = std::time::Instant::now()
        .checked_sub(Duration::from_secs(PROFILE_POLL_SECS + 1))
        .unwrap_or(std::time::Instant::now());

    loop {
        // ── Check for manual refresh request ─────────────────────────────────
        let refresh_requested = {
            let mut s = state.lock().unwrap();
            if s.refresh_requested {
                s.refresh_requested = false;
                true
            } else {
                false
            }
        };

        // ── Pause when idle / locked ──────────────────────────────────────────
        if !refresh_requested && IDLE_PAUSE > 0 && is_system_idle_or_locked() {
            debug!("System idle/locked — pausing poll for {}s", current_interval);
            wait_or_wake(current_interval, &notify).await;
            continue;
        }

        // ── Check rate-limit cooldown ─────────────────────────────────────────
        // Compute the boolean entirely inside the lock scope so the (non-Send)
        // MutexGuard is dropped before any `.await`.
        let in_backoff = {
            let s = state.lock().unwrap();
            match s.rate_limited_until {
                Some(until) => std::time::Instant::now() < until && !refresh_requested,
                None => false,
            }
        };
        if in_backoff {
            // Still in backoff; wait briefly then recheck. Wakeable so a manual refresh
            // (already rejected at the rate-limited branch in request_refresh) can at least
            // skip this pause if the backoff window expires between checks.
            wait_or_wake(10, &notify).await;
            continue;
        }

        // ── Read credentials ──────────────────────────────────────────────────
        let credentials = match read_credentials() {
            Ok(c) => c,
            Err(e) => {
                warn!("Cannot read credentials: {}", e);
                let snap = UsageSnapshot {
                    status: SourceStatus::AuthExpired,
                    ..UsageSnapshot::auth_expired()
                };
                let _ = sender.send(snap);
                sleep(Duration::from_secs(AUTH_RETRY_SECS)).await;
                continue;
            }
        };

        if credentials.is_expired {
            warn!("OAuth token expired");
            let _ = sender.send(UsageSnapshot::auth_expired());
            sleep(Duration::from_secs(AUTH_RETRY_SECS)).await;
            continue;
        }

        let token = &credentials.access_token;

        // ── Optionally fetch profile (slow cadence) ───────────────────────────
        let profile_age = last_profile_fetch.elapsed().as_secs();
        if cached_profile.is_none() || profile_age >= PROFILE_POLL_SECS {
            match fetch_profile(&client, token).await {
                ApiResult::Ok(p) => {
                    info!("Profile fetched: plan={}", detect_from_profile(&p));
                    cached_profile = Some(p);
                    last_profile_fetch = std::time::Instant::now();
                }
                ApiResult::Unauthorized => {
                    warn!("Profile fetch: 401 Unauthorized");
                    // Don't stop usage polling; just skip profile
                }
                ApiResult::RateLimited => {
                    warn!("Profile fetch: 429 RateLimited");
                    telemetry.record_rate_limit_hit("profile", 0);
                }
                ApiResult::NetworkError(e) => {
                    warn!("Profile fetch network error: {}", e);
                }
                ApiResult::ParseError(e) => {
                    warn!("Profile fetch parse error: {}", e);
                }
            }
        }

        // ── Fetch usage ───────────────────────────────────────────────────────
        let plan_override = state.lock().unwrap().plan_override.clone();

        match fetch_usage(&client, token).await {
            ApiResult::Ok((windows, extra_usage)) => {
                consecutive_errors = 0;
                backoff_secs = POLL_INTERVAL;

                // Detect utilization change for adaptive interval.
                let current_util: Vec<(String, f32)> = windows
                    .iter()
                    .map(|w| (w.key.clone(), w.utilization))
                    .collect();

                let utilization_rising = current_util.iter().any(|(key, util)| {
                    last_utilization
                        .iter()
                        .find(|(k, _)| k == key)
                        .map_or(false, |(_, prev)| util > prev)
                });

                last_utilization = current_util;

                if utilization_rising {
                    current_interval = POLL_FAST;
                    fast_extra_remaining = FAST_EXTRA_COUNT;
                    debug!("Utilization rising → fast poll ({}s)", POLL_FAST);
                } else if fast_extra_remaining > 0 {
                    fast_extra_remaining -= 1;
                    current_interval = POLL_FAST_EXTRA;
                    debug!(
                        "Fast-extra burst, {} remaining ({}s interval)",
                        fast_extra_remaining, POLL_FAST_EXTRA
                    );
                } else {
                    current_interval = POLL_INTERVAL;
                }

                // Clear rate-limit state.
                {
                    let mut s = state.lock().unwrap();
                    s.rate_limited_until = None;
                }

                let plan = resolve_plan(
                    cached_profile.as_ref(),
                    credentials.subscription_type.as_deref(),
                    credentials.rate_limit_tier.as_deref(),
                    &windows,
                    plan_override.as_ref(),
                );

                let snap = UsageSnapshot {
                    plan,
                    profile: cached_profile.clone(),
                    windows,
                    extra_usage,
                    status: SourceStatus::Live,
                    fetched_at: Utc::now(),
                    next_poll_in: current_interval,
                };

                {
                    let mut s = state.lock().unwrap();
                    s.last_snapshot = Some(snap.clone());
                }
                let _ = sender.send(snap);
            }

            ApiResult::RateLimited => {
                warn!("Usage fetch: 429 RateLimited — backing off");
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF);
                current_interval = backoff_secs;
                telemetry.record_rate_limit_hit("usage", backoff_secs);

                let until = std::time::Instant::now() + Duration::from_secs(backoff_secs);
                {
                    let mut s = state.lock().unwrap();
                    s.rate_limited_until = Some(until);
                }

                if let Some(last) = state.lock().unwrap().last_snapshot.clone() {
                    let stale = UsageSnapshot {
                        status: SourceStatus::Stale(format!(
                            "Rate limited — next retry in {}s",
                            backoff_secs
                        )),
                        next_poll_in: backoff_secs,
                        ..last
                    };
                    let _ = sender.send(stale);
                }
            }

            ApiResult::Unauthorized => {
                warn!("Usage fetch: 401 Unauthorized — auth expired");
                let _ = sender.send(UsageSnapshot::auth_expired());
                sleep(Duration::from_secs(AUTH_RETRY_SECS)).await;
                continue;
            }

            ApiResult::NetworkError(e) | ApiResult::ParseError(e) => {
                consecutive_errors += 1;
                warn!(
                    "Usage fetch error (attempt {}): {}",
                    consecutive_errors, e
                );

                current_interval = POLL_ERROR;

                if consecutive_errors >= 3 {
                    // Fall back to local JSONL aggregation.
                    info!("Falling back to local JSONL logs after {} errors", consecutive_errors);
                    let fallback = fallback_logs::aggregate();
                    let fb_windows = fallback_logs::to_quota_windows(&fallback);

                    let plan = resolve_plan(
                        cached_profile.as_ref(),
                        credentials.subscription_type.as_deref(),
                        credentials.rate_limit_tier.as_deref(),
                        &fb_windows,
                        plan_override.as_ref(),
                    );

                    let snap = UsageSnapshot {
                        plan,
                        profile: cached_profile.clone(),
                        windows: fb_windows,
                        extra_usage: None,
                        status: SourceStatus::Degraded,
                        fetched_at: Utc::now(),
                        next_poll_in: POLL_ERROR,
                    };
                    {
                        let mut s = state.lock().unwrap();
                        s.last_snapshot = Some(snap.clone());
                    }
                    let _ = sender.send(snap);
                } else if let Some(last) = state.lock().unwrap().last_snapshot.clone() {
                    let stale = UsageSnapshot {
                        status: SourceStatus::Stale(format!("Network error: {}", e)),
                        next_poll_in: POLL_ERROR,
                        ..last
                    };
                    let _ = sender.send(stale);
                }
            }
        }

        // ── Check for reset-alignment: if any window resets soon, add extra poll ──
        let sleep_secs = if let Some(last) = state.lock().unwrap().last_snapshot.clone() {
            let now = Utc::now();
            let imminent_reset = last.windows.iter().any(|w| {
                w.resets_at.map_or(false, |rt| {
                    let secs_until = (rt - now).num_seconds();
                    secs_until > 0 && secs_until < current_interval as i64
                })
            });
            if imminent_reset {
                // Poll right after the earliest reset.
                let earliest = last
                    .windows
                    .iter()
                    .filter_map(|w| w.resets_at)
                    .filter(|rt| *rt > now)
                    .min();
                if let Some(reset_time) = earliest {
                    let wait = ((reset_time - now).num_seconds() + 5).max(1) as u64;
                    debug!("Reset-aligned poll in {}s", wait);
                    wait
                } else {
                    current_interval
                }
            } else {
                current_interval
            }
        } else {
            current_interval
        };

        wait_or_wake(sleep_secs, &notify).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    /// `wait_or_wake` should return well before the timeout when `notify_one()` fires.
    #[tokio::test]
    async fn wait_or_wake_interrupted_by_notify() {
        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_clone = notify.clone();

        // Fire the notify after a brief delay in a sibling task.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            notify_clone.notify_one();
        });

        let start = Instant::now();
        wait_or_wake(30, &notify).await; // 30-second timeout
        let elapsed = start.elapsed();

        // Should have returned in well under 1 second (the notify fires at ~50ms).
        assert!(
            elapsed < Duration::from_secs(1),
            "wait_or_wake took {:?}, expected <1s when interrupted",
            elapsed
        );
    }

    /// `wait_or_wake` with no notify should run for approximately the requested duration.
    #[tokio::test]
    async fn wait_or_wake_expires_naturally() {
        let notify = tokio::sync::Notify::new();

        let start = Instant::now();
        wait_or_wake(1, &notify).await; // 1-second timeout
        let elapsed = start.elapsed();

        // Should take at least ~1 second and not much more.
        assert!(
            elapsed >= Duration::from_millis(900),
            "wait_or_wake returned too early: {:?}",
            elapsed
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "wait_or_wake took too long: {:?}",
            elapsed
        );
    }

    /// A `notify_one()` called *before* parking should not be lost.
    #[tokio::test]
    async fn wait_or_wake_pre_notified() {
        let notify = tokio::sync::Notify::new();
        // Store a permit before anyone is waiting.
        notify.notify_one();

        let start = Instant::now();
        wait_or_wake(30, &notify).await; // 30-second timeout
        let elapsed = start.elapsed();

        // The stored permit means this should return immediately.
        assert!(
            elapsed < Duration::from_secs(1),
            "pre-notified wait_or_wake took {:?}, expected immediate return",
            elapsed
        );
    }
}
