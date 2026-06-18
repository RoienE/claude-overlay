# PLAN: Quota-bar key exclusion ("Limit"/"Spend") + decouple overusage data from is_enabled
_id: 003-quota-bar-exclusion-and-overusage-data_
_status: planning_
_last-updated: 2026-06-17_

> Stack: Tauri 2 (Rust core in `src-tauri/src/`, vanilla-TS WebView UI in `src/`), communicating
> via the `usage://snapshot` Tauri event. No planner skill matches Tauri/Rust-desktop in this
> repo's skill set; context was mapped manually by reading every cited source file. All facts
> below are quoted from the actual code (file:line), not assumed. Follow-up to shipped runs
> `001-overusage-and-opacity` and `002-overusage-refinements-and-context-menu`.
>
> **This plan is PHASED and partially BLOCKED on a live diagnostic payload.** Phase 1 ships a
> temporary diagnostic logger; the user runs the app once and reports (a) the raw top-level keys
> of `/api/oauth/usage` and (b) the live `extra_usage` object when the overusage cap is met.
> Phase 2's two fixes are finalised against that captured data and the Phase-1 logging is removed.

---

## Requirement (restated)

Two confirmed problems, both gated on first capturing the live raw payload.

### Problem A — remove the "Limit" and "Spend" quota bars
The overlay renders a quota bar for **every** top-level key of the `/api/oauth/usage` JSON object
except `extra_usage` (`usage_client.rs:103-142` iterates `obj`, builds a `QuotaWindow` per
non-null key; `plan_detector.rs:114-137` `label_for_key` humanises unknown keys via title-case).
The API now returns two extra windows that humanise to **"Limit"** and **"Spend"**, both
perpetually 0% for this user, separate from overusage (the `extra_usage` footer shipped in run
002) and redundant.

- **Explicit ask:** remove these two **specifically** via a targeted **raw-key exclusion** — NOT a
  blanket "hide any 0% window" (that would wrongly hide a legitimately-empty `five_hour` window at
  the start of a period). The exclusion is keyed on the **raw API keys**.
- **Implied asks:** keep it trivial to add/remove keys later (Anthropic may add more); single,
  justified exclusion site; do not disturb existing window sorting/labelling for legit keys.

### Problem B — overusage shows "OFF" with no data even when the cap is reached
`usage_client.rs:109` sets `enabled: raw_extra.is_enabled.unwrap_or(false)`. The UI
(`overusage-section.ts`, shipped run 002) ties the **whole** section to `enabled`: when
`enabled === false` it muts the section (`disabled` class, `overusage-section.ts:119`), zeroes the
progress bar (`:113-115` `fill = vm.enabled ? computeFill(...) : 0`), and shows an "off" pill
(`:104-105`) — even though `used_credits` / `monthly_limit` may be present. Real overage data is
hidden behind an "off" state whenever `is_enabled` is false/null, including (user reports) when the
limit has been MET.

- **Explicit ask:** **decouple displaying the numbers from `is_enabled`.** Show
  `used_credits`/`monthly_limit` (formatted via the shipped `formatCredits`, ÷100, `€`) and the
  progress bar **whenever the values are present**, regardless of `is_enabled`. Rethink what the
  on/off indicator reflects (options a/b/c below) and recommend one.
- **Implied asks:** prefer a frontend-only fix if the needed data already reaches the UI
  (it does — see below); flag any required Rust/serde change explicitly; do NOT rename existing
  serde fields; keep the differential-DOM path and `—` null handling; cents→units ÷100 + `€`
  stays frontend-only (already shipped, do not duplicate in Rust).

### Cross-cutting constraints
- Phase-1 diagnostic must log **only the usage JSON body** (which contains no secrets) — never any
  token/credential, never read or write `.credentials.json` beyond existing read-only access.
- Phase-1 diagnostic must be explicitly marked for removal and removed in Phase 2.
- Verification: `pnpm exec tsc --noEmit` + `pnpm build` (frontend); `cd src-tauri && cargo test`
  + `cargo build` (backend). Existing tests stay green.
- Minimal, reversible changes. Clean frontend/backend partition with disjoint file ownership.

---

## Architecture recap (with file:line citations)

