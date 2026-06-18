# PLAN: Refresh responsiveness fix + shorter poll interval

_id: 001-refresh-responsiveness-fix_
_status: ready_
_last-updated: 2026-06-16_
_app: claude-overlay (Tauri 2 — Rust backend in `src-tauri/src`, TS frontend in `src`)_

> Planning only. No source code is edited by this document. Exact files, functions, and
> concrete constant values to change are listed below for the implementing agent.

---

## Requirement (restated)

**Explicit asks**
1. **Bug:** Usage refresh is inconsistent, and pressing **"Refresh Now"** (tray menu and the
   in-app context-menu item) does **not** reliably trigger a refresh.
2. Investigate and fix it.
3. **Enhancement:** Make refresh feel more real-time — target a **30s** interval if safe,
   **60s maximum**.

**Implied asks**
- A manual refresh must take effect **immediately** (within ~1s), not "whenever the current
  sleep happens to end."
- Polling more frequently must not trip Anthropic's 429 rate limiting on
  `https://api.anthropic.com/api/oauth/usage`.
- The adaptive-interval constants must stay internally consistent after lowering the base
  interval (today `POLL_FAST` = 120s would become **longer** than the new standard interval,
  which is inverted and nonsensical).
- Existing behaviours (idle-pause, 429 backoff/cooldown, reset-alignment, fallback-to-JSONL)
  must keep working and ideally also become wakeable by a manual refresh.

---

## Existing context touched (verified by reading the files)

### `src-tauri/src/poller.rs`
- `PollerState` (lines ~29–35): `Arc<Mutex<PollerState>>` aliased as `SharedPollerState`.
  Fields: `last_snapshot`, `plan_override`, `refresh_requested: bool`,
  `rate_limited_until: Option<Instant>`.
- `run()` is the single polling loop, spawned detached via `tauri::async_runtime::spawn`
  in `lib.rs`. Inside the loop:
  - **Top (lines ~102–111):** reads and *clears* `refresh_requested` into a local
    `refresh_requested` bool — this is the **only** place the flag is consumed.
  - **Idle-pause (lines ~113–118):** if not a manual refresh and system idle/locked,
    `sleep(current_interval).await; continue;` — a **bare, non-interruptible sleep**.
  - **Rate-limit cooldown (lines ~120–134):** if in backoff and not a manual refresh,
    `sleep(10).await; continue;` — another **bare sleep**.
  - **Auth-retry sleeps (lines ~146, ~154, ~281):** `sleep(AUTH_RETRY_SECS).await; continue;`.
  - **Bottom (lines ~333–364):** computes `sleep_secs` (either `current_interval` or a
    reset-aligned wait) then `sleep(Duration::from_secs(sleep_secs)).await;` — the **main
    inter-poll sleep**, up to `POLL_INTERVAL` (180s).
- Imports at top: `use tokio::time::sleep;` and `use tokio::sync::mpsc;`.

**The bug (confirmed):** `tokio::time::sleep` is not interruptible. When the user clicks
"Refresh Now" while the loop is parked in any of those `sleep().await` calls (most importantly
the bottom one, up to 180s), `refresh_requested = true` is set but the loop does not wake — it
only notices the flag the next time it loops back to the top. So a manual refresh is honoured
anywhere from instantly to ~3 minutes later, which reads as "inconsistent / broken."

### `src-tauri/src/lib.rs`
- Tray menu built with a **"Refresh Now"** item id `"refresh"` (lines ~33–34, ~45–49). Handler:
  `let mut s = state.lock().unwrap(); s.refresh_requested = true;` — sets the flag only.
- Poller spawned at lines ~77–79 with `state_for_poller` clone.
- `manage(poller_state.clone())` makes `SharedPollerState` available to commands.

### `src-tauri/src/window_ctl.rs`
- `request_refresh` command (lines ~90–104): checks `rate_limited_until`; if not rate-limited,
  `s.refresh_requested = true;` else returns `Err("Rate limited …")`. Sets the flag only.
