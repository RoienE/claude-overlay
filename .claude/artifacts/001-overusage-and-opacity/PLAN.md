# PLAN: Overusage statistics section + configurable (persisted) opacity
_id: 001-overusage-and-opacity_
_status: ready_
_last-updated: 2026-06-16_

> Stack: Tauri 2 (Rust core in `src-tauri/src/`, vanilla-TS WebView UI in `src/`).
> No matching planner skill exists for Tauri/Rust-desktop in this repo's skill set;
> `context-architect`-style mapping was done manually by reading every source file.
> All conventions below are quoted from the actual code, not assumed.

---

## Requirement

Two independent changes to the always-on-top Claude usage overlay.

**Feature 1 — Overusage statistics section.** Add a dedicated, visually-consistent
section (alongside the existing dynamic quota bars for "5-hour session" and "Weekly")
surfacing four overusage fields:
- enabled/disabled (overusage toggle)
- current overusage
- allowed overusage
- available credits

Explicit ask: investigate the data source for the existing sections and source the
overusage fields from the same place; render consistently; flag as an open question if
the source does not expose the fields.

**Feature 2 — Configurable opacity (persisted).** The user previously asked for
configurable opacity; the live slider exists but the value is never saved and resets on
restart. Add a persisted opacity setting using whatever config pattern the app already
uses, applied to the overlay rendering on startup and on change.

Implied asks:
- F1: keep the existing dynamic, differential-DOM rendering approach; do not regress the
  current minimal footer line (we will replace it with the richer section).
- F1: handle null fields gracefully (the API returns nulls frequently — see below).
- F2: match the existing settings pattern. **There is currently NO settings/config
  persistence mechanism at all** (confirmed below), so part of this work is introducing
  the smallest viable one and routing opacity through it.
- F2: the context-menu slider must initialise to the saved value instead of the hardcoded
  92%.

---

## Existing context touched

### Data sourcing (how the two existing sections are populated) — fully traced

The 5-hour and weekly sections are **quota windows** fetched from an undocumented Anthropic
OAuth endpoint. The flow:

1. `src-tauri/src/config.rs:5` — `USAGE_URL = "https://api.anthropic.com/api/oauth/usage"`.
2. `src-tauri/src/usage_client.rs:59` `fetch_usage()` — GETs that URL with four headers
   (Authorization Bearer, Content-Type, User-Agent `claude-code/2.1.85`, `anthropic-beta`),
   parses the body as a generic `serde_json::Value` object, and iterates its keys
   (`usage_client.rs:103-142`). Each non-null key becomes a `QuotaWindow`.
3. **`extra_usage` is already parsed here** (`usage_client.rs:104-117`): when the key is
   `extra_usage`, it deserialises into `RawExtraUsage` and maps to
   `model::ExtraUsage { enabled, used_credits, monthly_limit, utilization }`.
4. `fetch_usage` returns `(Vec<QuotaWindow>, Option<ExtraUsage>)`.
5. `src-tauri/src/poller.rs:188-251` — the poll loop calls `fetch_usage`, and on success
   builds a `UsageSnapshot { windows, extra_usage, ... }` (`poller.rs:237-245`) and sends it
   over an mpsc channel.
6. `src-tauri/src/lib.rs:83-87` — a task forwards each snapshot to the WebView via
   `app_handle.emit("usage://snapshot", &snapshot)`.
7. `src/main.ts:36-50` — `listen<UsageSnapshot>('usage://snapshot', ...)` puts the payload
   into the store; the store subscription (`main.ts:31-33`) calls `renderSnapshot`.
8. `src/components/usage-card.ts:53` `renderSnapshot()` renders the quota bars dynamically
   (`usage-card.ts:109-150`) via `window-bar.ts`, then renders a **minimal overusage footer**
   (`usage-card.ts:168-183`).

**Conclusion: the data source already exposes overusage.** The four fields map as:

| User-facing field    | Source field (`ExtraUsage`)         | Notes |
|----------------------|-------------------------------------|-------|
| enabled/disabled     | `enabled` (`is_enabled`)            | bool, already surfaced |
| current overusage    | `used_credits`                      | dollars spent so far; per plan doc §2.1 line 114 = "current overusage". Often `null`. |
| allowed overusage    | `monthly_limit`                     | the overage ceiling. Often `null` (means "no explicit limit set"). |
| available credits    | **DROPPED per user decision (OQ1)** — not rendered | API has no literal field; user chose to omit rather than derive. |

`utilization` (0–100, often null) is the percentage of the overage budget consumed — useful
to render a progress bar consistent with the quota bars.