### Usage fetch + window construction (Rust)
- `src-tauri/src/config.rs:5` — `USAGE_URL = "https://api.anthropic.com/api/oauth/usage"`.
- `src-tauri/src/usage_client.rs:59` `fetch_usage()` GETs the URL (`:65`), classifies HTTP status
  (`:71-81`), then reads the body as a `String` (`:83-86`) and parses it as a generic
  `serde_json::Value` (`:90-93`). The body string is **already in hand** at `:83` — the natural,
  no-extra-network place to add Phase-1 diagnostic logging.
- `:95-98` casts to a JSON object; `:103-142` iterates `(key, value)`:
  - `key == "extra_usage"` (`:104-117`) → deserialise `RawExtraUsage`, map to
    `model::ExtraUsage { enabled: raw.is_enabled.unwrap_or(false), used_credits, monthly_limit,
    utilization }` (`:108-113`), then `continue`.
  - null values skipped (`:120-122`).
  - every other key → push `QuotaWindow { label: label_for_key(key), key, utilization, resets_at }`
    (`:136-141`). **This is where "Limit"/"Spend" windows enter.**
- `:145` `crate::plan_detector::sort_windows(&mut windows)` orders them; `:147` returns
  `(windows, extra_usage)`.

### Labelling / sorting (Rust)
- `plan_detector.rs:114-137` `label_for_key` — known keys mapped; unknowns title-cased
  (`:122-135`). A raw key `"limit"` → `"Limit"`, `"spend"` → `"Spend"` (one-word title-case);
  multi-word raw keys (e.g. `"overage_limit"`) → `"Overage Limit"`. **Exact raw keys unknown —
  see BLOCKING gap.**
- `plan_detector.rs:141-156` `sort_windows` — priority list of known keys; unknowns appended
  (`:150-155`). Adding a filter here is one option for Problem A.
- `plan_detector.rs:158-260` has `#[cfg(test)]` covering `label_for_key`, `sort_windows`, etc. —
  the natural home for a new exclusion-filter test.

### Model / contract (Rust ↔ TS) — do NOT rename serde fields
- `model.rs:48-54` `RawExtraUsage { is_enabled, monthly_limit, used_credits, utilization }` (all
  `Option`) — exact wire shape.
- `model.rs:115-121` `ExtraUsage { enabled: bool, used_credits: Option<f64>, monthly_limit:
  Option<f64>, utilization: Option<f32> }` — normalised; emitted to UI.
- `model.rs:124-133` `Profile { …, has_extra_usage_enabled: bool (:131), … }` — parsed at
  `usage_client.rs:191` from `org.has_extra_usage_enabled.unwrap_or(false)`. **This is the
  more-authoritative on/off signal mentioned in the requirement.**
- `model.rs:154-165` `UsageSnapshot { plan, profile: Option<Profile> (:157), windows, extra_usage:
  Option<ExtraUsage> (:160), status, fetched_at, next_poll_in }`.
- TS mirror `src/store.ts`: `ExtraUsage` (`:15-20`), `Profile` (`:22-30`, incl.
  `has_extra_usage_enabled: boolean` `:28`), `UsageSnapshot` (`:40-48`, incl. `profile: Profile |
  null` `:42`). **`UsageSnapshot.profile` already reaches the frontend** — so
  `profile.has_extra_usage_enabled` is available to the renderer with NO Rust change.

### Snapshot construction (Rust)
- `poller.rs:206` matches `ApiResult::Ok((windows, extra_usage))`; `:256` `profile:
  cached_profile.clone()` and `:258` `extra_usage` are both placed in the snapshot. So the
  frontend receives both `profile` and `extra_usage` together.

### Event seam + render (Rust → TS)
- `lib.rs:104` `app_handle.emit("usage://snapshot", &snapshot)`. `env_logger::init()` at
  `lib.rs:24` — the `log` crate is live; `window_ctl.rs:38` already uses `log::warn!`. So
  `log::info!`/`log::warn!` from `usage_client.rs` will surface through the same logger
  (controlled by `RUST_LOG`).
- `usage-card.ts:179-184` `renderSnapshot()` footer block: `if (snap.extra_usage) {
  renderOverusageSection(footer, snap.extra_usage); footer.style.display = 'block'; } else hide`.
  **Note: the renderer is currently called with only `snap.extra_usage`** — to drive the pill from
  `profile.has_extra_usage_enabled` (option a) the call must also pass `snap.profile`.
