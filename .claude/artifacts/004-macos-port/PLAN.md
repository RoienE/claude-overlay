# PLAN: macOS port of claude-overlay

_id: 004-macos-port_
_status: ready_
_last-updated: 2026-06-18_

## Requirement

Make the Tauri 2 `claude-overlay` desktop app buildable, installable, and runnable on **macOS**
(currently Windows 11 only), without regressing Windows behaviour.

Explicit asks:
- Ordered, file-grouped change list, split into **must-have** (functional correctness on macOS)
  vs **nice-to-have** (native idle detection; signing/notarization for distribution).
- For each change: what, why, cross-platform pitfalls (`cfg(target_os=…)` vs runtime checks,
  path handling).
- Explicit open questions / decisions the user must make.
- macOS verification steps per feature.
- Call out what is already cross-platform and needs no change (avoid over-engineering).

Implied asks:
- Keep a single cross-platform codebase; gate OS-specific code with `cfg`, not forks.
- Read-only credential access on macOS must mirror the Windows guarantee (never write the token).
- Preserve existing Windows touchpoints (idle detection, taskbar/tray, window flags).

## Existing context touched (verified against source)

- `src-tauri/src/credential_source.rs`
  - `credentials_path()` (lines 44-74) already tries, in order: `CLAUDE_CONFIG_DIR` env →
    `USERPROFILE\.claude\.credentials.json` → `HOME/.claude/.credentials.json`. So the **HOME
    fallback already exists** and would work on macOS *iff* the file exists.
  - `read_credentials()` (lines 78-117) reads the file as a string, parses `ClaudeCredentials`
    (`claudeAiOauth.accessToken`, `expiresAt`, `subscriptionType`, `rateLimitTier`), checks expiry.
  - **CONFIRMED RISK:** On macOS, Claude Code stores the OAuth token in the **macOS Keychain**
    under service name `Claude Code-credentials`, *not* in a plaintext file by default. The file
    `~/.claude/.credentials.json` is only a fallback that Claude Code itself writes in special
    cases (e.g. SSH). So on a normal macOS install, `credentials_path()` returns `None` and the
    overlay shows AuthExpired forever despite a logged-in Claude Code. The Keychain item's secret
    value is the **same JSON** (`{"claudeAiOauth": {...}}`) the file would hold, retrievable via
    `security find-generic-password -s 'Claude Code-credentials' -w`. This is the must-fix item.

- `src-tauri/src/fallback_logs.rs`
  - `claude_dir()` (lines 20-40) mirrors the same 3-step resolution incl. `HOME` fallback.
    `~/.claude/projects/**/*.jsonl` resolution **already works on macOS** via the HOME branch.
    Only the doc comment (lines 4-6) is Windows/Linux-only and should mention macOS.

- `src-tauri/src/poller.rs`
  - `is_system_idle_or_locked()` (lines 44-79): `#[cfg(target_os = "windows")]` uses
    `winapi` `GetLastInputInfo` + `GetTickCount`; the `#[cfg(not(target_os = "windows"))]` arm
    (lines 77-78) **returns `false` unconditionally**. Consequence on macOS: idle detection is a
    no-op, so the adaptive-polling "pause when idle" feature (gated on `IDLE_PAUSE > 0`, default
    300 in config.rs:29) never triggers. App still works; it just polls during idle. This is a
    nice-to-have, not a correctness blocker.

- `src-tauri/Cargo.toml`
  - `[target.'cfg(windows)'.dependencies]` (lines 32-33) pulls in `winapi`. Need a parallel
    `[target.'cfg(target_os = "macos")'.dependencies]` block only if we implement native idle
    and/or Keychain via a crate (see decisions). `reqwest` already uses `rustls-tls` (line 25),
    so no OpenSSL/system-TLS issue on macOS.

- `src-tauri/tauri.conf.json`
  - Window flags (lines 14-30): `decorations: false`, `transparent: true`, `alwaysOnTop: true`,
    `resizable: true`, `skipTaskbar: false`, `shadow: false`, `focus: true`. These are honoured by
    Tauri on macOS but with platform quirks (see Plan §6). `transparent: true` on macOS requires
    the `macos-private-api` feature to be enabled (Tauri gate) — **currently NOT enabled**, so
    transparency will likely fail on macOS until added. There is **no `app > macOSPrivateApi`**
    key and **no `bundle > macOS`** section in the config today.
  - `bundle.targets: "all"` (line 34) → on macOS this produces `.app` + `.dmg`. Icons list
    (lines 35-41) already includes `icons/icon.icns`, which **exists** (verified in
    `src-tauri/icons/`). So icon assets need no change.