- This is the command the in-app context-menu "Refresh Now" calls via `invoke('request_refresh')`.

### `src-tauri/src/config.rs`
- `POLL_INTERVAL = 180` (line 18) — standard cadence.
- `POLL_FAST = 120` (line 20) — "rising utilization" cadence.
- `POLL_FAST_EXTRA = 2` (line 22) — rapid follow-ups.
- `POLL_ERROR = 30` (line 24).
- `MAX_BACKOFF = 900` (line 26) — 429 cap (15 min).
- `IDLE_PAUSE = 300` (line 29).
- `AUTH_RETRY_SECS = 60` (line 33).
- `PROFILE_POLL_SECS = 3600` (line 35) — leave as-is.

### `src-tauri/src/model.rs`
- `UsageSnapshot.next_poll_in: u64` (line ~164) is serialized to the UI. No model change needed.

### `src-tauri/Cargo.toml`
- `tokio = { version = "1", features = ["full"] }` — so `tokio::sync::Notify` and
  `tokio::select!` are already available; **no dependency change required.**

### Frontend (`src/…`) — verified
- `src/store.ts`: `UsageSnapshot.next_poll_in: number` exists in the type only.
- **`next_poll_in` is never read/rendered anywhere** (grep across `src` shows only the type
  declaration in `store.ts`). The UI has **no "time until next poll" countdown.**
- `src/countdown.ts` + `src/components/window-bar.ts`: the 1-second countdown is driven purely
  by each quota window's `resets_at`, independent of poll cadence.
- `src/components/context-menu.ts` (lines ~62–68): "Refresh Now" item calls
  `invoke('request_refresh')`; disabled when `currentOptions.isRateLimited`.
- `src/main.ts` (lines ~39–46): derives `isRateLimited` from a `stale` status whose detail
  contains "rate limit", and calls `setRateLimited(...)`.

**Frontend conclusion:** Shortening the interval and making refresh instant requires **no
frontend code change** to function. The snapshot already pushes via the `usage://snapshot`
Tauri event, so a quicker backend poll automatically updates the UI faster. (One optional,
non-required UX follow-up noted at the end.)

---

## Gaps & open questions

- **[NON-BLOCKING] 30s vs 60s default interval — needs a decision; recommendation given below.**
  See "Open question" section. Default chosen for the plan: **60s** as the steady cadence, with
  30s reserved for the active/"rising" cadence. The implementer can flip to a 30s steady cadence
  by changing one constant if the team accepts the rate-limit risk.
- **[NON-BLOCKING]** Whether the idle-pause and 429-cooldown sleeps should *also* be wakeable by a
  manual refresh. Recommendation: **yes, make them wakeable** (small, consistent, and matches the
  "Refresh Now should always work" intent). Detailed below; if the implementer prefers minimal
  change, the bottom sleep alone fixes the reported bug, but cooldown/idle would remain
  up-to-10s / up-to-interval laggy for a manual refresh.
- **[NON-BLOCKING]** No automated test harness exists for the async poller loop today; verification
  is primarily manual (desktop app). Test approach below favours a small extractable helper plus
  manual validation.

No **BLOCKING** gaps — the design and values are fully determined; the only judgement call is the
30s-vs-60s default, for which a safe default is recommended so work can proceed.

---

## Design

### Part A — Make the inter-poll wait interruptible (the core bug fix)

**Mechanism: `tokio::sync::Notify` stored in shared state.** Recommended over a watch/mpsc
channel for these reasons:

- The poller is a **single consumer** and the wake is a simple **edge signal** ("wake now"),
  which is exactly `Notify`'s sweet spot — no payload to carry.