- `overusage-section.ts` (run 002): `formatCredits` (`:49-52`) `value/100` + `€` (DONE — do not
  duplicate). `getFillClass` (`:54-59`). `computeFill` (`:72-84`) ratio→utilization→0.
  `deriveOverusage` (`:33-40`). `renderOverusageSection` (`:97-138`): builds pill from
  `vm.enabled` (`:104-105`), spent/allowed strings (`:108-109`), `fill = vm.enabled ?
  computeFill(...) : 0` (`:113-115`) ← **the bug: data zeroed when disabled**, `sectionClass`
  adds `disabled` when `!enabled` (`:119`), writes `footer.innerHTML` (`:121-137`).
- CSS `styles.css:166-241` overusage block: `.overusage-pill.on/.off` (`:187-197`),
  `.overusage-section.disabled { opacity:.6 }` (`:239-241`), values/sep/progress rules. Reusable
  as-is; minor additions only.

### Logging facts (for Phase 1)
- `log` crate in use via `env_logger` (`lib.rs:24`); `log::warn!` precedent (`window_ctl.rs:38`),
  `info!` precedent (`poller.rs:182`). Default `env_logger` is OFF unless `RUST_LOG` is set, so a
  diagnostic must be at a level the user can capture — recommend `log::warn!` (visible at the
  common `RUST_LOG=warn` and above) AND, to make it trivially findable/copyable regardless of
  `RUST_LOG`, **also write the body to a dedicated file** in the app config dir (reuse the
  `settings.rs` `app_config_dir()` idiom already established in run 001 — see
  `settings.rs`). Both are removed in Phase 2.

---

## Gaps & open questions

- [BLOCKING — on Phase-1 payload] **OQ-A1 — the exact raw API keys for "Limit"/"Spend".** No
  sample payload contains them (`.shared/plans/overlay-plan.md:85-100` predates them). Title-case
  inference suggests single-word `"limit"`/`"spend"`, but they could be compound
  (e.g. `"limit_window"`, `"spend_cap"`). **The exclusion blocklist cannot be finalised until the
  user reports the live top-level keys.** Phase 1 captures them; Phase 2 hardcodes the confirmed
  literals. Until then the blocklist constant is a TODO placeholder.
- [BLOCKING — on Phase-1 payload] **OQ-B1 — which `extra_usage` fields are populated when the cap
  is MET.** The decoupling DESIGN is decidable now (below), but the final logic — e.g. whether to
  add a "limit reached" state (option c) and what `is_enabled`/`utilization` look like at the cap —
  depends on the live `extra_usage` object captured in that state. Phase 1 captures it; Phase 2
  finalises the indicator logic. Frontend work that does NOT depend on this (passing `profile` to
  the renderer, removing the `vm.enabled ? … : 0` gate so data always shows) can be specified and
  built now; only the precise pill/limit-reached semantics wait.
- [NON-BLOCKING] **OQ-A2 — exclusion site.** Evaluated below; recommendation: a constant blocklist
  in `plan_detector.rs` applied at parse time in `usage_client.rs` (skip excluded keys when
  building `windows`). Justified in Problem A design.
- [NON-BLOCKING] **OQ-B2 — pill source / "limit reached" state.** Options (a)/(b)/(c) evaluated
  below; recommendation = **(b) + (c)**: always render data + bar (data fully decoupled from
  `is_enabled`); the pill reflects `profile.has_extra_usage_enabled` (more authoritative than
  `extra_usage.is_enabled`) — i.e. fold (a) into the pill source — and a distinct "limit reached"
  treatment appears when `used >= limit`. Final wording/visual confirmed against OQ-B1 data.
- [NON-BLOCKING] **OQ-A3 — could "Limit"/"Spend" ever be legit windows for other accounts?** The
  exclusion is a static blocklist; if Anthropic repurposes those keys it would wrongly hide them.
  Accepted per the user's explicit "remove these two specifically" decision; mitigated by keeping
  the blocklist a one-line-editable constant and documenting it.

**Phase-2 fix code MUST NOT be finalised/merged until OQ-A1 and OQ-B1 are resolved by the
captured payload.** Phase 1 is the unblocker and ships independently.

---

## Phase 1 — diagnostic capture (temporary, removed in Phase 2)

**Goal:** capture, from a single real run, (1) all top-level keys of the `/api/oauth/usage`
response and the full body, and (2) the live `extra_usage` object — ideally in the cap-met state.

