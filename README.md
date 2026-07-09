# Claude Overlay

> **⚠ Unofficial project — read the disclaimer below before use.**

A lightweight, always-on-top desktop widget that displays live Claude subscription usage: plan
type, 5-hour session window, weekly quotas, model-specific limits, and reset countdowns.

Supported platforms: **Windows 11**, **macOS 11.0+ (Big Sur)**, and **Linux (x86_64)**.

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
  signed updates (NSIS installer on Windows, DMG on macOS, AppImage on Linux), and falls back
  to opening the release page if a silent install is blocked or unsupported (Linux `.deb`/`.rpm`
  installs always update via the releases page). Running version is shown in the overlay's
  bottom-right corner
- Graceful degradation: falls back to local JSONL transcript estimates when the API is unavailable

---

## Screenshots

Available in the **[screenshots/](./screenshots/)** folder.

---

## Install

Download the latest build for your platform from the
**[GitHub Releases page](https://github.com/RoienE/claude-overlay/releases)**.

These builds are **not code-signed or notarized**, so Windows and macOS will show a one-time
security warning the first time you launch — the per-platform steps below explain how to get
past it.

### Windows

1. Download `claude-overlay_<version>_x64-setup.exe` (NSIS installer).
2. Run it. Windows SmartScreen may show a **"Windows protected your PC"** dialog because the
   build is unsigned — click **More info → Run anyway** to continue.
3. Follow the installer.

### macOS

1. Download `claude-overlay_<version>_universal.dmg` and open it, then drag the app to
   **Applications**.
2. Because the build is unsigned, macOS Gatekeeper blocks it on first launch. Bypass it with
   either approach:
   - **Right-click** the app in Finder and choose **Open**, then confirm in the dialog.
   - Or from Terminal:
     ```sh
     xattr -dr com.apple.quarantine /path/to/claude-overlay.app
     ```
3. Requires **macOS 11.0 (Big Sur)** or newer.

### Linux

Pick the package that fits your distro:

- **AppImage (recommended)** — download the `.AppImage`, then:
  ```sh
  chmod +x claude-overlay_*.AppImage
  ./claude-overlay_*.AppImage
  ```
  The AppImage auto-updates, but requires **glibc 2.39+** (Ubuntu 24.04+, Fedora 40+,
  Debian 13+, or equivalent).
- **Debian/Ubuntu (`.deb`)**:
  ```sh
  sudo apt install ./claude-overlay_*.deb
  ```
- **Fedora/RHEL (`.rpm`)**:
  ```sh
  sudo dnf install ./claude-overlay-*.rpm
  ```

> **Note:** the `.deb` and `.rpm` packages do **not** auto-update — grab a newer package from
> the releases page when one is available. Only the AppImage self-updates.

---

## First Run & Usage

The app reads Claude Code's OAuth token automatically. If Claude Code is installed and logged
in, the widget shows your live usage with no configuration.

### Windows

The app reads `%USERPROFILE%\.claude\.credentials.json` automatically (same file Claude Code
uses). No configuration needed — if Claude Code is logged in, the widget shows your live usage.

### macOS

**Credentials:** The app reads the OAuth token from the macOS **Keychain** (service name
`Claude Code-credentials`) — the same location Claude Code stores it on a normal macOS install.
On first credential read, macOS displays a one-time "allow access" prompt for the app; click
**Allow**. The app never writes to or modifies the Keychain item.

If you have a `~/.claude/.credentials.json` file (e.g. from an SSH/headless setup), the app
tries that file first and only falls back to the Keychain when the file is absent.

### Linux

**Credentials:** the app reads `~/.claude/.credentials.json` automatically (same file
Claude Code uses on Linux). No configuration needed — if Claude Code is logged in, the
widget shows your live usage. If Claude Code uses a non-default config directory, set
`CLAUDE_CONFIG_DIR` before launching and the app will follow it.

**Runtime caveats on Linux:**

- **Tray icon on vanilla GNOME:** GNOME Shell doesn't render AppIndicator tray icons
  without the **AppIndicator/KStatusNotifierItem** Shell extension installed and enabled;
  the `.deb`/`.rpm` packages pull in `libayatana-appindicator3`, but the GNOME Shell
  extension itself is a separate, user-side install.
- **Transparency:** the translucent overlay card requires a compositor (works out of the
  box on GNOME/KDE); bare X11 window managers without a compositor may render it opaque.
- **Always-on-top / taskbar hiding:** `alwaysOnTop` and `skipTaskbar` are window-manager
  hints, not guarantees — not every WM honors them, and behavior varies further on
  Wayland.
- **Idle-pause:** the idle-pause-on-lock polling optimization is Windows-only for now; on
  Linux (and macOS) the widget keeps polling at its normal cadence and never pauses on
  idle.
- **AppImage glibc floor:** the AppImage is built on Ubuntu 24.04 and requires glibc
  2.39+ (Ubuntu 24.04+, Fedora 40+, Debian 13+, or equivalent). Older distros should use
  the `.deb`/`.rpm` package for their release instead.

---

## Privacy / Telemetry

The app collects **anonymous, operational data** to help the maintainer understand how many
installs are active, which versions are running, and how often API rate-limit errors occur.

**What is sent:**

| Field | Example | Purpose |
|---|---|---|
| `install_id` | random UUID v4 | Distinguish installs (not reversible to a user) |
| `event` | `heartbeat` / `install` / `rate_limit_hit` | Event type |
| `app_version` | `0.8.0` | Version distribution |
| `os` | `windows` / `macos` / `linux` | Platform split |
| `arch` | `x86_64` / `aarch64` | Architecture split |

**What is never sent:** OAuth tokens, account info, profile data, plan or subscription
details, usage numbers, file paths, hostnames, usernames, or any machine-derived identifier.
The `install_id` is a randomly-generated UUID — it is not derived from your machine, account,
or any personal attribute and cannot be reversed to identify you.

**Default:** telemetry is **on by default** (opt-out). You can turn it off at any time under
**Settings → Privacy** — the change takes effect immediately without restarting the app and
survives updates.

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

## License

MIT

---

## Contributing / building from source

Want to build the app yourself or contribute? See **[CONTRIBUTING.md](./CONTRIBUTING.md)** for
prerequisites, build steps, architecture, and development commands.