The canonical data model is `src-tauri/src/model.rs`:
- `RawExtraUsage` (lines 48-54): `is_enabled, monthly_limit, used_credits, utilization` — all
  `Option`. This is the exact wire shape; do NOT rename.
- `ExtraUsage` (lines 115-121): normalized `{ enabled, used_credits, monthly_limit, utilization }`.
- TS mirror in `src/store.ts:15-20` `interface ExtraUsage` — identical field names (serde uses
  snake_case which already matches the TS interface).

So **no Rust/serde change is required to obtain the four fields** — they already arrive in the
snapshot. Feature 1 is a **frontend-only rendering change** plus an optional derived field.

### Rendering conventions (must match)

- `src/components/window-bar.ts` — the canonical "section" visual: a `.quota-bar-wrap` with a
  `.quota-bar-header` (label + right-aligned `.quota-pct` + `.quota-countdown`) and a
  `.progress-track > .progress-fill`. `getFillClass()` (line 82) colours the bar by threshold.
- `src/styles.css:165-227` — `.quota-bar-*` and `.progress-*` classes. The footer currently uses
  `.card-footer` / `.extra-usage-line` (`styles.css:143-156`).
- `usage-card.ts` uses **differential DOM updates** keyed on `dataset.key` (lines 109-150) — new
  sections should follow the same idempotent build/update pattern, or live in the footer region
  which is re-rendered wholesale each snapshot (simpler; chosen below).

### Settings / config mechanism — CONFIRMED ABSENT

There is **no runtime-persisted settings system**. Evidence:
- `src-tauri/src/window_ctl.rs:20-30` `set_opacity` only `window.eval(...)` injects CSS opacity
  into the live WebView — nothing is written to disk.
- `src/components/context-menu.ts:38` the slider calls `invoke('set_opacity', ...)`; on every
  menu open the slider is reset to `currentOpacity` which defaults to the hardcoded `0.92`
  (`context-menu.ts:17`, `124`) and `styles.css:23` (`#app { opacity: 0.92 }`).
- `src-tauri/Cargo.toml:20-29` lists no `tauri-plugin-store` or any persistence crate.
- The original design doc **`.shared/plans/overlay-plan.md:617`** lists "Persistence / saved
  settings (opacity, size, position survive restart)" under **Out of scope (future extension)**,
  and `README.md:180` has an unchecked roadmap item "Persist opacity/size/position across
  restarts". This is the never-implemented request.
- `config.rs` holds only compile-time constants, not user-mutable settings.

There is, however, an established pattern for **locating a JSON file in the user profile**:
`credential_source.rs:44-74` (`credentials_path()`) and `fallback_logs.rs:20-40` (`claude_dir()`)
both resolve `CLAUDE_CONFIG_DIR` → `%USERPROFILE%` → `$HOME`. The new settings file must reuse
this idiom but write to the **app's own** directory, NOT `.claude` (never write into Claude
Code's dir — `overlay-plan.md:183` "Never write to .credentials.json. Read-only.").

### Things that could break
- `set_opacity` clamps to `0.1..=1.0` (`window_ctl.rs:22`); the slider min is 20% (`context-menu.ts:83`).
  Keep these consistent when persisting.
- `usage-card.ts` early-returns for `loading` / `auth_expired` / empty-windows states (lines
  79-107) and hides the footer in those branches — the new overusage section must respect those
  same early-returns or it will render stale overage data over an auth-expired card.

---

## Gaps & open questions

> **USER DECISIONS LOCKED (2026-06-16):**
> - **OQ1 → DROP "available credits".** Render only the three literal fields: enabled/disabled,
>   current overusage (`used_credits`), allowed overusage (`monthly_limit`). Do NOT derive or show
>   available credits.
> - **OQ3 → ALWAYS SHOW the section** when `extra_usage` is present, with an on/off pill (visible
>   even when overusage is disabled).
> - **OQ4 → HAND-ROLLED `settings.rs`** (JSON in app config dir, zero new dependencies). Do NOT add
>   `tauri-plugin-store`.
> - OQ2 (currency formatting) and OQ5 (file location) keep their planned defaults.

- [RESOLVED — DROP] **OQ1 — "available credits" semantics.** User chose to omit the field entirely.
  Show only enabled, current overusage, allowed overusage. No derivation, no "available" line.
- [NON-BLOCKING] **OQ2 — currency/units.** `used_credits` / `monthly_limit` are rendered today as
  `$x.xx` (`usage-card.ts:174`). Real account values are needed to confirm they are USD dollars
  vs credit units. We keep the existing `$` formatting for consistency. Flagging because the field
  is named "credits".