- `src-tauri/src/lib.rs`
  - Tray built unconditionally (lines 34-78) with `tray-icon` feature (Cargo.toml:21). Works on
    macOS (shows in the menu bar). No Dock-hiding / activation-policy code exists → on macOS the
    app currently shows a Dock icon and a menu-bar app name. Decide whether to hide from Dock
    (`ActivationPolicy::Accessory`) for an overlay (nice-to-have / UX).
  - Startup opacity applied via `window.eval` on `#app` (lines 80-89) — cross-platform, no change.

- `src-tauri/src/settings.rs`
  - Uses `app.path().app_config_dir()` (line 39) → resolves to the correct per-OS dir on macOS
    (`~/Library/Application Support/com.claude-overlay.app/`). **Already cross-platform.**

- `src-tauri/src/main.rs`
  - `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` (line 2) is a no-op on
    non-Windows targets (the attribute is Windows-only by definition). **No change needed.**

- `src-tauri/src/window_ctl.rs`, `usage_client.rs`, `plan_detector.rs`, `model.rs`,
  `config.rs`, the entire `src/` TypeScript UI, `capabilities/default.json`
  - All platform-neutral. **No change needed.** (`reqwest` rustls, `chrono`, serde, Tauri window
    commands, CSS opacity, events all work identically on macOS.)

- `README.md`
  - Prerequisites/Quick Start (lines 27-84) are Windows/PowerShell-only; "First Run" cites
    `%USERPROFILE%\.claude\.credentials.json`. Roadmap line 181 already lists "macOS port
    (Keychain credential source)" as a known TODO — this plan delivers it.

## Gaps & open questions

- [BLOCKING] **Keychain vs file credential source.** Confirmed: normal macOS Claude Code installs
  keep the token in Keychain (`Claude Code-credentials`), not a file. **Decision needed on the
  read mechanism:**
  - Option A (recommended, lowest-risk, no new deps): shell out to the system `security` binary —
    `security find-generic-password -s 'Claude Code-credentials' -w` — capture stdout, parse the
    returned JSON with the **existing** `ClaudeCredentials` serde type. Reuses all parsing/expiry
    logic; the only macOS-specific code is "get the JSON string". Caveat: first read may trigger a
    one-time Keychain access-prompt for the app (per-app ACL) — acceptable and expected for an
    unsigned/dev build; a signed app can be granted persistent access.
  - Option B: use a Rust crate (`security-framework`) to read the generic password
    programmatically — no subprocess, but adds a macOS-only dependency and more code.
  - Either way, keep the **file path as a fallback first** (so a user who has the SSH-style file
    still works), then fall back to Keychain. Confirm Option A is acceptable.
- [NON-BLOCKING] **Is distribution (code signing + notarization) in scope now?** Required only for
  distributing the `.dmg`/`.app` to *other* Macs without Gatekeeper warnings; needs an Apple
  Developer ID cert ($99/yr) and `xcrun notarytool`. For local build/run it is **not** needed.
  Default assumption unless told otherwise: out of scope for v1 (documented as a follow-up).
- [NON-BLOCKING] **Native macOS idle detection wanted, or is the no-op fallback acceptable?**
  If wanted, IOKit `HIDIdleTime` (via `io_registry`) or CoreGraphics
  `CGEventSourceSecondsSinceLastEventType` gives real idle seconds. Adds a macOS-only dep. If the
  app polling during idle is acceptable, leave the `false` fallback. Default assumption: implement
  it as a nice-to-have only if time allows; correctness does not depend on it.
- [NON-BLOCKING] **Hide from Dock (Accessory activation policy)?** Overlay-style apps often hide
  the Dock icon and run as a menu-bar-only agent. Decision: keep Dock icon (simpler, matches
  current taskbar behaviour) vs `ActivationPolicy::Accessory`. Default assumption: keep Dock icon
  for v1; note as UX follow-up.
