# Contributing to Claude Overlay

Thanks for your interest in building or improving Claude Overlay. This guide covers the
toolchain, build steps, project architecture, and development workflow. If you just want to
download and run the app, see the **[README](./README.md)** instead.

---

## Prerequisites

**Required on the build machine (all must be present before building):**

### Windows

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

### macOS

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

### Linux

1. **Rust toolchain (stable)** — install from https://rustup.rs
   ```sh
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   rustup update stable
   ```
2. **Node.js v18+** — https://nodejs.org (v22 LTS recommended), plus **pnpm**
   (`npm i -g pnpm` or `corepack enable pnpm`).
3. **Tauri v2 prerequisites for Linux** (Debian/Ubuntu package names; adjust for your
   distro) — WebKitGTK, build tools, and the libraries needed to bundle `.deb`/`.rpm`/
   `.AppImage`:
   ```sh
   sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file libxdo-dev \
     libssl-dev libayatana-appindicator3-dev librsvg2-dev patchelf xdg-utils
   ```
4. **Tauri CLI v2** — installed automatically via `pnpm install` (devDependency; no
   global install needed).
5. A working **Claude Code** installation, logged in (supplies the OAuth token at
   `~/.claude/.credentials.json`, same as on Windows; the `CLAUDE_CONFIG_DIR`
   environment variable is respected if you use a non-default config location).

> **Cannot compile Rust?** — The frontend (TypeScript/Vite) can be built and verified
> independently on any platform with `pnpm install && pnpm build`.

---

## Build & Run

The build commands are identical on Windows, macOS, and Linux:

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
| Linux | `src-tauri/target/release/bundle/deb/*.deb`, `bundle/rpm/*.rpm`, and `bundle/appimage/*.AppImage` |

> **First `cargo build` can take 5–10 minutes** — Rust compiles all dependencies
> from source. Subsequent builds are fast (incremental).

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

## Development

These commands work the same on Windows (PowerShell), macOS, and Linux (Terminal/bash/zsh):

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
- [ ] Linux idle detection (D-Bus/X11) — same idle-pause gap as macOS; polling never pauses on
      Linux today
- [ ] Signed and notarized macOS builds for distribution (requires Apple Developer ID; local/dev
      builds run unsigned with a Gatekeeper right-click-open workaround)
- [ ] Universal (Intel + Apple Silicon) macOS binary via `--target universal-apple-darwin`
- [ ] Token self-refresh (requires OAuth client flow — risky; deferred)
- [ ] Historical usage charts
