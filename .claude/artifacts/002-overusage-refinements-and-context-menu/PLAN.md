# PLAN: Overusage refinements + context-menu clipping/dismissal fixes
_id: 002-overusage-refinements-and-context-menu_
_status: ready_
_last-updated: 2026-06-16_

> Stack: Tauri 2 (Rust core in `src-tauri/src/`, vanilla-TS WebView UI in `src/`).
> No planner skill matches Tauri/Rust-desktop in this repo's skill set; context was mapped
> manually by reading every relevant source file. All facts below are quoted from the actual
> code, not assumed. Follow-up to run `001-overusage-and-opacity` (shipped: overusage section
> + persisted opacity).

---

## Requirement (restated)

Six fixes/refinements to the shipped overusage overlay:

1. **Wrong magnitude (×100).** Raw `used_credits` / `monthly_limit` are in **cents** (minor
   units). UI prints them as whole units, so €20 shows as 2000.00 and 1921 shows as 1921.00.
   Must show 20.00 and 19.21 respectively. Decide ONE conversion site and justify.
2. **Currency symbol → €.** Replace hardcoded `$` with `€`. API exposes no currency field
   (verified) — hardcode `€`; flag hardcode-vs-detect as an open question.
3. **Overusage always at the BOTTOM** of the indicators regardless of plan/subscription state.
4. **Reformat the overusage indicator** to: one header row `Overusage  <spent>  <allowed>`
   (label left, two values right-aligned), then a full-width progress bar below (fill = spent
   vs allowed). Replaces the current header+pill / separate stats-line layout. Decide the
   ON/OFF pill's fate and the fill-% source.
5. **Context menu clipped** when the overlay window is small (menu is a DOM element inside a
   tiny WebView, cannot exceed window bounds). Recommend the best Tauri-2 fix.
6. **Context menu does not close on click-away** — diagnose and fix (likely: clicks outside the
   small window never reach the WebView; add a window blur / focus-loss dismissal).

Implied asks:
- Keep the existing differential-DOM render path and graceful null handling (`—`).
- Keep changes minimal and reversible; partition cleanly into frontend vs backend units.

---

## 1. Architecture recap (render + window-control paths, with file:line citations)

### Data path (Rust → WebView)
- `src-tauri/src/config.rs:5` — `USAGE_URL = https://api.anthropic.com/api/oauth/usage`.
- `src-tauri/src/usage_client.rs:59` `fetch_usage()` parses the body as `serde_json::Value`,
  iterates keys; the `extra_usage` key deserialises into `RawExtraUsage`
  (`usage_client.rs:104-117`) and maps to `model::ExtraUsage { enabled, used_credits,
  monthly_limit, utilization }`. **No scaling is applied here** — `used_credits` /
  `monthly_limit` pass through as raw `Option<f64>`.
- `src-tauri/src/model.rs:48-54` `RawExtraUsage` — wire shape: `is_enabled, monthly_limit,
  used_credits, utilization` (all `Option`). **Do NOT rename** (serde matches the JSON keys).
- `src-tauri/src/model.rs:115-121` `ExtraUsage` — normalized: `{ enabled: bool, used_credits:
  Option<f64>, monthly_limit: Option<f64>, utilization: Option<f32> }`.
- `src-tauri/src/poller.rs` builds `UsageSnapshot { …, extra_usage, … }`; `src-tauri/src/lib.rs:104`
  emits it as `app_handle.emit("usage://snapshot", &snapshot)`.
- `src/main.ts:52-66` `listen<UsageSnapshot>('usage://snapshot', …)` → `store.set` → subscription
  (`main.ts:47-49`) calls `renderSnapshot`.
- TS mirror: `src/store.ts:15-20` `interface ExtraUsage { enabled; used_credits|null;
  monthly_limit|null; utilization|null }`.