**Backend-only.** Single file touched: `src-tauri/src/usage_client.rs`.

- In `fetch_usage`, immediately after the body string is available (`usage_client.rs:83-86`, before
  the `serde_json::from_str` at `:90`), add a **clearly-marked temporary diagnostic block**:
  - `log::warn!("[DIAG-003 REMOVE-IN-PHASE-2] /api/oauth/usage raw body: {body}");` — logs the full
    JSON body. The body contains usage windows + `extra_usage` only; **it carries no token** (the
    token lives in the `Authorization` header, never echoed back) — safe to log per the constraint.
  - Additionally, after the object is parsed (`:95-98`), log just the keys for an easy scan:
    `log::warn!("[DIAG-003 REMOVE-IN-PHASE-2] usage top-level keys: {:?}", obj.keys().collect::<Vec<_>>());`
  - **And** write the raw body once to a findable file so the user need not configure `RUST_LOG`:
    e.g. `diag-usage.json` in the app config dir, reusing `settings.rs`'s `app_config_dir()`
    resolution (best-effort; ignore write errors, never panic). Mark it `// TODO(DIAG-003): remove
    in Phase 2` around the whole block. Do NOT log headers, the token, or anything from
    `credential_source`.
- Wrap the whole block in unmistakable `// ── DIAG-003 START (REMOVE IN PHASE 2) ──` /
  `// ── DIAG-003 END ──` comment fences so removal is a single delete.
- **No model/contract/UI change in Phase 1.** Existing tests untouched; `cargo build` + `cargo
  test` must stay green.

**User action between phases:** run the app once with overusage near/at the cap (set
`RUST_LOG=warn` if reading console, or just open `diag-usage.json`), copy the body, and report:
the top-level key list, and the `extra_usage` object values in the cap-met state.

---

## Phase 2 — the two fixes (finalised against the captured payload)

### Problem A — targeted raw-key exclusion (BACKEND)

**Chosen site (OQ-A2): a constant blocklist in `plan_detector.rs`, applied at PARSE TIME in
`usage_client.rs` when building `windows`.** Justification vs. alternatives:
- **Parse-time skip in `usage_client.rs` (chosen):** excluded keys never become `QuotaWindow`s, so
  they never reach `sort_windows`, the snapshot, or the UI — the cleanest "they don't exist"
  semantics, smallest blast radius, and no risk of a stray excluded window leaking through a code
  path that bypasses the filter. The check is a single `if is_excluded_window_key(key) { continue;
  }` next to the existing `extra_usage`/null `continue`s (`usage_client.rs:104-122`).
- **Filter inside `sort_windows` (rejected):** `sort_windows` is a pure ordering helper with tests
  asserting it only reorders; adding removal there overloads its contract and risks surprising any
  future caller. Also runs after the windows are already built.
- **Filter in `label_for_key` (rejected):** labelling cannot remove an item; wrong layer.
- **Config-constant location:** put the blocklist in `plan_detector.rs` (next to `label_for_key`
  and the `sort_windows` priority list, where window-key knowledge already lives) rather than
  `config.rs` (which holds only URLs/timeouts/sizes). Keeps all window-key policy in one module.

**Implementation:**
- `plan_detector.rs`: add a documented constant and a tiny predicate, e.g.
  ```
  /// Raw /api/oauth/usage keys that are NOT real quota windows and must not render
  /// as bars (perpetually-0% noise, separate from extra_usage). Edit this list to
  /// add/remove keys as the API evolves. Keys are the RAW API keys, not labels.
  pub const EXCLUDED_WINDOW_KEYS: &[&str] = &[/* TODO(DIAG-003): fill from Phase-1 payload, e.g. "limit", "spend" */];
  pub fn is_excluded_window_key(key: &str) -> bool { EXCLUDED_WINDOW_KEYS.contains(&key) }
  ```
  **The literal key strings are filled in only after OQ-A1 resolves.** Match is exact (raw key),
  case-sensitive — confirm the live keys' exact casing from the payload.
- `usage_client.rs`: in the key loop, after the null skip (`:120-122`) and before building the
  `QuotaWindow` (`:124`), add `if crate::plan_detector::is_excluded_window_key(key) { continue; }`.