- `Notify` is `Send + Sync` and lives behind an `Arc`, so it can sit alongside the existing
  `Arc<Mutex<PollerState>>` **without** being inside the mutex. This matters: the loop must
  `.await` on the notify, and we must **not** hold the (`!Send`) `MutexGuard` across an `.await`
  (the existing code is already carefully written to drop the guard before every `.await` — see
  the comment at poller.rs lines ~121–122). Keeping `Notify` outside the mutex preserves that.
- `Notify::notify_one()` stores a permit if no one is currently waiting, so a notify that races
  just *before* the loop parks is not lost — the next `notified()` returns immediately. This
  closes the obvious race (user clicks during the brief window between consuming the flag and
  re-parking).
- A `watch` channel would also work but carries version/value semantics we don't need and is
  slightly more boilerplate; `mpsc` risks unbounded buffering of redundant wake messages. `Notify`
  is the smallest correct primitive. **Recommendation: `Notify`.**

**Shape of the change**

1. Introduce a wake handle type, e.g.:
   ```
   pub type RefreshNotify = std::sync::Arc<tokio::sync::Notify>;
   ```
   Manage it in Tauri alongside the poller state. Two clean options — pick one and keep it
   consistent:
   - **(Preferred)** Add a field to `PollerState`? No — `Notify` should not be inside the mutex.
     Instead store the `Arc<Notify>` as a **sibling managed state** and pass a clone into
     `run()`. So `run(sender, state, notify)`.
   - Alternatively wrap both in a small struct `PollerHandles { state, notify }`. The sibling
     approach is fewer moving parts.

2. In `run()`, replace the **bottom** bare sleep (poller.rs line ~364):
   ```
   sleep(Duration::from_secs(sleep_secs)).await;
   ```
   with a race between the sleep and the wake:
   ```
   tokio::select! {
       _ = sleep(Duration::from_secs(sleep_secs)) => {}
       _ = notify.notified() => {
           // woken by a manual refresh; loop back to top immediately
       }
   }
   ```
   The loop then returns to the top, where it reads `refresh_requested` (already set by the
   handler) and performs the fetch. No change to the top-of-loop flag logic is required.

3. **Setters notify after setting the flag.** Update both call sites so they set the flag **and**
   wake the loop:
   - `lib.rs` tray handler (lines ~45–49): after `s.refresh_requested = true;`, drop the lock and
     call `notify.notify_one();` on the managed `Arc<Notify>` (fetch via `app.state()`).
   - `window_ctl.rs::request_refresh` (lines ~90–104): same — on the success branch, after
     setting the flag, call `notify.notify_one();`. The notify handle is injected the same way as
     `state` (an extra `tauri::State<'_, RefreshNotify>` parameter on the command). Keep the
     existing rate-limit guard: a refresh that is rejected for rate-limiting must **not** notify.

4. **Wire-up in `lib.rs`:**
   - Create `let refresh_notify: RefreshNotify = Arc::new(tokio::sync::Notify::new());`
   - `.manage(refresh_notify.clone())` so commands can resolve it via `app.state()`.
   - Pass a clone into the spawned `run(tx, state_for_poller, refresh_notify.clone())`.
   - Register `request_refresh` already exists in `invoke_handler`; signature change is internal.

### Part B — Make the *other* loop-top sleeps wakeable too (recommended)

The reported bug is fully fixed by Part A for the common case. But three other sleeps can also
delay a manual refresh:

- **Idle-pause** (`sleep(current_interval).await`, line ~116): the top-of-loop check already
  uses `!refresh_requested`, so once the loop wakes it will *skip* the idle pause. The problem is
  only that, while *parked* in this sleep, a click won't wake it for up to `current_interval`.
- **Rate-limit cooldown** (`sleep(10).await`, line ~132): also guarded by `!refresh_requested` at
  the top, but the 429-cooldown branch in `request_refresh` already rejects manual refresh while
  rate-limited, so waking here matters less. Still, with the interval shortened, a 10s cooldown
  sleep is a small lag.