### Render path (WebView)
- `src/components/usage-card.ts:54` `renderSnapshot()`:
  - early-returns hide the footer for `loading` (`:80-88`), `auth_expired` (`:90-98`), and
    `windows.length === 0` (`:100-108`).
  - quota bars are built/diffed into `#card-body` (`:110-151`) via `window-bar.ts`
    (`createWindowBar`/`updateWindowBar`), then **the whole body is cleared and re-appended in
    canonical order** (`:145-148`).
  - the optional `.stale-info` line is appended to `#card-body` (`:153-167`).
  - the overusage section is rendered into `#card-footer` (`:169-178`) whenever
    `snap.extra_usage` is present; the footer is a **separate flex-shrink:0 region below the
    scrollable body** (`buildCard` shell `:31`; CSS `.card-footer` `styles.css:144-148`).
- `src/components/overusage-section.ts` — `renderOverusageSection(footer, eu)` (`:55`) writes
  `footer.innerHTML`. Current layout = header row (label + on/off pill `:82-85`), optional
  progress bar (`:66-74`, gated on `enabled && utilization!==null`, width = `utilization`), then
  a separate `current · allowed` stats line (`:87-93`). `formatCredits` (`:36-39`) =
  `` `$${value.toFixed(2)}` ``. `getFillClass` (`:41-46`) thresholds the bar colour.

### Window-control path (context menu + Tauri)
- `src/components/context-menu.ts` builds one `<div class="context-menu">` appended to
  `document.body` (`:22-27`), positioned `position:fixed` (CSS `styles.css:293-306`).
  Menu height ≈ header label + opacity slider + Size (4 items) + Plan (6 items) + Refresh +
  Quit ≈ **300-340px tall** — taller than Small/Medium window heights (160/220).
  - `show(x,y,opts)` (`:107-140`) positions via `left/top`, then clamps with
    `getBoundingClientRect()` against `window.innerWidth/innerHeight` (`:132-139`). **The clamp
    keeps the menu inside the WebView, which is exactly why a tall menu gets capped/clipped in a
    small window** — there is no room to fit, so it overflows the bottom and is cut by
    `overflow:hidden` on `html,body` (`styles.css:8-17`).
  - dismissal: outside-click handler (`:158-163`) hides when a click lands outside `#context-menu`;
    Escape handler (`:165-167`). **No blur/focus-loss handler exists** — clicking another app or
    the desktop never produces a DOM `click` in this WebView, so the menu stays open (issue 6).
  - `invoke('set_opacity', …)` (`:38`) and `invoke('set_size_preset', …)` (`:48`) call Rust.
- `src-tauri/src/window_ctl.rs` — `set_opacity` (`:21-42`, persists via `settings`),
  `set_size_preset` (`:51-64`, applies Logical size from `config.rs:43-51` presets),
  `set_always_on_top` (`:67-73`), `toggle_visibility` (`:76-85`), `quit_app` (`:138-142`).
  Commands registered in `lib.rs:110-119` `invoke_handler!`.
- `src-tauri/capabilities/default.json:6-20` grants `core:window:allow-set-size`,
  `allow-set-position`, `allow-set-always-on-top`, `allow-set-focus`, `allow-show`, `allow-hide`,
  `allow-is-visible`, `allow-start-dragging`, `core:event:allow-listen/emit`. **There is NO
  permission for creating a second webview window, nor an explicit `allow-set-focus`-on-blur
  listener; `core:window` events like `blur` are delivered via the JS `getCurrentWindow().onFocusChanged`
  API which needs no extra ACL beyond `core:default`** (verify during impl).
- `src-tauri/tauri.conf.json:14-30` — single `main` window: 260×200, `minWidth:200 minHeight:120`,
  `decorations:false transparent:true alwaysOnTop:true resizable:true`.

---

## 2. Gaps & open questions

- [NON-BLOCKING] **OQ-A — cents factor confirmed; pick the conversion site.** The ×100 hypothesis
  is **verified against both supplied data points**: raw `monthly_limit` 2000 → 20.00 (=€20 cap),
  raw `used_credits` 1921 → 19.21. So values are in **cents**. Recommendation below (TS formatter
  divides by 100). Confirm there is no third consumer of these raw values that would double-divide
  (there is not: `used_credits`/`monthly_limit` are only read in `overusage-section.ts`).