- [NON-BLOCKING] **Single cross-platform config vs per-OS.** Recommend single `tauri.conf.json`
  with an added `bundle.macOS` section + `app.macOSPrivateApi: true`; no per-OS config file.
- [NON-BLOCKING] **Min macOS version / arch.** Confirm target (Apple Silicon vs Intel vs
  universal) for the `bundle.macOS.minimumSystemVersion` and the build `--target`. Default
  assumption: build for the host arch; document universal as a follow-up.

> First step below is "resolve the BLOCKING decision" (Keychain read mechanism). Unit B assumes
> **Option A** (shell out to `security`) unless the user says otherwise; if Option B is chosen, the
> file scope is identical and only the implementation inside `credential_source.rs` (+ Cargo.toml
> macOS dep) changes.

## Plan

Grouped by area, ordered. **[MUST]** = needed for macOS functional correctness; **[NICE]** =
optional polish / distribution.

### A. Credentials — Keychain read path  **[MUST]** (highest risk)
1. In `src-tauri/src/credential_source.rs`: add a macOS Keychain read path. Keep current file
   resolution as the **first** attempt (handles the SSH-style file + the env override), then on
   macOS fall back to reading the Keychain when no file is found. — files:
   `src-tauri/src/credential_source.rs`, skill: `auth-security`.
   - Refactor: split today's "read file → string" so parsing is shared. Add a macOS-gated
     `fn read_keychain_json() -> Result<String>` that (Option A) runs
     `security find-generic-password -s 'Claude Code-credentials' -w` and returns stdout trimmed,
     then feed that string into the **existing** serde parse + expiry logic (do NOT duplicate
     parsing). The Keychain secret value is the same `{"claudeAiOauth": {...}}` JSON.
   - `read_credentials()` flow on macOS becomes: try `credentials_path()` file; if `None`/unread,
     try `read_keychain_json()`; parse whichever succeeded. On Windows/Linux behaviour is
     unchanged (`#[cfg(target_os = "macos")]` gate around the Keychain branch only).
   - Pitfalls: use `cfg(target_os = "macos")` (not `cfg(unix)` — Linux uses the file, not
     Keychain). Trim trailing newline from `security` stdout. Treat empty/locked-Keychain output
     as "not found" and fall through to the same AuthExpired path the poller already handles
     (poller.rs:154-166 already degrades gracefully). Never log the token value. Do NOT write or
     modify the Keychain item — read-only, matching the file guarantee in the README disclaimer.
2. Add Rust unit test(s) that exercise the parsing path with a Keychain-shaped JSON string
   (reuse the existing fixtures in `credential_source.rs` tests, lines 119-174) so the shared
   parse path is covered without touching the real Keychain in CI. — files:
   `src-tauri/src/credential_source.rs` (test module), skill: `testing-qa`.

### B. Window / transparency on macOS  **[MUST]**
3. In `src-tauri/tauri.conf.json`: enable the Tauri **private API** so `transparent: true` works on
   macOS. Add `"macOSPrivateApi": true` under `app`. Without it macOS transparency is not applied
   and the overlay renders opaque. — files: `src-tauri/tauri.conf.json`, skill: (none — Tauri config).
   - Pitfall: `macOSPrivateApi` uses Apple private APIs → an app that uses it **cannot be submitted
     to the Mac App Store** (fine here; distribution is DMG/Developer-ID, not MAS). Note this in the
     decisions/README.
   - Verify `decorations: false` + `transparent: true` together: on macOS this yields a borderless
     transparent window; the CSS `#app` background already controls the card look. No traffic-light
     buttons appear with `decorations: false`. `shadow: false` is honoured.
4. (Already correct — no change) `alwaysOnTop`, `resizable`, `focus`, `skipTaskbar` behave on macOS.
   Note: `skipTaskbar` maps to "skip the app switcher / Dock" semantics differently on macOS; it is
   currently `false`, so no surprise. Document that drag-to-move relies on the existing
   `core:window:allow-start-dragging` capability (capabilities/default.json:8) which is OS-neutral.