**Recommendation:** Wrap **both** of these in the same `select! { sleep | notify.notified() }`
pattern (idle-pause especially, since `IDLE_PAUSE`/`current_interval` can be large). This makes
"Refresh Now" feel instant in every state and is a tiny, uniform change. A small local helper is
worth introducing to avoid repeating the `select!` three times:
```
async fn wait_or_wake(secs: u64, notify: &tokio::sync::Notify) {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
        _ = notify.notified() => {}
    }
}
```
Then replace the bottom sleep, the idle-pause sleep, and the cooldown sleep with
`wait_or_wake(secs, &notify).await;`.

**Auth-retry sleeps** (lines ~146/154/281): these run when credentials are missing/expired. A
manual refresh can't fix expired auth, so waking them early just re-hits the same failure faster.
**Leave these as plain `sleep`** (explicitly out of scope) — documented here so the implementer
doesn't "fix" them. (Optional: they *could* use `wait_or_wake` for uniformity at no harm, but
it's unnecessary.)

> **Race note:** Because `Notify::notify_one()` stores one permit when no waiter is parked, the
> sequence "consume flag at top → (click happens here) → park on `wait_or_wake`" is safe: the
> stored permit makes `notified()` return immediately, so the click is not lost. Set the flag
> **before** calling `notify_one()` in the handlers so the woken loop always sees the flag.

### Part C — Shorten the interval and reconcile the adaptive constants

Edit `src-tauri/src/config.rs`. Recommended concrete values:

| Constant            | Current | New (recommended) | Rationale |
|---------------------|---------|-------------------|-----------|
| `POLL_INTERVAL`     | 180     | **60**            | Steady cadence; 60s is the safe real-time default re: 429s. |
| `POLL_FAST`         | 120     | **30**            | Must be **shorter** than the steady interval. 30s for "utilization rising." Fixes the inversion. |
| `POLL_FAST_EXTRA`   | 2       | **5** (or keep 2) | The 2s burst, fired 3× (`FAST_EXTRA_COUNT`) right after activity, is aggressive against rate limits now that base cadence is also faster. Bumping to ~5s is gentler; keeping 2 is acceptable since it's only 3 shots. **Recommend 5.** |
| `POLL_ERROR`        | 30      | **30** (unchanged)| Already reasonable; now equals `POLL_FAST`, which is fine. |
| `MAX_BACKOFF`       | 900     | **900** (unchanged)| 429 cap stays at 15 min. |
| `IDLE_PAUSE`        | 300     | **300** (unchanged)| Idle behaviour unchanged; still saves polls when the user is away. |
| `AUTH_RETRY_SECS`   | 60      | **60** (unchanged)| |
| `PROFILE_POLL_SECS` | 3600    | **3600** (unchanged)| Per requirement, leave as-is. |

**Key reconciliation point:** Today `POLL_FAST (120) < POLL_INTERVAL (180)`, so "fast" is
correctly faster. If we only lowered `POLL_INTERVAL` to 60 and left `POLL_FAST = 120`, "fast"
would become *slower* than steady — inverted and wrong. Lowering `POLL_FAST` to 30 restores the
invariant **`POLL_FAST_EXTRA (5) < POLL_FAST (30) ≤ POLL_ERROR (30) < POLL_INTERVAL (60)`**.

> Also note: `backoff_secs` is initialised to `POLL_INTERVAL` (poller.rs line 92) and reset to
> `POLL_INTERVAL` on success (line 191). With `POLL_INTERVAL = 60`, the first 429 doubles 60 → 120
> → 240 … capped at 900. That is a *gentler* starting backoff than today's 180 and is fine.

### Rate-limit safety analysis (30s vs 60s)