- Tests (`plan_detector.rs` `#[cfg(test)]`): `is_excluded_window_key("limit") == true` (using the
  confirmed keys), `is_excluded_window_key("five_hour") == false`,
  `is_excluded_window_key("seven_day") == false`. Keep all existing tests green. (Optionally a
  small test that a `Vec<(key,value)>` filtered by the predicate drops the excluded keys — but the
  parse loop itself is in `usage_client.rs` and is exercised manually; prefer the pure predicate
  test.)
- **No frontend change for Problem A** — excluded windows simply never arrive, and
  `usage-card.ts`'s differential update naturally drops bars no longer present in the snapshot
  (`usage-card.ts:120-123`).

### Problem B — decouple overusage data from `is_enabled` (FRONTEND-only; no Rust change)

**Data is already available frontend-side** (`UsageSnapshot.profile.has_extra_usage_enabled`,
`store.ts:28`, threaded via `poller.rs:256`) so **no Rust/serde change is required** — prefer
frontend-only per the constraint. Do NOT rename serde fields; do NOT thread
`has_extra_usage_enabled` into `ExtraUsage` (unnecessary — `profile` already reaches the UI).

**Recommended design (OQ-B2 = options b + c, with a folded in):**
1. **Always render the numbers and the bar whenever values are present, regardless of
   `is_enabled`.** In `overusage-section.ts`:
   - Remove the `vm.enabled ? … : 0` gate at `:113-115`; compute `fill =
     computeFill(vm.currentOverusage, vm.allowedOverusage, vm.utilization)` unconditionally.
     `computeFill` already returns 0 when no values are present (`:83`), so an empty bar still
     renders honestly when there is genuinely no data — but a present `used/limit` now always
     fills.
   - `formatCredits` already shows `€x.xx` for present values and `—` for null (`:49-52`) — keep;
     it is no longer suppressed by `enabled`.
   - Stop adding the `disabled` mute purely from `enabled`. Re-scope what "disabled/muted" means
     (see pill below): mute only when the indicator says off AND there is no data to show, so real
     cap-met data is never greyed out.
2. **Pill reflects the authoritative enabled signal (fold in option a).** Drive the on/off pill
   from `profile.has_extra_usage_enabled` rather than `extra_usage.is_enabled`. This requires
   passing `snap.profile` into the renderer:
   - `usage-card.ts:179-181`: change the call to
     `renderOverusageSection(footer, snap.extra_usage, snap.profile)` (Unit owns this file).
   - `overusage-section.ts`: extend `renderOverusageSection(footer, eu, profile?)` signature; the
     pill on/off reads `profile?.has_extra_usage_enabled ?? eu.enabled` (fallback to the old field
     if profile is absent, e.g. during early/degraded states). Keep `deriveOverusage` pure; either
     pass the boolean in or add a small `resolveEnabled(eu, profile)` pure helper for testability.
3. **Distinct "limit reached" state (option c).** When both values are present and
   `used >= limit` (and `limit > 0`), render a clear "limit reached" treatment (e.g. a small
   `limit reached` pill/badge + danger-coloured bar at 100% via the existing
   `getFillClass(>=90)→danger`). The exact label/visual and whether the cap-met state also flips
   `is_enabled`/`utilization` is **finalised against OQ-B1**; the gating predicate (`used >=
   limit`) is decidable now. Add a pure exported helper, e.g.
   `isLimitReached(used, limit): boolean`, for unit-testing.