- [NON-BLOCKING] **OQ-B — hardcode `€` vs detect currency.** Verified: the API `extra_usage` block
  exposes **no currency field** (only `is_enabled, monthly_limit, used_credits, utilization` —
  `model.rs:48-54`, sample in `.shared/plans/overlay-plan.md:93-98`). No locale/currency anywhere
  in the payload. **Default: hardcode `€`** per the user's explicit ask. If currency-correctness
  ever matters, the only detect option is account locale from `/api/oauth/profile` (not currently
  fetched for currency) — out of scope. Flagging so the user confirms `€` for all accounts.
- [RESOLVED — KEEP INDICATOR] **OQ-C — ON/OFF indicator.** User decision (2026-06-16): **KEEP a
  small on/off indicator next to the "Overusage" label**, in addition to the new layout. Use a
  compact indicator (small "on"/"off" pill or a coloured dot + short text) placed inline with the
  label in the header row, sized to not crowd the two right-aligned values. Reuse the existing
  amber (#ff9800) "on" / muted-white "off" tokens. When `enabled === false`, also mute the section
  and render an empty/zero progress track.
- [NON-BLOCKING] **OQ-D — progress-bar fill source.** Two candidates: the API `utilization` field
  (0-100, **frequently null** per `overlay-plan.md:96-98`), or a derived `spent/allowed` ratio.
  Since the new layout shows spent and allowed explicitly, **derive fill = clamp(used/limit*100,
  0, 100)** when both are present; **fall back to `utilization`** when the ratio can't be computed
  (limit null/zero) but `utilization` is present; otherwise render an empty track. This keeps the
  bar consistent with the two numbers the user now sees. Confirm preference.
- [RESOLVED — OPTION (b)] **OQ-E — context-menu clipping approach.** User decision (2026-06-16):
  **option (b) — temporarily grow the overlay window while the menu is open, restore on close**,
  with **option (c) CSS scroll safety-net always on**. **Unit D runs** (window-size commands +
  ACL). Implement the grow-then-restore exactly as described in §3 issue 5.

No other blocking gaps. All non-blocking defaults are safe and reversible.

---

## 3. Per-issue design (with frontend/backend partition)

### Issue 1 — ×100 magnitude (FRONTEND)
**Where to convert: in the TS formatter, dividing by 100.** Justification:
- The raw cents value is the *true* stored unit; the Rust layer is a faithful, lossless mirror of
  the wire shape (`model.rs` deliberately does not transform — the codebase's convention is
  "Rust normalizes shape, not semantics"; see `usage_client.rs` passthrough). Converting in Rust
  (`f64/100.0`) would bake a presentation decision into the data model and risk a second consumer
  double-scaling later. Keeping cents in the model and converting at the single render site
  (`overusage-section.ts`) is the smallest, most local, most reversible change, and it co-locates
  the divide with the currency symbol that also changes (issue 2). It is also the only consumer.
- **Change:** in `src/components/overusage-section.ts`, `formatCredits` (`:36-39`) divides by 100
  before formatting. New body:
  `if (value === null) return '—'; return \`€${(value / 100).toFixed(2)}\`;`
  (the `€` swap is issue 2 — same one-line edit). This yields 2000→`€20.00`, 1921→`€19.21`.
- **No Rust change.** `model.rs` `ExtraUsage` stays in cents. Document in a code comment that
  `used_credits`/`monthly_limit` are minor units (cents) and the UI divides by 100.

### Issue 2 — currency `€` (FRONTEND)
- Same `formatCredits` edit as issue 1 (one function, both fixes). Hardcode `€` (OQ-B).
- Optional: hoist the symbol to a `const CURRENCY = '€';` at the top of `overusage-section.ts`
  so a future detect/config swap is one line. Do not add a Rust currency field.

### Issue 3 — overusage always at the BOTTOM (FRONTEND — verify, likely no functional change)
- **Current behaviour already places it at the bottom under all rendered states**: the overusage
  section lives in `#card-footer`, a `flex-shrink:0` sibling that sits **below** the scrollable
  `#card-body` in the card's column flex (`buildCard` shell `usage-card.ts:19-32`; CSS
  `.overlay-card{flex-direction:column}` `styles.css:37`, `.card-footer` `styles.css:144`). The
  quota bars (5-hour, Weekly, Limits, Spend) all render **inside `#card-body`**, never into the
  footer, for every plan — `renderSnapshot` only ever appends windows to `body` (`:145-148`) and
  the stale line to `body` (`:163`); nothing else is appended to `footer`.