The usage endpoint is polled once per cycle (plus an occasional profile fetch on a 1-hour
cadence). Going from 180s → 60s roughly **triples** steady request volume (about 60 requests/hour
vs ~20). Going to 30s would be ~120 requests/hour, and the existing `POLL_FAST_EXTRA` bursts and
reset-alignment polls add a few extra. Anthropic's OAuth usage endpoint is lightweight but not
documented as unlimited; the app already implements exponential backoff to `MAX_BACKOFF = 900s`
on 429, so a too-aggressive cadence degrades gracefully rather than breaking — but it would show
"Stale — rate limited" to the user, which is the exact "inconsistent" feeling we're trying to
remove.

**Recommendation: default the steady interval to 60s, not 30s.** 60s already delivers the
"feels real-time" improvement (3× fresher than today) while keeping request volume modest and
well clear of plausible per-minute limits. Reserve 30s for the **active** path (`POLL_FAST`), so
the app *does* poll every 30s precisely when the user is actively burning quota and wants live
numbers — which is the moment real-time matters most — then relaxes back to 60s when steady. This
gives near-30s responsiveness when it counts without sustaining 120 req/hour around the clock.
Combined with **instant manual refresh** (Part A), the perceived latency is effectively zero when
the user explicitly asks, so a 30s *steady* cadence buys little extra at meaningful rate-limit
cost.

If the team still wants a 30s steady cadence: set `POLL_INTERVAL = 30` and `POLL_FAST = 15` (or
keep `POLL_FAST = 30` and accept that fast == steady). Watch for 429s in logs after rollout.

---

## Plan (ordered task list)

1. **config.rs — interval values.** Update constants per the table in Part C:
   `POLL_INTERVAL 180→60`, `POLL_FAST 120→30`, `POLL_FAST_EXTRA 2→5` (optional), leave
   `POLL_ERROR/MAX_BACKOFF/IDLE_PAUSE/AUTH_RETRY_SECS/PROFILE_POLL_SECS` unchanged.
   Files: `src-tauri/src/config.rs`. Skill: backend (Rust; no specific skill loaded — see note).

2. **poller.rs — wake plumbing.** Add `pub type RefreshNotify = Arc<tokio::sync::Notify>;`
   (or reuse a chosen name) and change `run()`'s signature to accept the notify handle:
   `pub async fn run(sender: SnapshotSender, state: SharedPollerState, notify: RefreshNotify)`.
   Add the private `wait_or_wake(secs, &notify)` helper. Keep `Notify` **outside** the mutex.
   Files: `src-tauri/src/poller.rs`.

3. **poller.rs — interruptible waits.** Replace the bottom main sleep (line ~364) with
   `wait_or_wake(sleep_secs, &notify).await;`. Replace the idle-pause sleep (line ~116) and the
   rate-limit cooldown sleep (line ~132) with `wait_or_wake(...).await;`. **Do not** change the
   three auth-retry sleeps (lines ~146/154/281) — leave them as plain `sleep`. Confirm no
   `MutexGuard` is held across any `.await` (the notify lives outside the mutex, so this holds).
   Files: `src-tauri/src/poller.rs`.

4. **lib.rs — create + manage + spawn + tray.** Create
   `let refresh_notify: RefreshNotify = Arc::new(tokio::sync::Notify::new());`, add
   `.manage(refresh_notify.clone())`, pass a clone into the spawned
   `crate::poller::run(tx, state_for_poller, refresh_notify.clone())`, and in the tray
   `"refresh"` handler set `s.refresh_requested = true;`, drop the guard, then resolve the
   managed notify (`app.state::<RefreshNotify>()`) and call `notify_one()`.
   Files: `src-tauri/src/lib.rs`.

5. **window_ctl.rs — notify on manual refresh.** Add a `notify: tauri::State<'_, RefreshNotify>`
   parameter to `request_refresh`; on the non-rate-limited success branch, after
   `s.refresh_requested = true;`, drop the lock and call `notify.notify_one();`. Keep the
   rate-limited branch returning `Err(...)` **without** notifying. Import `RefreshNotify` from
   `crate::poller`. Files: `src-tauri/src/window_ctl.rs`.