- [NON-BLOCKING] **OQ3 — show the section when overusage is DISABLED?** Today the footer is hidden
  entirely unless `extra_usage.enabled` (`usage-card.ts:169`). The new requirement explicitly lists
  "enabled/disabled" as a field to surface, implying the section should be visible even when
  disabled (showing "Overusage: off"). **Default assumed: always render the section when
  `extra_usage` is present in the snapshot, showing on/off state.** Confirm if you'd rather keep it
  hidden when off.
- [NON-BLOCKING] **OQ4 — opacity persistence backend choice.** No persistence exists. Two options:
  (a) add the official `tauri-plugin-store` crate (one new dependency, idiomatic), or
  (b) hand-roll a tiny JSON settings file in the app config dir reusing the existing
  path-resolution idiom (zero new dependencies, matches the repo's "as lightweight as possible /
  not over-engineered" ethos from `overlay-plan.md:41`). **Default chosen: (b) hand-rolled
  `settings.rs`**, because the repo already hand-rolls all its file IO and avoids extra crates, and
  because a Tauri-store plugin also requires a JS-side dependency + capability permission wiring.
  Confirm if you'd prefer the plugin.
- [NON-BLOCKING] **OQ5 — settings file location.** Proposed:
  `%APPDATA%\com.claude-overlay.app\settings.json` via Tauri's `app.path().app_config_dir()`
  (preferred, OS-correct, uses the bundle identifier `com.claude-overlay.app` from
  `tauri.conf.json:5`). Fallback if path API is awkward: `%USERPROFILE%\.claude-overlay\settings.json`.
  **Default: use Tauri `app_config_dir()`.**

No BLOCKING gaps. All defaults above are safe and reversible; proceed unless the user objects.

---

## Plan

### Feature 1 — Overusage statistics section (frontend-only)

1. **Decide the field math** in one helper. — files: `src/components/usage-card.ts`
   (or a new `src/components/overusage-section.ts`), skill: none.
   Add a pure helper `deriveOverusage(eu: ExtraUsage)` returning a view-model:
   `{ enabled, currentOverusage: number|null, allowedOverusage: number|null,
   utilization: number|null }`.
   **Per OQ1 (LOCKED): do NOT compute or include `availableCredits` — that field is dropped.**
   Keep it exported and pure so it is unit-testable (Vitest, per `testing-qa`).

2. **Render the section.** — files: `src/components/usage-card.ts`, `src/styles.css`.
   Replace the current footer block (`usage-card.ts:168-183`) with a richer overusage section.
   Render it **in the footer region** (`#card-footer`) so it sits visually below the quota bars,
   matching the original design (`overlay-plan.md:442` "Footer (when extra_usage.enabled)").
   Per OQ3 default, render whenever `snap.extra_usage` is present (not only when enabled):
   - A header row styled like `.quota-bar-header`: label "Overusage" + a small on/off pill.
   - When enabled and `utilization` is non-null, a `.progress-track > .progress-fill` bar reusing
     `getFillClass(utilization)` for threshold colouring — visually identical to the quota bars.
   - A compact stats grid/lines for the THREE values only (OQ1 locked — no available credits):
     `current $x.xx · allowed $y.yy` with "—" for any null.
   - Respect the early-return states: keep the footer hidden in `loading` / `auth_expired` /
     no-windows branches exactly as today (do not render overage over those states).

3. **Add CSS.** — files: `src/styles.css`.
   Add `.overusage-section`, `.overusage-pill.on/.off`, and reuse existing `.progress-track` /
   `.progress-fill` / `.quota-bar-header` rules. Match the dark translucent palette and the
   10–12px type scale already in use (`styles.css:150-209`). Do not introduce new colours outside
   the existing tokens (#ff9800 amber for "on", muted white for "off").

4. **Type-check + lint.** `pnpm exec tsc --noEmit` then `pnpm build` (README:161-164). No Rust
   rebuild needed for F1.

> Note: NO Rust changes are required for Feature 1 — the snapshot already carries `extra_usage`
> with all needed fields. If during implementation the live API is found to omit `used_credits`/
> `monthly_limit` for this account (they are frequently null per `overlay-plan.md:96-98`), that is
> expected: render "—". Do not invent values. (This realises OQ1's graceful path.)

### Feature 2 — Configurable, persisted opacity

1. **Introduce a settings module (Rust).** — files: NEW `src-tauri/src/settings.rs`,
   modify `src-tauri/src/lib.rs`, modify `src-tauri/src/config.rs`. skill: none.
   - Define `#[derive(Serialize, Deserialize, Clone)] struct Settings { opacity: f32 }` with a
     `Default` of `opacity: 0.92` (matching `styles.css:23` / `context-menu.ts:17`). Use
     `#[serde(default)]` on fields so forward-compatible additions (size/position later) don't
     break parsing.
   - `settings_path(app: &AppHandle) -> PathBuf`: use `app.path().app_config_dir()` (Tauri 2 path
     API) joined with `settings.json`; create the dir if missing. Mirror the resilient
     resolution style of `credential_source.rs` but for our own dir (OQ5).
   - `load(app) -> Settings`: read+parse the file; on any error return `Settings::default()` (never
     panic — the app must run on first launch with no file).
   - `save(app, &Settings) -> Result<(), String>`: serialize pretty JSON, write atomically
     (write temp + rename, or simple write — file is tiny/low-risk).
   - Add `#[cfg(test)]` round-trip + default-on-missing tests (per `testing-qa` xUnit-equivalent →
     here Rust `cargo test`).
   - Add an `OPACITY_MIN: f32 = 0.2` / `OPACITY_MAX: f32 = 1.0` constant pair to `config.rs` and
     use it for clamping in both the settings load and `set_opacity` so the slider (20–100%) and
     backend agree.

2. **Persist on change + add a getter command.** — files: `src-tauri/src/window_ctl.rs`,
   `src-tauri/src/lib.rs`.
   - Change `set_opacity` (`window_ctl.rs:21`) to ALSO persist: after applying via `eval`, call
     `settings::load`, set `opacity`, `settings::save`. Clamp with the new `OPACITY_MIN/MAX`.
     (Keep the `eval` apply for instant feedback; the JS side also applies directly per
     `context-menu.ts:35-37`, so persistence is the only behavioural addition.)
   - Add a new command `get_settings(app) -> Result<Settings, String>` returning the loaded
     settings, and register it in the `invoke_handler!` list (`lib.rs:91-99`).

3. **Apply saved opacity at startup.** — files: `src-tauri/src/lib.rs` and/or `src/main.ts`.
   Preferred (no flash, single source of truth): in `lib.rs` `setup()` (around lines 30-89), after
   the window exists, load settings and apply opacity to `#app` via `window.eval(...)` once at
   boot — OR emit it to the frontend. Simpler and matching the existing event idiom: have the
   **frontend fetch settings on boot**:
   - `src/main.ts`: after building the card, `invoke('get_settings')` and apply
     `appEl.style.opacity = String(settings.opacity)` (replaces reliance on the CSS default).
   - This also feeds the context menu's initial slider value (next step).

4. **Initialise the context-menu slider from the saved value.** — files: `src/main.ts`,
   `src/components/context-menu.ts`.
   - `context-menu.ts` currently hardcodes `currentOpacity: 0.92` (line 17) and the slider HTML
     `value="92"` (line 83). Add an exported `setCurrentOpacity(v: number)` (parallel to the
     existing `setOpacityCallback`, `context-menu.ts:176`) and call it from `main.ts` with the
     loaded settings value so the slider reflects the persisted opacity when the menu opens
     (`show()` already syncs the slider to `currentOptions.currentOpacity`, lines 122-125).
   - No TS `store.ts` change needed (settings are separate from the usage snapshot).

5. **Add a `Settings` TS type** (optional, small). — files: `src/store.ts` or inline in `main.ts`.
   `interface Settings { opacity: number }` to type the `invoke<Settings>('get_settings')` call.

6. **Build/verify.** `cd src-tauri && cargo test` for the Rust settings tests; `pnpm exec tsc
   --noEmit` + `pnpm build` for the frontend; manual `pnpm tauri:dev`: change opacity via slider,
   quit, relaunch → opacity persists.

7. **Docs.** Tick `README.md:180` partial ("Persist opacity ... across restarts") — at minimum
   note opacity is now persisted. (Size/position remain future work.)

---

## Tests required

Per `testing-qa` conventions (Rust `cargo test`; frontend Vitest — note: Vitest is not yet wired
in this repo, `countdown.ts:86` says "To add tests: pnpm add -D vitest". Adding it is optional;
if not added, cover F1 logic via a pure exported helper and rely on `tsc`/manual verification):

- **Rust (`settings.rs`):** default-when-file-missing returns `opacity == 0.92`; save→load round-trip
  preserves the value; out-of-range opacity is clamped to `[0.2, 1.0]`.
- **Rust (`window_ctl.rs`):** `set_opacity` clamps and persists (can be a thin logic test on the
  clamp helper extracted to `config.rs`/`settings.rs`).
- **Frontend (pure helper):** `deriveOverusage` — available = limit−used when both present; null
  when either missing; enabled flag passthrough. (Unit-testable even without a test runner by
  keeping it pure; add a Vitest spec if the runner is introduced.)
- **Manual / integration:** overusage section renders with on/off pill and "—" for null fields;
  does not appear over `auth_expired`/`loading`; opacity survives an app restart and the slider
  opens at the saved value.

---

## Risks & rollback

- **F1 is low-risk, additive, frontend-only.** Rollback = revert `usage-card.ts` + `styles.css`.
  Worst case the section shows "—" for all values when the API returns nulls — acceptable and
  honest (matches existing behaviour where the footer just hid).
- **F2 introduces the first disk-write the app performs.** Risks: writing to the wrong dir, or a
  read/parse error blocking startup. Mitigations: write ONLY to the app's own config dir (never
  `.claude`); `load()` must be infallible (return `Default` on any error); `save()` failure must be
  non-fatal (log + ignore, opacity still applied live via `eval`). Rollback = revert `settings.rs`,
  the `lib.rs` registration, the `window_ctl.rs` persistence lines, and the `main.ts`/`context-menu.ts`
  boot fetch; behaviour returns to live-only opacity.
- **Compatibility:** `Settings` uses `#[serde(default)]` so older/newer files parse; deleting
  `settings.json` resets to defaults cleanly.
- **No security surface added** beyond a local user-readable JSON containing only an opacity float
  (no secrets) — `auth-security` review not required, but note the file must not be world-writable
  in a way that matters; it lives in the per-user app config dir.

---

## Work units (for parallel developer subagents)

Both units are runnable **in parallel** — they touch disjoint files except for two small, clearly
partitioned edits in `usage-card.ts`/`main.ts` (see Parallel schedule for the de-conflict rule).

- [x] **Unit A — Overusage statistics section** — status: done — frontend —
  agent: developer-frontend — feature: F1.
  - files (disjoint): `src/components/usage-card.ts` (footer/overusage render region only,
    lines ~168-183 + helpers), optional NEW `src/components/overusage-section.ts`,
    `src/styles.css` (overusage classes only).
  - depends on: none (data already in snapshot; contract C below is already satisfied by existing code).
  - skill: none (no Tauri/Rust skill matches; follow existing TS patterns in `window-bar.ts`).

- [ ] **Unit B — Persisted configurable opacity** — status: pending — backend (+ thin frontend) —
  agent: developer-backend — feature: F2.
  - files (disjoint): NEW `src-tauri/src/settings.rs`, `src-tauri/src/config.rs` (add OPACITY_MIN/MAX),
    `src-tauri/src/window_ctl.rs` (`set_opacity` persist + `get_settings`), `src-tauri/src/lib.rs`
    (module decl + command registration + optional boot apply), `src/components/context-menu.ts`
    (add `setCurrentOpacity`, slider init), `src/main.ts` (boot `invoke('get_settings')` + apply),
    `src/store.ts` (optional `Settings` type), `README.md` (roadmap tick).
  - depends on: none.
  - skill: none.

## Contracts (seams between units)

- **C1 (already implemented, no change):** `UsageSnapshot.extra_usage: ExtraUsage | null` where
  `ExtraUsage = { enabled: boolean; used_credits: number|null; monthly_limit: number|null;
  utilization: number|null }` — Rust `model.rs:115-121` ↔ TS `store.ts:15-20`. Unit A consumes
  this as-is; no backend change.
- **C2 (new, owned by Unit B):** Tauri command `get_settings() -> { opacity: number }` and command
  `set_opacity(opacity: number)` (existing signature, now also persists). Unit B produces both;
  only Unit B's frontend files consume them.

## Parallel schedule

- Unit A and Unit B start **concurrently**; neither blocks the other.
- **De-conflict rule for the two shared files** (`src/main.ts`, `src/components/usage-card.ts`):
  - `usage-card.ts` is edited **only by Unit A** (overusage render). Unit B must NOT touch it.
  - `main.ts` is edited **only by Unit B** (boot settings fetch + slider init). Unit A must NOT
    touch it — Unit A's render is driven entirely by the existing store subscription, so it needs
    no `main.ts` change.
  This keeps every file owned by exactly one unit. If Unit A discovers it needs a `main.ts` change,
  it must coordinate/sequence rather than edit in parallel.

## Suggested follow-ups (out of scope here)
- Persist size + window position too (the settings module is built to extend — add fields with
  `#[serde(default)]`). Completes `README.md:180`.
- Wire up Vitest (`countdown.ts:86`) so `deriveOverusage` and `formatCountdown` get real specs.
