# Implementation Plan: Claude Usage Overlay (working title `claude-overlay`)

_Author: planner agent_
_Date: 2026-06-16_
_Status: ready for implementation_
_Target: greenfield desktop app, primary platform Windows 11, cross-platform-aware_

> **Decisions confirmed by the user (2026-06-16):**
> 1. **Stack = Tauri 2.**
> 2. **Refresh must feel faster than a flat 180s** → use **adaptive polling** modelled on the
>    reference project [`jens-duttke/usage-monitor-for-claude`](https://github.com/jens-duttke/usage-monitor-for-claude)
>    (see §5.3). That project (Python + pywebview, ~12.5 MB) does essentially the same job and is
>    the source for the exact endpoints, headers, and interval defaults used below.
> 3. **Reading `~/.claude/.credentials.json` read-only is approved.**
>
> _Key research upgrades from the reference project:_ a second endpoint **`/api/oauth/profile`**
> gives **authoritative plan detection** (no more guessing Max tier from utilization), and the
> usage endpoint returns **more quota types than first thought** (`seven_day_cowork`,
> `seven_day_oauth_apps`, …) — so the UI must render quota bars **dynamically**.

---

## 1. Product summary

A tiny always-on-top desktop widget that floats above all windows and shows **live Claude
subscription usage**: which plan is active, the 5-hour session window, the weekly (7-day)
windows (all-models and Sonnet-specific), overage/extra-usage state, and reset countdowns.

Core window requirements:

1. Always-on-top overlay.
2. Borderless (no title bar / chrome).
3. Configurable opacity.
4. Draggable by grabbing the widget body.
5. Resizable.
6. Closeable + re-openable from the taskbar icon (toggle show/hide); keeps a taskbar presence
   despite being borderless.
7. Detect active plan (Free / Pro / Max 5x / Max 20x / other) and show the relevant limits.
8. No persistence — purely a live visual readout. (Persistence noted as a future extension.)

Non-functional: as lightweight as possible, reliable, **not** over-engineered, modern (not
outdated) tech, Windows 11 first with a cheap path to macOS/Linux later.

---

## 2. Data source research (the highest-risk area — read this first)

This section is the result of concrete research into how a desktop app can obtain real-time
Claude usage stats. **Be honest: every viable source is unofficial.** There is no public,
documented Anthropic "consumer usage" API for subscription (Pro/Max) plans. The findings below
rank the options by reliability.

### 2.1 Primary source — the OAuth usage endpoint (what Claude Code itself uses)

Claude Code and the claude.ai dashboard populate their quota bars from an **undocumented**
endpoint:

```
GET https://api.anthropic.com/api/oauth/usage
```

Required headers (all four matter):

| Header | Value | Notes |
|---|---|---|
| `Authorization` | `Bearer <oauth_access_token>` | Token read from local Claude Code credentials (see 2.2). |
| `anthropic-beta` | `oauth-2025-04-20` | Required; beta gate for the OAuth surface. |
| `User-Agent` | `claude-code/<version>` | **Critical.** Without a `claude-code/...` UA you land in an aggressively rate-limited bucket and get persistent 429s. The reference project pins a fallback of `claude-code/2.1.85` and otherwise uses the locally-installed version. |
| `Content-Type` | `application/json` | Standard. |

Method is **GET**; the reference project uses a **10s timeout**. Verified against
`jens-duttke/usage-monitor-for-claude/usage_monitor_for_claude/api.py`, whose header dict is
exactly:

```python
{ 'Authorization': f'Bearer {token}', 'Content-Type': 'application/json',
  'User-Agent': _user_agent(), 'anthropic-beta': 'oauth-2025-04-20' }
```

Observed response shape (the authoritative window state — same data that powers the in-CLI
`/usage` and dashboard bars). This **does not consume inference tokens** and does not count
against usage. Note the quota set is **open-ended** — Anthropic adds new ones, so parse it as a
map, not a fixed struct:

```jsonc
{
  "five_hour":          { "utilization": 37, "resets_at": "2026-06-16T18:00:00Z" },
  "seven_day":          { "utilization": 12, "resets_at": "2026-06-20T09:00:00Z" },
  "seven_day_opus":     null,                     // object or null depending on plan/model use
  "seven_day_sonnet":   { "utilization": 8,  "resets_at": "2026-06-20T09:00:00Z" },
  "seven_day_cowork":   { "utilization": 0,  "resets_at": "2026-06-20T09:00:00Z" }, // newer quota
  "seven_day_oauth_apps": null,                   // newer quota
  "extra_usage": {
    "is_enabled":    true,
    "monthly_limit": null,                        // value or null
    "used_credits":  null,                         // value or null  → "current overusage" / balance
    "utilization":   null
  }
}
```

Field semantics:
- `utilization` — integer-ish percentage 0–100 of that window consumed; `0` when no active
  session in the window.
- `resets_at` — ISO-8601 UTC timestamp when that window rolls over → drives the countdown.
- `seven_day` — combined all-models weekly.
- `seven_day_sonnet` — Sonnet-specific weekly (Max plans surface this separately).
- `seven_day_opus` — Opus weekly bucket, may be null.
- `seven_day_cowork`, `seven_day_oauth_apps`, … — additional/newer weekly buckets; treat the
  whole object as a dictionary of `{name → {utilization, resets_at}}` and render every non-null
  entry dynamically (this is what the reference project does — "dynamically detected usage bars
  for all active quota types … and any new quotas Anthropic adds").
- `extra_usage` — the "overusage enabled / current overusage / balance" data:
  `is_enabled` = overage toggle; `used_credits` = current overusage; `monthly_limit` = the
  overage ceiling / remaining-balance basis.

### 2.1b Profile endpoint — authoritative plan/account detection

```
GET https://api.anthropic.com/api/oauth/profile
```

Same four headers as above. Returns account + organization + application info. Relevant fields:

```jsonc
{
  "account":      { "uuid": "...", "full_name": "...", "display_name": "...",
                    "email": "...", "has_claude_max": true, "has_claude_pro": false,
                    "created_at": "..." },
  "organization": { "uuid": "...", "name": "...", "organization_type": "...",
                    "billing_type": "...", "rate_limit_tier": "...",
                    "has_extra_usage_enabled": true, "subscription_status": "...",
                    "subscription_created_at": "..." },
  "application":  { "uuid": "...", "name": "...", "slug": "..." }
}
```

This is the **primary plan-detection source** (see §2.3): `has_claude_max` / `has_claude_pro` and
`organization.rate_limit_tier` give a far more reliable plan label than inferring from the usage
payload. Poll it much less often than usage (plan rarely changes) — e.g. once at startup and then
hourly.

> **Critical caveat (de-risks the whole design):** there is an open report
> (anthropics/claude-code#31637) that this endpoint rate-limits **aggressively** — once 429ed it
> can stay 429 for 30+ minutes even with exponential backoff. A separate report
> (Claude-Code-Usage-Monitor#202) finds that **with the correct `claude-code/<version>`
> User-Agent, polling at ~180s intervals is safe.** Therefore "real-time" here means **poll
> every 3 minutes (180s), not every few seconds.** The UI must degrade gracefully to a "stale"
> state and back off hard on 429. See refresh strategy (§5.3).

### 2.2 Where the OAuth token lives (local, per-OS)

Confirmed from Claude Code's authentication docs:

- **Windows:** `%USERPROFILE%\.claude\.credentials.json` — plaintext JSON, secured by NTFS ACLs
  on the user profile (restricted to the current user). **This is the path we use.**
- **Linux:** `~/.claude/.credentials.json`, mode `0600`.
- **macOS:** encrypted **Keychain** (no flat file) — the eventual macOS port must read Keychain
  instead. Out of scope now but noted for the port.
- If `CLAUDE_CONFIG_DIR` is set (Linux/Windows), the file lives under that directory instead —
  the app must honour this env var.

Credentials file shape:

```jsonc
{
  "claudeAiOauth": {
    "accessToken":  "sk-ant-oat01-...",
    "refreshToken": "sk-ant-ort01-...",
    "expiresAt":    1748276587173,           // epoch ms
    "scopes":       ["user:inference", "user:profile"],
    "subscriptionType": "max",                // MAY be present — useful hint for plan detection
    "rateLimitTier":    "..."                  // MAY be present
  }
}
```

Token handling rules for the app:
- Read `accessToken` fresh **every poll** (it can be rotated/refreshed by Claude Code at any time).
- Check `expiresAt`; if expired, do **not** attempt our own refresh in v1 (refresh requires the
  OAuth client flow and risks corrupting Claude Code's own credential state). Instead show an
  "auth expired — open Claude Code to re-login" state. (Self-refresh is a future extension.)
- Never write to `.credentials.json`. Read-only.

### 2.3 Plan detection

In priority order:
1. **`/api/oauth/profile`** (§2.1b) — the authoritative source. `account.has_claude_max` /
   `account.has_claude_pro` give the plan family; `organization.rate_limit_tier` is the best
   available signal for the **Max 5x vs 20x** distinction (the tier string encodes the quota
   multiplier). `has_extra_usage_enabled` confirms the overage toggle independently of the usage
   payload. This is the main reason to call the profile endpoint at all.
2. **`subscriptionType` / `rateLimitTier`** in `.credentials.json` if present (offline hint,
   usable before the first profile call returns).
3. **Inference from the usage payload shape** (last-resort fallback if profile is unreachable):
   - Free / Pro → typically only `five_hour` + `seven_day`, no separate Sonnet weekly.
   - Max → `seven_day_sonnet` present alongside `seven_day`.
4. If nothing is detectable → "Unknown plan" state, still render whatever windows came back.

> Honesty note: even with `rate_limit_tier`, the exact 5x/20x label is best-effort because the
> tier strings are undocumented. Map known tier strings confidently; for anything unrecognized,
> label "Max" and expose a manual override in the context menu (cheap; no persistence needed
> within a run). The app labels confidently only what it can prove and shows what the endpoints
> report otherwise.

### 2.4 Fallback source — local JSONL transcripts

If the endpoint is unreachable/429/expired-auth, fall back to **local usage logs**:
- Claude Code writes one JSONL file per session to `%USERPROFILE%\.claude\projects\**/*.jsonl`.
- Each assistant record carries `message.usage.{input_tokens, output_tokens,
  cache_creation_input_tokens, cache_read_input_tokens}` and `message.model`.
- We can aggregate token consumption per rolling 5h / 7d window locally. **This cannot produce
  an authoritative utilization percentage** (we don't know the plan's absolute cap), but it can
  show "tokens used this session / this week" and a relative trend so the widget is still useful
  when the endpoint is unavailable.
- This source costs nothing, never rate-limits, and is fully offline.

### 2.5 What is explicitly NOT a source
- The Anthropic **Console / platform API** usage endpoints are for **API-key** (pay-per-token)
  accounts, not Pro/Max subscriptions — not applicable.
- The chat-completions API would consume tokens — never used for polling.

### 2.6 ToS / risk statement (must be surfaced in README)
- The `/api/oauth/usage` endpoint is **undocumented and unofficial**; reading another app's
  (Claude Code's) local credential file is reverse-engineering. Anthropic may change the
  endpoint, headers, response shape, or rate-limit policy without notice, or could consider
  programmatic access a ToS concern. The app is read-only, non-inference, and mirrors what the
  user already sees in their own Claude Code, which minimizes (but does not eliminate) risk.
- Design so the data layer is swappable: if Anthropic ships an official usage API, only one
  module changes.

---

## 3. Tech stack recommendation

### 3.1 Requirements that constrain the choice
Borderless + per-pixel/window opacity + always-on-top + drag-to-move + resize + **taskbar icon
that toggles a borderless window** + small memory/disk footprint + modern + room to port to
macOS/Linux. The app also needs to make plain HTTPS calls and read a local file — trivial for
any stack.

### 3.2 Candidate comparison

| Stack | Bundle / disk | Idle RAM | Always-on-top + transparency + click handling | Taskbar toggle of borderless window | Cross-platform | Verdict |
|---|---|---|---|---|---|---|
| **Tauri 2 (Rust core + tiny web UI)** | ~3–10 MB installer; uses OS WebView2 (already on Win11) | ~40–80 MB | First-class: `alwaysOnTop`, `decorations:false`, `transparent:true`, `setIgnoreCursorEvents` for click-through, programmatic move/resize, `startDragging()` | `skipTaskbar` configurable; tray + show/hide built in | Excellent (Win/mac/Linux) | **Primary** |
| **WPF (.NET, C#)** | needs .NET runtime (or ~60–150 MB self-contained); Windows-only | ~60–120 MB | Excellent native: `Topmost`, `WindowStyle=None`, `AllowsTransparency`, per-window opacity, `DragMove()`, custom resize grips | Easy (`ShowInTaskbar`, NotifyIcon) | **Windows only** (no port) | **Runner-up** |
| WinUI 3 | needs Windows App SDK; heavier deploy | ~80–150 MB | Good but transparency/borderless is fiddlier than WPF; newer API churn | Yes | Windows only | Rejected: more complexity, weaker transparency story than WPF, no port |
| Electron | 80–150+ MB installer (bundles Chromium) | 150–300+ MB | Full support | Yes | Excellent | Rejected: violates "as lightweight as possible" |
| Avalonia (.NET) | self-contained ~40–80 MB | ~80–150 MB | Good, cross-platform | Yes | Good | Viable XPlat-native option but heavier and more ceremony than Tauri for a tiny widget |
| Flutter desktop | ~20–40 MB | ~80–150 MB | Possible via window plugins; transparency/click-through less mature on Windows | Plugin-dependent | Good | Rejected: window-management maturity risk for exactly our hard requirements |
| Raw Win32 / C++ | smallest | smallest | Maximum control | Yes | Windows only, painful port | Rejected: over-engineering; slow to build; not modern DX |

### 3.3 Recommendation

**Primary: Tauri 2** (Rust backend + minimal HTML/CSS/vanilla-TS or a featherweight UI lib).

Why it wins for this exact product:
- Smallest practical footprint for a modern stack on Win11 because it reuses the bundled
  **WebView2** instead of shipping a browser engine.
- Window requirements are all first-class Tauri APIs: `alwaysOnTop`, `decorations: false`,
  `transparent: true`, runtime `setOpacity`/CSS opacity, `appWindow.startDragging()` for
  grab-anywhere move, programmatic resize + native resize via `resizable: true`, `skipTaskbar`
  control, and a **tray/taskbar toggle** to show/hide.
- Rust side is the natural home for the polling + credential-reading data layer (fast, no
  GC pauses, easy HTTPS via `reqwest`, easy file watch).
- Cheapest cross-platform path of all native-ish options: the same codebase targets macOS/Linux;
  only the credential-source module (Keychain on macOS) needs an OS branch later.

Trade-off accepted: requires the Rust toolchain to build (not to run). For a single-developer
greenfield widget that's fine, and the runtime payoff (tiny, fast) is large.

**Runner-up: WPF (.NET 10, C#)** — if the developer strongly prefers C#/.NET, has no
cross-platform ambition, or wants zero web tech. WPF's transparency + borderless + `DragMove`
story is the best of the native Windows options and extremely battle-tested. The cost is a
Windows-only lock-in and a larger deploy if self-contained. Choose this only if the Tauri/Rust
toolchain is a non-starter.

> The rest of this plan is written for the **Tauri 2** primary recommendation, but the
> architecture (§4) is stack-neutral so the WPF fallback reuses the same module boundaries.

---

## 4. Architecture

Clean separation between **data acquisition** (Rust) and **presentation** (WebView UI). The UI
never touches credentials or HTTP; it only receives a normalized snapshot.

```
+--------------------------------------------------------------+
|                        Tauri App                             |
|                                                              |
|  Rust core (src-tauri/)                                      |
|  ┌───────────────────────────────────────────────────────┐  |
|  | credential_source  → locate + read .credentials.json   |  |
|  |                      (honours CLAUDE_CONFIG_DIR)        |  |
|  | usage_client       → GET /api/oauth/usage (reqwest)    |  |
|  |                      correct headers, backoff, 429 mgmt |  |
|  | fallback_logs      → aggregate ~/.claude/projects JSONL |  |
|  | plan_detector      → classify plan from creds+payload  |  |
|  | poller             → 180s loop, emits UsageSnapshot     |  |
|  | window_ctl         → tray toggle, opacity, drag, resize |  |
|  └───────────────────────────────────────────────────────┘  |
|            │  emits "usage://snapshot" events                |
|            ▼                                                 |
|  WebView UI (src/)                                           |
|  ┌───────────────────────────────────────────────────────┐  |
|  | state store (last snapshot, status: live/stale/error)  |  |
|  | <UsageCard> per active window (5h, 7d, sonnet, extra)  |  |
|  | countdown ticker (local, 1s) off the resets_at fields  |  |
|  | context menu (opacity slider, size, plan override,quit)|  |
|  └───────────────────────────────────────────────────────┘  |
+--------------------------------------------------------------+
```

### Module responsibilities

- **`credential_source`** — resolve path (`CLAUDE_CONFIG_DIR` else `%USERPROFILE%\.claude`),
  read + parse JSON, return `{ access_token, expires_at, subscription_type? }`. Read fresh per
  poll. Pure read-only.
- **`usage_client`** — build the request with the four required headers (UA string pulled from a
  const, e.g. `claude-code/<pinned-version>`), parse the JSON into a typed `RawUsage`. Classify
  HTTP results: `Ok`, `RateLimited(429)`, `Unauthorized(401)`, `NetworkError`.
- **`fallback_logs`** — scan `projects/**/*.jsonl`, sum token usage into rolling 5h/7d buckets;
  return a `FallbackUsage` (token counts + relative %, no authoritative cap).
- **`plan_detector`** — map credentials + payload → `Plan` enum + which windows to display.
- **`poller`** — orchestrates: on each tick, try `usage_client`; on success emit a `live`
  snapshot; on 429 keep last snapshot, mark `stale`, increase interval; on auth-expired emit
  `auth_expired`; on hard failure fall back to `fallback_logs` and emit `degraded`.
- **`window_ctl`** — Tauri commands invoked from the UI context menu: set opacity, set size,
  toggle click-through (optional), and the tray show/hide handler.

---

## 5. Data layer detail

### 5.1 Data model (normalized snapshot the UI consumes)

```rust
enum Plan { Free, Pro, Max5x, Max20x, Max /*ambiguous*/, Unknown }

// Quota windows are NOT a fixed set — parse the usage object as a dictionary so new
// Anthropic quota types appear automatically.
struct QuotaWindow {
    key: String,          // raw key, e.g. "five_hour", "seven_day_sonnet", "seven_day_cowork"
    label: String,        // display label derived from key ("5-hour session", "Weekly (Sonnet)")
    utilization: f32,     // 0..=100
    resets_at: Option<DateTime<Utc>>,
}

struct ExtraUsage {
    enabled: bool,
    used_credits: Option<f64>,    // "current overusage"
    monthly_limit: Option<f64>,   // overage ceiling / balance basis
    utilization: Option<f32>,
}

// From /api/oauth/profile — drives plan detection + account label.
struct Profile {
    display_name: Option<String>,
    email: Option<String>,
    has_claude_max: bool,
    has_claude_pro: bool,
    rate_limit_tier: Option<String>,        // best signal for Max 5x vs 20x
    has_extra_usage_enabled: bool,
    subscription_status: Option<String>,
}

enum SourceStatus { Live, Stale(reason), Degraded /*from logs*/, AuthExpired, Error(msg) }

struct UsageSnapshot {
    plan: Plan,
    profile: Option<Profile>,
    windows: Vec<QuotaWindow>,    // ordered, dynamic — render every entry
    extra_usage: Option<ExtraUsage>,
    status: SourceStatus,
    fetched_at: DateTime<Utc>,
}
```

Display ordering of `windows`: a small priority map keyed on the raw quota key
(`five_hour` first, then `seven_day`, then `seven_day_sonnet`/`opus`/`cowork`/…), with any
unknown keys appended in payload order and given a humanized label. This keeps the layout stable
while still surfacing brand-new quotas the moment Anthropic ships them.

### 5.2 Plan → which windows are shown

| Plan | 5h session | Weekly all-models | Weekly Sonnet | Extra usage block |
|---|---|---|---|---|
| Free | yes | (if returned) | no | hide |
| Pro | yes | yes | no | show (overusage/balance) |
| Max 5x / 20x | yes | yes | yes | show (overusage/balance) |
| Unknown | render whatever fields came back | | | |

### 5.3 Refresh / "real-time" strategy — **adaptive polling** (user requirement: feel faster than flat 180s)

Modelled directly on the reference project's proven scheme. The trick to "feeling real-time"
without tripping the 429 wall is to **poll faster only when usage is actually moving** and let
**local 1s countdown tickers** carry the UI between network calls. Interval defaults (all
constants in one config module, all in seconds):

| Constant | Default | Meaning |
|---|---|---|
| `poll_interval` | **180** | Standard cadence when usage is steady. |
| `poll_fast` | **120** | Cadence while utilization is actively increasing. |
| `poll_fast_extra` | **2** | A few rapid follow-up checks right after activity stops, to catch the final jump quickly, then settle back to standard. |
| `poll_error` | **30** | Retry cadence after transient 5xx / network errors. |
| `max_backoff` | **900** | Hard cap for 429 exponential backoff (15 min). |
| `idle_pause` | **300** | After this much OS idle (or when the workstation is **locked**), pause polling entirely; `0` disables. |

Behaviour:
- **Adaptive speed-up:** compare the new utilization to the last snapshot. If any window's
  utilization rose, switch to `poll_fast`; when it stops rising, fire a short burst at
  `poll_fast_extra` then relax to `poll_interval`. → live-feeling during active Claude sessions,
  quiet when idle.
- **Local countdown tickers** still update `resets_at` displays every 1s with no network cost.
- **Idle / lock pause:** detect OS idle time and lock state (Windows: `GetLastInputInfo` /
  session-lock events); pause the loop past `idle_pause` and resume on activity. This both saves
  battery and avoids burning quota on the rate-limited endpoint while away.
- **Reset alignment:** when a window's `resets_at` is imminent, schedule a poll just after it to
  capture the rollover promptly.
- **On HTTP 429:** keep last good snapshot, mark `Stale`, exponential backoff capped at
  `max_backoff` (900s); a success resets to normal cadence.
- **On 401 / expired token:** emit `AuthExpired`, stop endpoint polls, re-read credentials every
  60s (user may re-login in Claude Code). Never self-refresh in v1.
- **On 5xx / network error:** retry at `poll_error`; after repeated failure fall back to
  `fallback_logs`, mark `Degraded`.
- **Profile endpoint** (§2.1b) is polled on its own slow timer (startup + ~hourly), independent
  of the usage cadence above.

---

## 6. UI / UX design

### 6.1 Layout (compact card)
A small rounded card (default ~260×180, resizable). Top row: plan badge ("MAX 20x") + optional
account/display-name (from profile) + tiny status dot (green=live, amber=stale, grey=degraded/
auth). Body: one horizontal progress bar **per quota window present in the snapshot**, rendered
**dynamically** from `snapshot.windows` (5h, weekly all-models, Sonnet, Opus, Cowork, and any new
quota) — each with label, percentage, and a `resets in 2h 14m` countdown. Footer (when
`extra_usage.enabled`): "Overusage: on · used 1.20" line. Because the bar count varies by plan and
by Anthropic's quota set, the card height auto-fits its content within the user's chosen size.

Visual: semi-transparent dark card; respects the global opacity setting. No window chrome.

### 6.2 States
- **Live** — bars + countdowns, green dot.
- **Stale** — last values dimmed, amber dot, "updated 4m ago".
- **Degraded (logs)** — bars show token-based relative usage with a "local estimate" tag.
- **Auth expired** — message "Sign in to Claude Code to see usage" + grey dot.
- **No data / unknown plan** — minimal "Waiting for Claude usage…".

### 6.3 Controls (surfaced without window chrome)
- **Right-click anywhere → context menu:**
  - Opacity submenu / slider (e.g. 30%–100%).
  - Size presets (Small / Medium / Large) + "resizable edges" toggle.
  - Plan override (Auto / Pro / Max 5x / Max 20x) — for the ambiguous-Max case.
  - "Refresh now" (respects backoff; disabled while rate-limited).
  - Quit.
- **Drag:** left-click-drag on the card body moves the window (`startDragging()`); interactive
  controls (slider) stop propagation so they don't drag.
- **Resize:** native resizable edges (`resizable: true`); optionally a small bottom-right grip.
- **Taskbar/tray icon click:** toggles window visibility (show/hide).

---

## 7. Window behaviour implementation (Tauri 2 specifics)

`tauri.conf.json` window defaults:
```jsonc
{
  "width": 260, "height": 180,
  "decorations": false,        // borderless
  "transparent": true,         // per-pixel alpha for rounded translucent card
  "alwaysOnTop": true,
  "resizable": true,
  "skipTaskbar": false,        // keep a taskbar presence (req. 6)
  "shadow": false
}
```
- **Borderless:** `decorations: false`.
- **Always-on-top:** config flag + `WebviewWindow.setAlwaysOnTop(true)`.
- **Opacity:** primarily via CSS `rgba`/`opacity` on the card (cheap, smooth); a window-level
  opacity command is the fallback. Driven by the context-menu slider.
- **Drag-to-move:** on `mousedown` over the body call `appWindow.startDragging()`.
- **Resize:** `resizable: true` for native edges; expose a grip if needed.
- **Taskbar toggle (req. 6):** this is the subtle one. A borderless window can still show in the
  taskbar with `skipTaskbar: false`. To also toggle from the icon: register a **tray icon**
  (`tauri-plugin` tray) whose click handler does `if visible { hide() } else { show(); setFocus() }`.
  On Windows, clicking the taskbar button of a visible app minimizes it; we intercept/treat
  hide+show via the tray and/or a global show/hide command, ensuring the icon reliably toggles
  visibility while the window stays borderless and on-top when shown.
- **Click-through (optional, not required):** `setIgnoreCursorEvents(true)` available if a future
  "ghost mode" is wanted; off by default so controls work.

> WPF fallback equivalents: `WindowStyle=None`, `AllowsTransparency=True`,
> `Background=Transparent`, `Topmost=True`, `ResizeMode=CanResizeWithGrip`, `DragMove()` in
> `MouseLeftButtonDown`, `ShowInTaskbar=True`, and a `NotifyIcon` whose click toggles
> `Visibility`/`Show()/Hide()`.

---

## 8. Implementation roadmap (phased, each phase independently verifiable)

**Phase 0 — Scaffold & window shell**
- `pnpm create tauri-app` (vanilla-TS template). Pin versions. Configure `tauri.conf.json` per §7.
- Verify: borderless translucent always-on-top window appears, shows in taskbar, can be dragged
  and resized. No data yet (placeholder card).

**Phase 1 — Window controls**
- Right-click context menu; opacity slider wired to CSS opacity; size presets; tray icon with
  show/hide toggle; quit.
- Verify: opacity changes live; tray click hides/shows; quit works; drag doesn't fire on the
  slider.

**Phase 2 — Credential source**
- Rust `credential_source`: resolve path (`CLAUDE_CONFIG_DIR` → else `%USERPROFILE%\.claude`),
  read `.credentials.json`, parse token + `expiresAt` + optional `subscriptionType`.
- Verify: with a real logged-in Claude Code, the token is read; with the file absent, a clean
  `AuthExpired`/missing state is produced (no crash).

**Phase 3 — Usage client + live data (adaptive polling)**
- Rust `usage_client`: `GET /api/oauth/usage` with the four headers (UA `claude-code/<ver>`,
  fallback `claude-code/2.1.85`, 10s timeout), parse the usage object as a **dynamic map** of
  quota windows; classify 200/401/429/5xx/network.
- `poller`: implement the **adaptive scheme** of §5.3 (poll_interval/poll_fast/poll_fast_extra/
  poll_error/max_backoff/idle_pause), OS idle+lock detection, emit `UsageSnapshot` events.
- UI renders bars + countdowns dynamically; status dot reflects live/stale.
- Verify: real percentages appear and match Claude Code's `/usage`; cadence visibly speeds up
  during an active session and pauses when idle/locked; forcing a wrong UA produces `Stale` with
  capped backoff (no crash/spin).

**Phase 4 — Plan detection (profile endpoint) + per-plan layout**
- `usage_client` also calls `GET /api/oauth/profile` on a slow timer; `plan_detector` maps
  `has_claude_max`/`has_claude_pro`/`rate_limit_tier` → `Plan` (with credential/payload
  fallbacks per §2.3); show extra-usage block per `has_extra_usage_enabled`; plan-override menu
  item for unrecognized Max tiers; show account/display-name.
- Verify: correct plan badge + correct dynamic window set for the developer's actual plan;
  override switches the label.

**Phase 5 — Fallback (local JSONL) + resilience**
- `fallback_logs`: aggregate `projects/**/*.jsonl` token usage into rolling 5h/7d buckets; UI
  `Degraded` state with "local estimate" tag.
- Backoff/escalation on 429; auth-expired retry loop.
- Verify: kill network → widget switches to degraded estimate; restore → returns to live.

**Phase 6 — Polish & package**
- Visual polish, default sizes, README (including the ToS/risk note from §2.6 and a "this is
  unofficial / endpoint may change" disclaimer). Build a Windows installer (MSI/NSIS via Tauri
  bundler).
- Verify: clean install on a fresh Win11 user runs, reads usage, toggles from taskbar/tray.

---

## 9. Project structure

```
claude-overlay/
├─ .shared/plans/overlay-plan.md        ← this document
├─ src/                                  ← WebView UI
│  ├─ index.html
│  ├─ main.ts                            ← bootstrap, event subscription, render loop
│  ├─ store.ts                           ← last snapshot + status
│  ├─ components/
│  │  ├─ usage-card.ts
│  │  ├─ window-bar.ts                   ← single progress bar + countdown
│  │  └─ context-menu.ts
│  ├─ countdown.ts                       ← 1s local ticker off resets_at
│  └─ styles.css
├─ src-tauri/
│  ├─ tauri.conf.json
│  ├─ Cargo.toml
│  └─ src/
│     ├─ main.rs                         ← app setup, tray, command registration
│     ├─ config.rs                       ← interval constants (§5.3), UA string, endpoint consts
│     ├─ credential_source.rs
│     ├─ usage_client.rs                  ← GET /api/oauth/usage + /api/oauth/profile
│     ├─ fallback_logs.rs
│     ├─ plan_detector.rs
│     ├─ poller.rs
│     ├─ window_ctl.rs                   ← opacity/size/show-hide commands
│     └─ model.rs                        ← RawUsage + UsageSnapshot types
├─ icons/
├─ package.json
└─ README.md                             ← incl. unofficial-source + ToS disclaimer
```

---

## 10. Risks, unknowns, open questions

| Risk / unknown | Impact | De-risk |
|---|---|---|
| `/api/oauth/usage` is undocumented; shape/headers/host may change | Breaks live data | Isolate in `usage_client`; one-module swap; fallback to logs; pin & surface UA/beta as config consts. |
| Aggressive 429 rate-limiting (issue #31637) | "Real-time" is limited | 180s poll + correct UA + hard backoff + stale UI state. Never sub-minute. |
| Max 5x vs 20x not distinguishable from utilization % | Wrong tier label | Use `subscriptionType` if present; else label "Max" + manual override. |
| Token rotated/expired by Claude Code mid-session | Auth failures | Re-read token every poll; `AuthExpired` state; never self-refresh in v1. |
| macOS uses Keychain not a flat file | Port effort | Already isolated in `credential_source`; flagged for the port, not v1. |
| ToS / acceptability of reading another app's creds + undocumented endpoint | Reputational/policy | Read-only, non-inference, mirrors user's own data; explicit README disclaimer; designed to drop in an official API if one appears. |
| Borderless + taskbar-toggle interaction on Windows | Req. 6 correctness | Prototype tray-toggle in Phase 1 before building data; `skipTaskbar:false` + tray handler. |
| Requires Rust toolchain to build | DX friction | One-time setup; documented in README; WPF runner-up if blocking. |

**Open questions — all RESOLVED with the user (2026-06-16):**
1. ~~Acceptable to read `~/.claude/.credentials.json` read-only?~~ → **Yes, approved.**
2. ~~Is 180s "real-time enough"?~~ → **Needs to feel faster** → adopted the reference project's
   **adaptive polling** (§5.3): fast cadence during active usage, rapid follow-up checks, 1s local
   countdowns, idle/lock pause. Authoritative live percentages still can't safely go sub-minute on
   the endpoint, but adaptive polling delivers the "live during work" experience requested.
3. ~~Confirm primary stack = Tauri 2?~~ → **Confirmed: Tauri 2.** (WPF remains the documented
   fallback; architecture is stack-neutral.)

No blocking questions remain — the plan is ready to implement on approval.

---

## 11. Out of scope (explicit)
- **Persistence / saved settings** (opacity, size, position survive restart) — future extension.
- **macOS / Linux ports** — architecture-ready (`credential_source` is the only OS branch for
  macOS Keychain), but not built now.
- **Self-refreshing the OAuth token** — risky vs Claude Code's own state; deferred.
- **Historical charts / cost analytics** — this app is a live readout only.
- **Multi-account support.**

---

## Sources
- [jens-duttke/usage-monitor-for-claude](https://github.com/jens-duttke/usage-monitor-for-claude) — reference implementation; source of the exact `/api/oauth/usage` + `/api/oauth/profile` endpoints, header dict, UA fallback, and adaptive-polling interval defaults (`docs/api-reference.md`, `docs/configuration.md`, `usage_monitor_for_claude/api.py`).
- [Claude AI Pricing 2026 guide](https://www.glbgpt.com/hub/claude-ai-pricing-2026-the-ultimate-guide-to-plans-api-costs-and-limits/)
- [Claude pricing (May 2026)](https://mem0.ai/blog/anthropic-claude-pricing)
- [Claude Max plan limits](https://intuitionlabs.ai/articles/claude-max-plan-pricing-usage-limits)
- [Claude Code authentication docs (credential paths)](https://code.claude.com/docs/en/authentication)
- [phuryn/claude-usage (local JSONL transcript reader)](https://github.com/phuryn/claude-usage)
- [claude-code issue #31637 — /api/oauth/usage rate limiting](https://github.com/anthropics/claude-code/issues/31637)
- [Claude-Code-Usage-Monitor issue #202 — OAuth usage API window state + 180s polling](https://github.com/Maciek-roboblog/Claude-Code-Usage-Monitor/issues/202)
- [Claude Code rate limits explained (2026)](https://www.truefoundry.com/blog/claude-code-limits-explained)
- [Claude Code usage limits 2026 (5-hour + weekly)](https://www.morphllm.com/claude-code-usage-limits)
