# Claude Overlay

> **⚠ Unofficial project — read the disclaimer below before use.**

A lightweight, always-on-top desktop widget that displays live Claude subscription usage: plan
type, 5-hour session window, weekly quotas, model-specific limits, and reset countdowns.

Supported platforms: **Windows 11** and **macOS 11.0+ (Big Sur)**.

Built with [Tauri 2](https://tauri.app) (Rust core + vanilla TypeScript WebView UI).

---

## Features

- Always-on-top, borderless, translucent overlay card
- Drag anywhere on the card to move; native resize edges
- Live quota bars (dynamic — shows whatever the API returns, including new quota types)
- 1-second countdown tickers for each window's reset time
- Adaptive polling: fast during active sessions, idle-paused when locked/idle
- Plan badge (Free / Pro / Max 5× / Max 20×) from the profile endpoint
- Right-click context menu: opacity slider, size presets, plan override, refresh, quit
- Settings page: opacity slider, size presets, check for updates, refresh, quit
- Tray icon + taskbar presence; click the tray icon to toggle show/hide
- Automatic updates: checks GitHub Releases on launch and every 2 hours, prompts to install
  signed updates (NSIS installer), and falls back to opening the release page if a silent
  install is blocked. Running version is shown in the overlay's bottom-right corner
- Graceful degradation: falls back to local JSONL transcript estimates when the API is unavailable

---

## Screenshots
Available in the **[screenshots/](./screenshots/)** folder.


## Quick Start

### Prerequisites

**Required on the build machine (all must be present before building):**

#### Windows

1. **Rust toolchain (stable)** — install from https://rustup.rs
   ```powershell
   winget install Rustlang.Rustup   # or visit rustup.rs
   rustup update stable
   ```
2. **Node.js v18+** — https://nodejs.org (v22 LTS recommended)
3. **Tauri v2 prerequisites for Windows:**
   - **Microsoft C++ Build Tools** (or Visual Studio with C++ workload) —
     required by the Rust `windows-sys` dependencies Tauri uses.
     Install via https://visualstudio.microsoft.com/visual-cpp-build-tools/
   - **WebView2 Runtime** — pre-installed on Windows 11; if missing, download
     from https://developer.microsoft.com/en-us/microsoft-edge/webview2/
4. **Tauri CLI v2** — installed automatically via `pnpm install` (declared as a
   devDependency; no global install needed). **pnpm** is the package manager —
   install it with `npm i -g pnpm` or `corepack enable pnpm`.
5. A working **Claude Code** installation, logged in (supplies the OAuth token at
   `%USERPROFILE%\.claude\.credentials.json`)

#### macOS

1. **Xcode Command Line Tools** — required for the macOS build toolchain (C compiler,
   linker, SDK headers):
   ```sh
   xcode-select --install
   ```
2. **Rust toolchain (stable)** — install from https://rustup.rs
   ```sh
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   rustup update stable
   ```
3. **Node.js v18+** — https://nodejs.org (v22 LTS recommended). Install via the
   official installer, `nvm`, `fnm`, or Homebrew (`brew install node`).
4. **pnpm** — install with `npm i -g pnpm` or `corepack enable pnpm`.
5. **Tauri CLI v2** — installed automatically via `pnpm install` (devDependency; no
   global install needed). macOS uses the system **WKWebView** — no WebView2 required.
6. A working **Claude Code** installation, logged in. On macOS, Claude Code stores the
   OAuth token in the **macOS Keychain** under service name `Claude Code-credentials`
   (not a plaintext file). The app reads the token from the Keychain automatically; no
   manual configuration is needed.

> **Cannot compile Rust?** — The frontend (TypeScript/Vite) can be built and verified
> independently on any platform with `pnpm install && pnpm build`.

### Build & Run

The build commands are identical on Windows and macOS:

```sh
# 1. Install frontend dependencies (also installs the Tauri CLI)
pnpm install

# 2. Verify the frontend builds clean (TypeScript + Vite — no Rust needed)
pnpm build

# 3. Generate app icons (requires a 1024×1024 source PNG at src/assets/icon.png)
#    pnpm tauri icon src/assets/icon.png
#    OR manually place PNG/ICO icons in icons/ folder

# 4. Development mode — starts Vite dev server + Tauri hot-reload (requires Rust)
pnpm tauri:dev

# 5. Production build — compiles Rust + bundles frontend (requires Rust)
pnpm tauri:build
```

**Build output by platform:**

| Platform | Output location |
|---|---|
| Windows | `src-tauri/target/release/bundle/nsis/*.exe` |
| macOS | `src-tauri/target/release/bundle/macos/*.app` and `bundle/dmg/*.dmg` |

> **First `cargo build` can take 5–10 minutes** — Rust compiles all dependencies
> from source. Subsequent builds are fast (incremental).

### First Run

#### Windows

The app reads `%USERPROFILE%\.claude\.credentials.json` automatically (same file Claude Code
uses). No configuration needed — if Claude Code is logged in, the widget shows your live usage.

#### macOS

**Credentials:** The app reads the OAuth token from the macOS **Keychain** (service name
`Claude Code-credentials`) — the same location Claude Code stores it on a normal macOS install.
On first credential read, macOS displays a one-time "allow access" prompt for the app; click
**Allow**. The app never writes to or modifies the Keychain item.

If you have a `~/.claude/.credentials.json` file (e.g. from an SSH/headless setup), the app
tries that file first and only falls back to the Keychain when the file is absent.

**Gatekeeper (unsigned build):** This build is not code-signed or notarized. On first launch,
macOS Gatekeeper will block the app with a "can't be opened" message. To allow it, use either
approach:

- **Right-click** the `.app` in Finder and choose **Open**, then confirm in the dialog.
- Or from Terminal:
  ```sh
  xattr -dr com.apple.quarantine /path/to/claude-overlay.app
  ```

Subsequent launches open normally once you have allowed it once.

**Minimum supported macOS:** 11.0 (Big Sur).

---

## Configuration

All polling intervals and constants live in `src-tauri/src/config.rs`. The defaults are:

| Constant | Default | Meaning |
|---|---|---|
| `POLL_INTERVAL` | 180s | Standard cadence |
| `POLL_FAST` | 120s | While usage is rising |
| `POLL_FAST_EXTRA` | 2s | Burst after activity stops |
| `POLL_ERROR` | 30s | After network errors |
| `MAX_BACKOFF` | 900s | Max 429 backoff (15 min) |
| `IDLE_PAUSE` | 300s | Pause after 5 min OS idle |

---

## Architecture

```
Rust core (src-tauri/src/)
  config.rs           — all constants (endpoints, UA, intervals)
  credential_source.rs — read credentials: file on Windows/Linux; macOS Keychain fallback
  usage_client.rs      — GET /api/oauth/usage + /api/oauth/profile
  fallback_logs.rs     — aggregate ~/.claude/projects/**/*.jsonl
  plan_detector.rs     — classify plan; label & sort quota windows
  poller.rs            — adaptive polling loop; emits UsageSnapshot events
  window_ctl.rs        — Tauri commands (opacity, size, show/hide, etc.)
  model.rs             — shared data types

WebView UI (src/)
  main.ts              — bootstrap, Tauri event subscription
  store.ts             — snapshot state store
  countdown.ts         — 1-second local countdown tickers
  updater.ts           — auto-update check + install via GitHub Releases (tauri-plugin-updater)
  components/
    usage-card.ts      — full card renderer (differential DOM updates)
    window-bar.ts      — single quota bar + countdown row
    context-menu.ts    — right-click menu (opacity, size, plan override, quit)
    settings-panel.ts  — settings view (opacity, size, check for updates, quit)
    version-label.ts   — version badge in the overlay/settings footer
```

---

## ⚠ Disclaimer — Unofficial Endpoint / ToS Note

**This application uses undocumented, unofficial Anthropic API endpoints:**

- `GET https://api.anthropic.com/api/oauth/usage`
- `GET https://api.anthropic.com/api/oauth/profile`

These are the same endpoints that Claude Code and the claude.ai dashboard use internally.
**They are not part of any public Anthropic API contract** and may change, break, or be
rate-limited without notice.

Additionally, the app **reads the OAuth access token from Claude Code's credential store**:
on Windows and Linux this is `~/.claude/.credentials.json`; on macOS this is the **macOS
Keychain** (service name `Claude Code-credentials`), with the file as a fallback. This is
intentional, read-only, and mirrors data you already own and can see in Claude Code's `/usage`
command — but it constitutes access to another application's stored credentials.

**What this app does NOT do:**
- Consume inference tokens (all calls are metadata/quota checks)
- Write to or modify the credentials file or Keychain item
- Store, transmit, or share your credentials or usage data

**Risk assessment:** The design is deliberately isolated (one swappable `usage_client` module)
so that if Anthropic ships an official usage API, only that module changes. If Anthropic
considers this access a ToS concern or changes the endpoints, the fallback to local JSONL
transcript aggregation keeps the widget partially functional.

Use at your own discretion.

---

## Development

These commands work on both Windows (PowerShell) and macOS (Terminal/zsh):

```sh
# TypeScript type-check only (no Rust needed)
pnpm exec tsc --noEmit

# Frontend build (TypeScript + Vite bundle, no Rust needed)
pnpm build

# Rust unit tests (requires Rust toolchain)
cd src-tauri && cargo test

# Full dev run (requires Rust toolchain + all prerequisites above)
pnpm tauri:dev
```

Unit tests cover: plan detection mapping, quota-window sorting, label generation, countdown
formatting, and credential parsing.

---

## Roadmap

- [x] Persist opacity across restarts (settings.json in app config dir; size/position remain future work)
- [x] macOS port — Keychain credential source, transparent overlay via `macOSPrivateApi`, `.app`/`.dmg` bundle
- [x] Automatic updates from GitHub Releases — signed `tauri-plugin-updater`, in-app prompt, NSIS installer
- [ ] Native macOS idle detection (adaptive polling works on macOS, but the idle-pause feature that
      suspends polling after 5 min of OS inactivity is Windows-only for now)
- [ ] Signed and notarized macOS builds for distribution (requires Apple Developer ID; local/dev
      builds run unsigned with a Gatekeeper right-click-open workaround)
- [ ] Universal (Intel + Apple Silicon) macOS binary via `--target universal-apple-darwin`
- [ ] Token self-refresh (requires OAuth client flow — risky; deferred)
- [ ] Historical usage charts

---

## License

MIT