6. **Frontend — confirm no change needed.** Verified: `next_poll_in` is not rendered; the only
   UI cadence is the `resets_at` countdown. No edits required to `src/store.ts`, `src/countdown.ts`,
   `src/components/window-bar.ts`, or `src/components/context-menu.ts` for correctness. (See
   optional follow-up.) Files: none.

7. **Build + manual verification** (see Tests required).

> **Skill note:** No Rust/Tauri-specific skill is present in the available skill set (the backend
> skills cover ASP.NET/Node/Python). The plan therefore relies only on patterns verified directly
> in this repo (guard-drop-before-await discipline, existing `Arc<Mutex<…>>` + `manage` wiring,
> `tokio` "full" features already enabled). No framework behaviour was assumed beyond standard
> `tokio::sync::Notify` / `tokio::select!` semantics.

---

## Tests required

This is a desktop Tauri app with no existing async-loop test harness, so verification is mostly
manual, supported by one cheap unit-testable seam.

**Automated (cheap, optional but recommended):**
- Extract the wake-vs-timeout race into the testable `wait_or_wake` helper and add a `#[tokio::test]`
  (using `tokio::time` paused/auto-advance or a short real sleep) asserting that:
  - `notify_one()` called before/while waiting causes `wait_or_wake` to return well before the
    timeout, and
  - with no notify, it returns at ~the timeout. This proves the interruptibility without booting
    Tauri. Place under `src-tauri/src/poller.rs` `#[cfg(test)]` (matching the existing test style
    in `model.rs`).
- `cargo build` / `cargo clippy` must pass; confirm no `MutexGuard`-across-`await` regressions
  (clippy/`Send` errors would surface these).