- **CSS (`styles.css`):** reuse existing tokens. Add a small `.overusage-pill.reached` (or reuse
  danger red) and ensure the section is no longer auto-muted by `enabled` alone (adjust/limit
  `.overusage-section.disabled` usage `:239-241`). Keep changes minimal and within the existing
  palette (#ff9800 amber, muted whites, the danger red already used by `.progress-fill.danger`).
- **Differential-DOM + `—` null handling preserved:** the renderer still writes
  `footer.innerHTML` wholesale (run-002 pattern) and `formatCredits` still returns `—` for null —
  unchanged.

**Why no Rust change:** the requirement explicitly prefers frontend-only when the data already
reaches the UI; it does (`profile` in the snapshot). If, after Phase-1 data, the team decides the
pill should also reflect a field not currently surfaced, that would be a flagged Rust/serde
addition — but none is anticipated.

---

## Tests required

Per `testing-qa` (Rust `cargo test`; frontend has no runner wired — keep new logic in pure
exported helpers, verify via `tsc` + manual; add Vitest only if introduced):

- **Backend (`plan_detector.rs` `#[cfg(test)]`):**
  - `is_excluded_window_key(<confirmed key>) == true`; `is_excluded_window_key("five_hour") ==
    false`; `is_excluded_window_key("seven_day") == false`.
  - Existing `label_for_key`, `sort_windows`, plan-detection tests stay green.
- **Frontend pure helpers (`overusage-section.ts`):**
  - `resolveEnabled`/pill source: profile-enabled overrides `eu.enabled`; falls back to `eu.enabled`
    when profile is null.
  - data-always-shows: with `enabled=false` but `used=1921, limit=2000`, `computeFill` → ~96.05 and
    `formatCredits` → `€19.21`/`€20.00` (not zeroed, not `—`).
  - `isLimitReached`: (2000,2000)→true; (2100,2000)→true; (1921,2000)→false; (x,null)/(x,0)→false.
- **Manual / integration:**
  - Phase 1: run app once → console (`RUST_LOG=warn`) and `diag-usage.json` contain the raw body +
    key list; confirm NO token/credential appears anywhere in the output.
  - Phase 2 / A: "Limit" and "Spend" bars are gone; `five_hour`, `seven_day`, model windows still
    render and sort correctly; a freshly-empty `five_hour` (0%) still shows.
  - Phase 2 / B: with overusage at/over the cap, the numbers and a filled bar show even though the
    pill reads off (or shows "limit reached"); null values still render `—`; footer still pinned at
    the bottom (run-002 invariant, `usage-card.ts:174-178`).

**Verification commands:** `pnpm exec tsc --noEmit` then `pnpm build` (frontend);
`cd src-tauri && cargo test` and `cargo build` (backend); `pnpm tauri:dev` for manual checks.

---

## Risks & rollback

- **Phase 1 (diagnostic) risk — accidental secret logging.** Mitigated: only the response *body*
  is logged (the bearer token is in the request header, never in the body); headers/credentials are
  never touched. The block is fenced with `DIAG-003` markers and a file written to the app config
  dir (not `.claude`). Rollback = delete the fenced block + the temp file. **MUST be removed in
  Phase 2** (explicit constraint).
- **Problem A risk — wrong/over-broad key exclusion.** If the wrong literal is added, a legit
  window could vanish, or the noise could persist. Mitigated by finalising keys only from the live
  payload (OQ-A1), exact case-sensitive match, and a one-line-editable constant. Rollback = empty
  the `EXCLUDED_WINDOW_KEYS` array (one line) — bars return immediately. No data loss; purely
  presentational filtering at parse time.
- **Problem B risk — showing stale/irrelevant numbers, or a confusing pill.** Mitigated: numbers
  only ever come straight from `extra_usage` (no derivation beyond the existing ratio), `—` for
  null is preserved, and the pill semantics are finalised against the cap-met payload (OQ-B1).
  Rollback = revert `overusage-section.ts` + the one-line `usage-card.ts` call change + the CSS
  tweak; behaviour returns to the run-002 "tied to enabled" rendering.
- **Contract stability.** No serde field renamed; no new Rust→TS contract field added (profile
  already flows). `ExtraUsage`/`Profile`/`UsageSnapshot` shapes unchanged → no risk to other
  consumers.
- **Sequencing risk.** Phase-2 fixes must not merge before OQ-A1/OQ-B1 are answered; the work-unit
  statuses below encode this (Phase-2 units start `blocked`).

---

## Work units (for parallel developer subagents)

- [x] **Unit E — Phase-1 diagnostic logging** — status: done — **backend** —
  agent: developer-backend — phase: 1.
  - files (disjoint, Unit E owns exclusively): `src-tauri/src/usage_client.rs` (fenced `DIAG-003`
    block only — body/keys log + best-effort temp-file write reusing `settings.rs`
    `app_config_dir()`).
  - depends on: none. **Ships first; unblocks everything else.**
  - skill: none (follow `log::warn!` precedent at `window_ctl.rs:38`, `app_config_dir()` idiom in
    `settings.rs`).
  - removal: deleted by Unit F as the first step of Phase 2.

- [x] **Unit F — Problem A: targeted key exclusion (+ remove DIAG-003)** — status: done —
  agent: developer-backend — **backend** — phase: 2.
  - files (disjoint, Unit F owns exclusively): `src-tauri/src/plan_detector.rs`
    (`EXCLUDED_WINDOW_KEYS` const + `is_excluded_window_key` + test), `src-tauri/src/usage_client.rs`
    (one `continue` guard in the key loop AND deletion of the Unit-E DIAG-003 block).
  - depends on: **OQ-A1 (Phase-1 payload)** for the literal keys; Unit E (removes its diagnostic).
  - skill: none.

- [ ] **Unit G — Problem B: decouple overusage data from is_enabled** — status: blocked —
  agent: developer-frontend — **frontend** — phase: 2.
  - files (disjoint, Unit G owns exclusively):
    - `src/components/overusage-section.ts` — remove the `vm.enabled ? … : 0` fill gate; always
      render numbers + bar; pill from `profile.has_extra_usage_enabled` (fallback `eu.enabled`);
      add `resolveEnabled` + `isLimitReached` pure helpers; "limit reached" state.
    - `src/components/usage-card.ts` — **one-line change** to pass `snap.profile` into
      `renderOverusageSection` (`:179-181`). Unit G owns this file; no backend unit touches it.
    - `src/styles.css` — overusage block tweaks (limit-reached pill, stop auto-muting on `enabled`
      alone). Within existing tokens.
  - depends on: **OQ-B1 (Phase-1 payload)** to finalise pill/limit-reached semantics. The
    data-decoupling + profile-pill plumbing can be built immediately; only the cap-met visual
    wording waits on the payload.
  - skill: none (follow existing TS patterns in `overusage-section.ts`/`window-bar.ts`).

### De-conflict rules for shared files
- `src-tauri/src/usage_client.rs` is touched by **Unit E** (add DIAG-003) then **Unit F** (remove
  DIAG-003 + add the exclusion guard). These are **sequential, same agent (developer-backend)** —
  Unit F runs after Unit E, in the same file, no parallel write. No other unit edits this file.
- `src/components/usage-card.ts` is touched **only by Unit G** (the one-line profile pass-through).
  No backend unit edits frontend files; no frontend unit edits Rust files.
- `plan_detector.rs` → Unit F only. `overusage-section.ts` + `styles.css` → Unit G only.
- `lib.rs` is **not touched** this run (no new commands; no contract change).

---

## Contracts (seams between units)

- **C1 (existing, UNCHANGED):** `UsageSnapshot { profile: Profile | null; extra_usage: ExtraUsage |
  null; windows: QuotaWindow[]; … }` — `model.rs:154-165` ↔ `store.ts:40-48`. Unit G consumes
  `extra_usage` AND `profile.has_extra_usage_enabled` (`store.ts:28`) as-is. **No backend change to
  the contract** — `profile` already reaches the UI.
- **C2 (Problem A, backend-internal, no wire change):** excluded keys are filtered before windows
  are built, so the `windows: QuotaWindow[]` array simply omits them. No new field, no shape
  change; the frontend differential renderer (`usage-card.ts:120-123`) drops the now-absent bars
  automatically.
- **No new Tauri command, no new serde field, no renamed field.**

---

## Parallel schedule

1. **Phase 1 (now):** Unit E ships alone (backend). User runs the app once and reports the live
   top-level keys + the cap-met `extra_usage` object. This resolves OQ-A1 and OQ-B1.
2. **Phase 2 (after payload):** Unit F (backend) and Unit G (frontend) run **concurrently** — fully
   disjoint files (`plan_detector.rs` + `usage_client.rs` vs. `overusage-section.ts` +
   `usage-card.ts` + `styles.css`). Unit F's first action is deleting the Unit-E DIAG-003 block.
3. Both Phase-2 units are `blocked` until the payload lands; do not merge Phase-2 changes before
   OQ-A1/OQ-B1 are answered.

## Suggested follow-ups (out of scope)
- Wire Vitest (`countdown.ts` notes the command) so `resolveEnabled`/`isLimitReached`/`computeFill`
  get real specs.
- If Anthropic adds more noise windows, extend `EXCLUDED_WINDOW_KEYS` (one line) — or, if the
  pattern becomes "any window with no `resets_at` and 0% is non-quota", consider a structural rule
  instead of a name blocklist (explicitly rejected for now per the user's "remove these two
  specifically" decision).
- Consider surfacing a currency/locale from `/api/oauth/profile` if `€` ever needs to vary
  (carried over from run 002).