- Therefore DOM order is **body(bars + stale) → footer(overusage)** in every non-early-return
  state, independent of plan/subscription. **No reordering bug exists for the live/stale/degraded
  states.** Unit C must *verify* this holds after the layout reflow in issue 4 and add a short
  code comment asserting the invariant ("overusage is rendered into #card-footer so it is always
  the last/bottom element").
- **One real risk to check:** if `#card-body` content is short, the footer should still pin to the
  bottom. It does (`.card-body{flex:1}` `styles.css:104` pushes the footer down). If during impl
  the footer ever visually floats up, ensure `.overlay-card` keeps `height:100%` and body keeps
  `flex:1`. No change expected.

### Issue 4 — reformat the overusage indicator (FRONTEND)
Target layout (single header row, then full-width bar):
```
Overusage          €19.21   /   €20.00
=========================================------
```
- **Rewrite `renderOverusageSection`** (`overusage-section.ts:55-95`) to emit:
  - A header row reusing `.quota-bar-header` (flex, space-between, baseline — `styles.css:235-240`):
    - left: `<span class="quota-bar-label">Overusage</span>`
    - right: a `.quota-bar-right`-style group with the two values right-aligned:
      `<span class="overusage-spent">€19.21</span>` and
      `<span class="overusage-allowed">€20.00</span>` (separated by a thin `/` or middot; keep
      tabular-nums). Use `formatCredits` for both; `—` when null.
  - Directly below: a `.progress-track > .progress-fill` bar (reuse existing CSS
    `styles.css:274-290`), width = fill% per OQ-D, colour via `getFillClass(fill%)`.
- **Remove** the separate `.overusage-stats` line (`:87-93`). **Per OQ-C (KEEP indicator): retain a
  compact on/off indicator inline with the "Overusage" label** in the header row (small pill or
  coloured dot + text), not a separate row. Size it so it does not crowd the two right-aligned
  values.
- **Fill computation helper** (pure, exported, testable): 
  `computeFill(used: number|null, limit: number|null, utilization: number|null): number` returning
  0-100. Logic: if `used!=null && limit!=null && limit>0` → `clamp(used/limit*100,0,100)`; else if
  `utilization!=null` → `clamp(utilization,0,100)`; else 0. **Note: used/limit are in cents — the
  ratio is unit-agnostic so divide-by-100 is irrelevant to the ratio; only the displayed numbers
  need /100.**
- **Disabled state (OQ-C):** when `eu.enabled === false`, add a `disabled`/muted class to the
  section root (greyed values, `progress-fill zero`). Keep rendering whenever `snap.extra_usage`
  is present (matches current `usage-card.ts:173`).
- **CSS:** update `src/styles.css` overusage block (`:165-226`): remove `.overusage-stats*` rules,
  **keep/repurpose `.overusage-pill.on/.off`** (or a small dot) for the retained inline indicator
  (OQ-C), add `.overusage-spent`/`.overusage-allowed` (tabular-nums, right-aligned group),
  `.overusage-section.disabled { opacity:.6 }`. Reuse `.progress-track`/`.progress-fill`. Stay
  within existing tokens (#ff9800 amber, muted whites).

### Issue 5 — context-menu clipping in a small window (BACKEND + FRONTEND seam)
**Root cause:** the menu is a `position:fixed` DOM node inside the WebView; the WebView is the
size of the OS window (Small 220×160, Medium 280×220, Large 340×280). The menu is ~300-340px tall.
`show()`'s clamp (`context-menu.ts:132-139`) can't make it fit, and `html,body{overflow:hidden}`
(`styles.css:8-17`) clips the overflow. A native menu was considered but **rejected: the menu
contains an opacity range slider, which OS-native context menus cannot host** — so the menu must
stay an in-WebView custom element (or a custom Tauri webview), not a `Menu`/`MenuItem` native menu.

**Options evaluated:**
- (a) **Separate Tauri `WebviewWindow` for the menu** that can exceed overlay bounds. Pros: never
  clipped, can be larger than the overlay, gets its own `onFocusChanged` for clean dismissal.
  Cons: heaviest — new window lifecycle, new HTML/JS entry or a shared route, new capability
  permissions (`core:webview:allow-create-webview-window` etc.), positioning at the cursor in
  screen coords, plumbing the slider's `set_opacity` from the child window, focus juggling with an
  always-on-top overlay. High complexity for a tiny menu.
- (b) **Temporarily grow the overlay window while the menu is open; restore on close.** *(Recommended.)*
  On `contextmenu`, before showing, ensure the window is at least tall/wide enough for the menu
  (e.g. grow height to `max(currentH, menuNeededH)` and width to `max(currentW, menuMinWidth≈200)`),
  show the menu, and on `hide()` restore the previous size. Pros: reuses the existing
  `core:window:allow-set-size` permission and the established `set_size_preset` pattern; menu stays
  one DOM element; no new window/HTML. Cons: the overlay card visibly resizes while the menu is
  open (acceptable — it's a transient interaction); must remember & restore the exact prior size
  (including custom user resizes, not just presets), and re-clamp on-screen.
- (c) **Compact + internally scrollable menu** (frontend-only). Make `.context-menu`
  `max-height: calc(100vh - 8px); overflow-y:auto`, shrink paddings, possibly collapse Plan-override
  into a submenu. Pros: zero backend, zero permissions, smallest change. Cons: in a 160px-tall
  window the menu becomes a tiny scroll area — usable but cramped; doesn't fully satisfy "not
  clipped/capped" for a long menu.
- (d) **Hybrid:** (c) as an always-on safety net (menu never overflows the window even if resize
  is disabled), plus (b) for a good experience. **This is the proposed combination.**

**Recommendation: (b) grow-then-restore, with (c) as a guaranteed fallback safety net.**
- Backend (Unit D): add a Tauri command to read current size and set a size, OR a single command
  `ensure_min_size_for_menu(width, height) -> prevSize` and `restore_size(prevSize)`. Simpler:
  add `get_window_size() -> (f64,f64)` and reuse a generic `set_window_size(w,h)` (the menu code
  computes target = max(prev, needed)). Use Logical units to match `set_size_preset`
  (`window_ctl.rs:62`). Register in `lib.rs` `invoke_handler!`. Verify `core:window:allow-set-size`
  covers it (it does — `default.json:8`); `get_window_size` may need
  `core:window:allow-inner-size`/`outer-size` — **add the needed ACL permission(s) to
  `capabilities/default.json`** (Unit D owns that file).
- Frontend (Unit C): in `context-menu.ts`, on `contextmenu`, `await invoke('get_window_size')`,
  compute the menu's needed height (measure after building, or use a known constant), grow if
  needed via `invoke('set_window_size', …)`, then position/show. On `hide()`, restore the saved
  size. Always also apply the (c) safety CSS (`max-height`/`overflow-y:auto`) so the menu can
  never be clipped even if a resize call fails.
- **Seam/contract C2 (below).** Unit D produces the commands + ACL; Unit C consumes them.

### Issue 6 — context menu doesn't close on click-away (FRONTEND, ties to issue 5)
**Diagnosis:** the existing outside-click handler (`context-menu.ts:158-163`) only fires for clicks
that land **inside the WebView**. When the user clicks the desktop or another application (the
common "click away" for a tiny always-on-top overlay), no DOM `click` reaches this WebView, so the
menu never hides. Escape works; click-away outside the window does not.
**Fix (frontend, Unit C):**
- Add a window focus-loss handler using the Tauri JS API:
  `getCurrentWindow().onFocusChanged(({ payload: focused }) => { if (!focused) hide(); })` (wired in
  `init()`), AND a belt-and-braces `window.addEventListener('blur', () => hide())`. Either fires
  when focus leaves the overlay → the menu closes on click-away to another app/desktop. Keep the
  existing inside-click and Escape handlers.
- **Interaction with issue 5 option (b):** growing/restoring the window must not itself trigger a
  spurious blur that closes the menu immediately. Mitigate: only bind the blur/`onFocusChanged`
  dismissal *after* the menu is shown and the resize has settled (e.g. set a short `menuOpen`
  guard, or ignore the first focus event within N ms of opening). Restoring size happens inside
  `hide()`, after which the guard resets. Verify no resize→blur→hide loop during impl.
- If the user later chooses issue-5 option (a) (separate window), dismissal moves to the child
  window's own `onFocusChanged` — note but do not build unless (a) is chosen.

---

## 4. Tests required

Per `testing-qa` (Rust `cargo test`; frontend has no runner wired — keep new logic in pure
exported helpers and verify via `tsc` + manual; add Vitest only if introduced):
- **Frontend pure helpers (`overusage-section.ts`):**
  - `formatCredits`: 2000 → `€20.00`; 1921 → `€19.21`; 0 → `€0.00`; null → `—`.
  - `computeFill`: (1921,2000,null) → ~96.05; (used,null,42) → 42; (null,null,null) → 0;
    (used>limit) → clamped 100; (used,0,util) → falls back to util or 0.
  - `deriveOverusage`: passthrough of enabled + raw cents values unchanged (no scaling in derive).
- **Rust (`window_ctl.rs` / Unit D, if option (b)):** logic test on any size-clamp/compute helper
  if one is extracted; the `set_window_size`/`get_window_size` commands are thin Tauri wrappers
  (manual verification). Keep existing `settings.rs` and `model.rs` tests green.
- **Manual / integration:**
  - Overusage row reads `Overusage  €19.21  €20.00` with a bar ~96% filled; bottom of card on
    Pro, Max, and a plan with many quota bars (scrolls body, footer pinned).
  - Disabled state renders muted with empty bar.
  - Small window (220×160): right-click → menu fully visible (grows window per (b)) OR scrolls
    (fallback (c)); restores size on close.
  - Click another app/desktop → menu closes; Escape closes; inside-click outside menu closes.

**Verification commands:** `pnpm exec tsc --noEmit` then `pnpm build` (frontend);
`cd src-tauri && cargo test` and `cargo build` (backend); `pnpm tauri:dev` for manual checks.

---

## 5. Risks & rollback
- **Issue 1/2/4 are frontend-only, additive, low-risk.** Rollback = revert `overusage-section.ts`
  + `styles.css`. Worst case: nulls render `—` (honest), or a wrong currency symbol (cosmetic).
- **Issue 1 mis-scaling risk:** if any account ever returns *units* not cents, the /100 would show
  €0.19 for €19. Mitigated by the two verified data points and the fact this is the sole consumer;
  flagged as OQ-A. The `const CURRENCY`/divide live in one function for a one-line revert.
- **Issue 5 option (b) risk:** window resize jank, failure to restore exact prior size (esp. after
  a user manual-resize), or a resize→blur→auto-close loop with issue 6. Mitigations: capture exact
  current size before growing; restore in `hide()`; guard the blur dismissal during the
  open/resize transition; the (c) CSS safety net guarantees no clipping even if backend calls fail.
  Rollback = revert `context-menu.ts` resize calls + the new Rust commands + the ACL additions;
  the (c) CSS alone still prevents clipping.
- **Issue 6 risk:** an over-eager blur handler closing the menu when the slider is dragged or when
  the window grows. Mitigated by the open/resize guard and by only dismissing on genuine
  focus-loss. Rollback = remove the `onFocusChanged`/blur listeners; Escape + inside-click remain.
- **Capabilities change (Unit D):** adding window permissions to `default.json` widens the ACL
  surface minimally (size read/write only). No secrets, no new network. `auth-security` review not
  required.

---

## 6. Work units (for parallel developer subagents)

- [x] **Unit C — Overusage refinements + context-menu dismissal & scroll-safety** —
  status: done — **frontend** — agent: developer-frontend — issues: 1, 2, 3, 4, 6, and the
  frontend half of 5 (consume backend commands + (c) CSS safety net).
  - files (disjoint, Unit C owns exclusively):
    - `src/components/overusage-section.ts` — `formatCredits` /100 + `€`; new `computeFill`;
      rewrite `renderOverusageSection` to the new layout; drop pill/stats line.
    - `src/components/context-menu.ts` — add `onFocusChanged`/`blur` dismissal; consume
      `get_window_size`/`set_window_size` for grow-then-restore (gated on OQ-E); open/resize guard.
    - `src/styles.css` — overusage layout rules; `.context-menu { max-height; overflow-y:auto }`
      safety net; compact paddings.
    - `src/components/usage-card.ts` — **comment-only**: assert the footer/bottom invariant
      (issue 3). No functional change expected; if a real reflow fix is needed it lives here and
      Unit C owns it (Unit D must not touch this file).
  - depends on: Unit D's contract C2 for the grow-then-restore calls (issue 5 (b)); the (c) CSS
    safety net and all of issues 1/2/3/4/6 have **no dependency** and start immediately.
  - skill: none (follow existing TS patterns in `window-bar.ts` / `overusage-section.ts`).

- [ ] **Unit D — Window size read/write commands + ACL (for menu grow-then-restore)** —
  status: pending — **backend** — agent: developer-backend — issue: backend half of 5 (only if
  OQ-E confirms option (b)).
  - files (disjoint, Unit D owns exclusively):
    - `src-tauri/src/window_ctl.rs` — add `get_window_size(app) -> (f64,f64)` (Logical) and
      `set_window_size(app, width, height)` (or a combined `ensure_window_size`); reuse the
      `main_window()` helper and Logical-size pattern from `set_size_preset` (`:51-64`).
    - `src-tauri/src/lib.rs` — register the new command(s) in `invoke_handler!` (`:110-119`)
      **only** (additive lines).
    - `src-tauri/capabilities/default.json` — add any ACL permission needed to read window size
      (e.g. `core:window:allow-inner-size` / `allow-outer-size`); `allow-set-size` already present.
  - depends on: **OQ-E confirmation** (blocking for this unit). If the user picks option (c)
    instead, **Unit D is dropped** and Unit C does (c) alone.
  - skill: none.

### De-conflict rule for shared files
- `src-tauri/src/lib.rs` is touched **only by Unit D** (command registration). Unit C must not edit
  it.
- `src/components/usage-card.ts` is touched **only by Unit C** (issue-3 comment / any reflow). Unit
  D must not edit it.
- No file is co-owned. `overusage-section.ts`, `context-menu.ts`, `styles.css` → Unit C only.
  `window_ctl.rs`, `default.json` → Unit D only.

## 7. Contracts (seams between units)
- **C1 (existing, unchanged):** `UsageSnapshot.extra_usage: ExtraUsage | null`,
  `ExtraUsage = { enabled; used_credits|null (cents); monthly_limit|null (cents); utilization|null }`
  (`model.rs:115-121` ↔ `store.ts:15-20`). Unit C consumes as-is; **no backend change** — cents→units
  conversion is frontend-only.
- **C2 (new, owned by Unit D, consumed by Unit C — only if OQ-E = option (b)):**
  - `get_window_size() -> { width: number; height: number }` (Logical pixels).
  - `set_window_size(width: number, height: number) -> void` (Logical pixels).
  - Plus the matching ACL permissions in `capabilities/default.json`.
  Unit C calls these to grow the overlay before showing the menu and restore on hide.

## 8. Parallel schedule
- **Start immediately, concurrently:**
  - Unit C: issues 1, 2, 3, 4, 6, and the (c) CSS safety net of issue 5 — none depend on backend.
  - Unit D: only after **OQ-E is confirmed as option (b)**. While unconfirmed, Unit D is blocked.
- **Gated:** Unit C's grow-then-restore calls (issue 5 (b)) wait on Unit D's C2 commands. Unit C
  should build issues 1/2/3/4/6 + CSS safety net first, then integrate C2 once Unit D lands.
- If OQ-E resolves to **option (c)**, Unit D is dropped; Unit C completes issue 5 alone (CSS only),
  and there is no backend work this run.

## Suggested follow-ups (out of scope)
- Wire Vitest (`countdown.ts:86`) so `formatCredits`/`computeFill`/`deriveOverusage` get real specs.
- If currency-correctness across regions becomes a requirement, fetch account locale/currency from
  `/api/oauth/profile` and thread a currency code through `ExtraUsage` (replaces hardcoded `€`).
- Persist window size/position (the `settings.rs` module is built to extend) so grow-then-restore
  interacts cleanly with a user-chosen size.