**Manual (primary acceptance):**
1. Run the app (`cargo tauri dev` / the project's run task). Observe an initial fetch.
2. **Refresh Now (tray):** right-click tray → "Refresh Now." Confirm a fetch fires **immediately**
   (watch logs for the usage fetch line / observe the snapshot/`fetched_at` updating within ~1s),
   even if clicked right after a poll when the loop is mid-sleep.
3. **Refresh Now (in-app context menu):** right-click the overlay → "Refresh Now." Same instant
   behaviour. Confirm it's disabled/rejected while rate-limited (existing guard).
4. **Steady cadence:** with the app idle-but-active, confirm polls now occur ~every 60s (logs),
   not every 180s.
5. **Fast cadence:** drive utilization up (use Claude Code) and confirm the loop switches to ~30s
   (`POLL_FAST`) and the 3 follow-up bursts (`POLL_FAST_EXTRA`) fire, then relaxes to 60s.
6. **Idle-pause still works:** leave the machine idle past `IDLE_PAUSE` (300s) and confirm polling
   pauses; then click "Refresh Now" and confirm it wakes and fetches immediately (Part B).
7. **429 backoff intact:** if reproducible, confirm a 429 still backs off (doubling from 60 toward
   900) and surfaces the "Stale — rate limited" status, and that a manual refresh is rejected
   while rate-limited.
8. Watch logs over ~30 min of normal use for unexpected 429s at the new cadence; if they appear,
   revisit the 30s-vs-60s decision (see Open question).

---

## Risks & rollback

- **Rate limiting (primary risk):** 3× (60s) or 6× (30s) more requests/hour could trip 429s.
  Mitigated by the existing exponential backoff and by choosing 60s steady / 30s active. Rollback
  is trivial: the cadence is entirely in `config.rs` constants — bump `POLL_INTERVAL`/`POLL_FAST`
  back up without touching logic.
- **`MutexGuard` across `.await`:** introducing `.await` points must not hold the lock. The notify
  lives outside the mutex and the handlers drop the guard before `notify_one()`, so this is safe;
  clippy will catch any slip (`await_holding_lock`). Low risk.
- **Lost-wakeup race:** addressed by `Notify`'s stored-permit semantics and by setting the flag
  *before* notifying. Low risk.
- **Spurious extra fetch:** if a notify arrives with no pending `refresh_requested` (shouldn't
  happen given handlers set the flag first), the loop simply does one normal early poll — benign.
- **Behavioural change to idle/cooldown waits (Part B):** making them wakeable is a deliberate,
  small change; if undesired, revert just those two `wait_or_wake` calls back to `sleep` — the
  core bug fix (bottom sleep) is independent.
- **Overall rollback:** revert the constant changes and the `Notify` wiring (3 backend files
  + config) to return to current behaviour. No data/migration concerns; no contract change.

---

## Open question — 30s vs 60s default (with recommendation)

**Question:** Should the *steady* poll interval be 30s (max real-time) or 60s (safer)?

**Recommendation: 60s steady, 30s active (`POLL_FAST`).** Rationale:
- 60s already triples freshness vs today and stays well under plausible per-minute limits.
- The active path drops to 30s exactly when the user is burning quota and wants live numbers.
- **Instant manual refresh** (this fix) makes on-demand latency ~0, so a 30s *steady* cadence adds
  little perceived benefit at double the steady request volume and a higher 429 risk — the very
  symptom ("inconsistent / stale") we're removing.
- One-line escape hatch if the team disagrees: set `POLL_INTERVAL = 30` (and `POLL_FAST = 15`) and
  monitor logs for 429s.

This keeps the result inside the requirement's stated bound (30–60s) while defaulting to the safe
end.

---

## Suggested follow-ups (out of scope; do not implement now)

- **Optional UX:** Surface a tiny "next refresh in Xs" or "updated Ys ago" indicator using the
  already-serialized `next_poll_in` / `fetched_at` (currently `next_poll_in` is unused on the
  frontend). Would make the faster cadence visible. Frontend-only, additive.
- **Optional:** Make the auth-retry sleeps wakeable for full uniformity (no functional need).
- **Optional:** Telemetry/log counter of polls-per-hour and 429 count to validate the cadence
  choice empirically after rollout.

---

## Work units (single-developer; minimal parallelism)

This is a small, tightly-coupled backend change (shared notify handle threads through 4 files),
so parallel split offers little and risks the disjoint-file rule. Recommended as **one backend
unit**, optionally with a trivially-parallel config unit.

- [ ] **Unit A — backend — agent: developer-backend** — Interruptible-refresh wake + interval tuning.
  - files (exclusive): `src-tauri/src/poller.rs`, `src-tauri/src/lib.rs`,
    `src-tauri/src/window_ctl.rs`, `src-tauri/src/config.rs`
  - depends on: none
  - skill: none stack-specific available (Rust/Tauri) — follow repo conventions verified above
  - status: pending
- [ ] **Unit B — frontend — agent: developer-frontend** — *No-op / verification only.* Confirm no
  frontend change is required for correctness; optionally implement the "next refresh in Xs"
  indicator **only if explicitly requested** (otherwise skip).
  - files (exclusive, only if the optional indicator is approved): `src/main.ts`,
    `src/components/usage-card.ts`
  - depends on: none (reads the existing `next_poll_in` contract)
  - status: pending (expected: closed as "no change needed")

## Contracts (seams)

No contract change. `UsageSnapshot` (Rust `model.rs` ↔ TS `store.ts`) is unchanged, including
the existing `next_poll_in: u64`/`number` field. The Tauri command surface is unchanged from the
frontend's view: `invoke('request_refresh')` keeps the same name and signature (the added
`Notify` state parameter is injected by Tauri and invisible to the caller).

## Parallel schedule

Unit A is the whole fix and runs alone. Unit B is verification-only and can run concurrently but
is expected to result in no edits unless the optional UX follow-up is approved.