### C. Bundle / distribution config  **[MUST for build], [NICE for signed distribution]**
5. In `src-tauri/tauri.conf.json`: add a `bundle.macOS` section. **[MUST]** minimal:
   `"macOS": { "minimumSystemVersion": "<decided, e.g. 11.0>" }`. Keep `targets: "all"` (yields
   `.app` + `.dmg` on macOS). Icons already include `icon.icns` (present) — no icon change. — files:
   `src-tauri/tauri.conf.json`.
6. **[NICE]** For Developer-ID signed + notarized distribution, add to `bundle.macOS`:
   `signingIdentity` (or rely on `APPLE_SIGNING_IDENTITY` env) and configure notarization via the
   Tauri-supported env vars (`APPLE_ID`, `APPLE_PASSWORD`/app-specific password or
   `APPLE_API_KEY`/`APPLE_API_ISSUER`). Do this only if distribution is in scope. — files:
   `src-tauri/tauri.conf.json` (+ CI secrets if a workflow is added later), skill:
   `cicd-github-actions` (only if a macOS build workflow is wanted).

### D. Native idle detection on macOS  **[NICE]**
7. **[NICE]** Replace the `#[cfg(not(target_os = "windows"))]` `false` stub in
   `src-tauri/src/poller.rs` (lines 77-78) with a `#[cfg(target_os = "macos")]` arm that returns
   real idle seconds and compares against `IDLE_PAUSE`, plus keep a `#[cfg(all(not(windows),
   not(macos)))]` `false` arm for Linux. Use CoreGraphics
   `CGEventSourceSecondsSinceLastEventType(.combinedSessionState, kCGAnyInputEventType)` or IOKit
   `HIDIdleTime`. — files: `src-tauri/src/poller.rs`, and a
   `[target.'cfg(target_os = "macos")'.dependencies]` block in `src-tauri/Cargo.toml` (e.g.
   `core-graphics` or `core-foundation` + IOKit binding). skill: (none).
   - Pitfall: keep the function signature and the `IDLE_PAUSE > 0` gate identical so poller logic
     (poller.rs:129-133) is untouched. Three-way cfg must be exhaustive so Linux still compiles.

### E. Documentation  **[MUST]**
8. In `README.md`: add macOS Prerequisites + Quick Start. — files: `README.md`.
   - Prerequisites: Xcode Command Line Tools (`xcode-select --install`), Rust via `rustup`,
     Node + pnpm (same as Windows). No WebView2 (macOS uses system WKWebView).
   - Build/run: same `pnpm install` / `pnpm tauri:dev` / `pnpm tauri:build`; output is
     `src-tauri/target/release/bundle/dmg/*.dmg` and `.../macos/*.app` (vs the NSIS path on
     Windows).
   - First Run: explain the macOS credential source = Keychain (`Claude Code-credentials`), the
     one-time Keychain access prompt, and the `~/.claude/.credentials.json` file fallback. Update
     the disclaimer note that on macOS the token is read from Keychain (still read-only).
   - Update Roadmap line 181 (`[ ] macOS port`) to checked once delivered.
   - Note that `macOSPrivateApi: true` precludes Mac App Store submission.

## Tests required (per testing-qa)

- Rust unit test in `credential_source.rs`: feed a Keychain-shaped JSON string through the shared
  parse + expiry path; assert token extraction and expiry detection (mirrors existing tests
  119-174). Must not touch the real Keychain (no live `security` call in tests — factor the parse
  step so it takes a `&str`).
- `cd src-tauri && cargo test` must pass on both Windows (existing) and macOS (CI/host) — verifies
  the cfg-gated code compiles on each target. Confirm a `cargo check --target` (or build) on macOS
  compiles all three idle arms if Unit D is done.
- Frontend unchanged: `pnpm exec tsc --noEmit` and `pnpm build` must remain green (no UI changes).
- Manual macOS smoke test = the Verification section below.

## Risks & rollback

- **Keychain prompt / locked Keychain (highest risk).** First read may prompt; a locked or
  inaccessible Keychain (e.g. headless/SSH) yields no token → app shows AuthExpired but still works
  via the existing JSONL fallback after 3 errors (poller.rs:311-338). Mitigation: file fallback
  first, then Keychain; clear error messaging. Rollback: the Keychain branch is `cfg(macos)` and
  additive — removing it reverts macOS to file-only with zero impact on Windows/Linux.
