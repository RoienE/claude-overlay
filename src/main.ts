/**
 * Bootstrap: subscribe to Tauri events, render the usage card, wire up context menu.
 */

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { store, type UsageSnapshot, type Settings } from './store.ts';
import { buildCard, renderSnapshot, setHeaderRefreshRateLimited } from './components/usage-card.ts';
import {
  init as initContextMenu,
  setRateLimited,
  setOpacityCallback,
  setCurrentOpacity,
} from './components/context-menu.ts';
import {
  init as initSettingsPanel,
  setCurrentSettings,
} from './components/settings-panel.ts';
import { init as initSessionsPanel } from './components/sessions-panel.ts';
import { init as initVersionLabel } from './components/version-label.ts';
import { initUpdater, checkForUpdates } from './updater.ts';

async function main(): Promise<void> {
  const appEl = document.getElementById('app');
  if (!appEl) throw new Error('Missing #app element');

  // ── Build the overlay card ─────────────────────────────────────────────────
  const card = buildCard();
  appEl.appendChild(card);

  // ── Wire up context menu ───────────────────────────────────────────────────
  initContextMenu();

  // ── Wire up settings panel ────────────────────────────────────────────────
  initSettingsPanel();

  // ── Wire up sessions panel ────────────────────────────────────────────────
  initSessionsPanel();

  // ── Version label (persists across overlay and settings views) ────────────
  void initVersionLabel();

  // ── Auto-updater: startup check + periodic background check ───────────────
  initUpdater();
  checkForUpdates({ interactive: false }).catch(() => {});
  setInterval(() => {
    checkForUpdates({ interactive: false }).catch(() => {});
  }, 2 * 60 * 60 * 1000); // every 2 hours

  // Provide the opacity change callback so the slider updates the card immediately.
  setOpacityCallback((val: number) => {
    appEl.style.opacity = String(val);
  });

  // ── Apply persisted settings at startup ────────────────────────────────────
  // Fetch saved settings from the Rust backend and apply opacity so the overlay
  // starts at the persisted value rather than the CSS default (0.92).
  invoke<Settings>('get_settings')
    .then((settings) => {
      appEl.style.opacity = String(settings.opacity);
      // Tell the context menu so the slider opens at the right position.
      setCurrentOpacity(settings.opacity);
      // Preselect settings panel controls with the persisted values.
      // Degrade gracefully: if the backend hasn't added size_preset / plan_override yet,
      // the fields will be undefined — fall back to safe defaults.
      setCurrentSettings({
        opacity: settings.opacity,
        size_preset: settings.size_preset ?? 'default',
        plan_override: settings.plan_override ?? null,
        history_threshold_mins: settings.history_threshold_mins ?? 30,
      });
    })
    .catch((err) => {
      // Non-fatal: CSS default opacity remains in effect.
      console.warn('Failed to load settings:', err);
    });

  // ── Subscribe store → render (single render path) ─────────────────────────
  store.subscribe((snap) => {
    renderSnapshot(card, snap);
  });

  // ── Subscribe to Tauri snapshot events → store ─────────────────────────────
  await listen<UsageSnapshot>('usage://snapshot', (event) => {
    const snap = event.payload;

    // Compute rate-limited flag and notify all surfaces that reflect it (C3).
    const isRateLimited =
      snap.status.type === 'stale' &&
      'detail' in snap.status &&
      (snap.status as { type: string; detail: string }).detail
        .toLowerCase()
        .includes('rate limit');
    setRateLimited(isRateLimited);             // context menu refresh item
    setHeaderRefreshRateLimited(isRateLimited); // header refresh button

    // Updating the store triggers re-render via the subscription above.
    store.set(snap);
  });

  // ── Tray "Check for Updates" → interactive check ──────────────────────────
  await listen('updater://check-requested', () => {
    checkForUpdates({ interactive: true }).catch(() => {});
  });
}

main().catch(console.error);