- **Transparency private API.** `macOSPrivateApi` is additive in config; if it ever breaks a Tauri
  upgrade, revert the one key. Precludes MAS (accepted).
- **Native idle (Unit D)** adds a macOS dep; if it misbehaves, revert to the `false` stub — purely
  a polish regression (polls during idle), no correctness loss.
- **Signing/notarization** is opt-in config + env; absent it, unsigned `.app` runs locally with a
  Gatekeeper right-click-open. No Windows impact.
- All changes are `cfg`-gated or macOS-only config keys → **zero risk to the working Windows build**.

## Work units (for parallel developer subagents)

- [x] Unit A — status: done — backend — agent: developer-backend — feature/part:
  macOS Keychain credential read path (BLOCKING decision: Option A shell-out unless told otherwise)
  - files (disjoint): `src-tauri/src/credential_source.rs`, and the
    `[target.'cfg(target_os = "macos")'.dependencies]` block in `src-tauri/Cargo.toml` **only if
    Option B (security-framework) is chosen**
  - depends on: BLOCKING decision (Keychain vs file mechanism)
  - skill: `auth-security`, `testing-qa`
- [x] Unit B — status: done — backend — agent: developer-backend — feature/part:
  macOS window transparency + bundle config (`app.macOSPrivateApi`, `bundle.macOS`)
  - files (disjoint): `src-tauri/tauri.conf.json`
  - depends on: decision on `minimumSystemVersion`; signing keys only if distribution in scope
  - skill: (none)
- [ ] Unit C — status: pending — backend — agent: developer-backend — feature/part:
  **[NICE]** native macOS idle detection in poller + macOS Cargo dep
  - files (disjoint): `src-tauri/src/poller.rs`; and the macOS dependencies block in
    `src-tauri/Cargo.toml` — **CONFLICTS with Unit A's Cargo.toml edit if Option B chosen.** If
    both touch Cargo.toml, sequence: Unit A first, then Unit C edits the same block; or assign the
    whole `Cargo.toml` macOS block to Unit C and have Unit A request its dep via Unit C. See schedule.
  - depends on: NON-BLOCKING decision (implement native idle?)
  - skill: (none)
- [x] Unit D — status: done — docs — agent: developer-frontend — feature/part:
  README macOS prerequisites / build / first-run / roadmap
  - files (disjoint): `README.md`
  - depends on: outcomes of A/B (to describe credential source + transparency note accurately);
    can draft in parallel and finalize after A/B land
  - skill: (none)

## Contracts (seams between units)

- No HTTP/DTO contract changes — this is a platform port; `usage_client.rs`, `model.rs`, and the
  `usage://snapshot` event payload are unchanged.
- Internal seam: `credential_source::read_credentials() -> Result<ResolvedCredentials>` keeps its
  **exact signature** (poller.rs:154 depends on it). Unit A changes only the resolution internals.
- `poller::is_system_idle_or_locked() -> bool` keeps its signature (poller.rs:129). Unit C changes
  only the macOS arm.
- `Cargo.toml` macOS-deps block is the one potential write-collision point (Units A-OptionB and C).
  Default: only Unit C owns it (Unit A uses Option A / no new dep) → no collision.

## Parallel schedule

- Gate: resolve the **BLOCKING** Keychain-mechanism decision before Unit A starts coding.
- Concurrent wave 1 (independent file sets): **Unit A** (credential_source.rs), **Unit B**
  (tauri.conf.json), **Unit C** (poller.rs) — all disjoint **iff** Unit A uses Option A (no
  Cargo.toml edit). If Option B is chosen, run Unit A before Unit C (shared Cargo.toml macOS block)
  or fold the dep add into Unit C.
- **Unit D** (README) drafts concurrently, finalizes after A and B land so it accurately documents
  the credential source and the transparency/MAS note.
- Build/verify on a macOS host after wave 1 completes.

## Suggested follow-ups (out of scope)

- GitHub Actions macOS build/notarize workflow (skill: cicd-github-actions).
- Universal (Intel + Apple Silicon) binary via `--target universal-apple-darwin`.
- `ActivationPolicy::Accessory` to hide the Dock icon for a true menu-bar overlay.
- Persisting window size/position cross-platform (already noted in README roadmap).
