/**
 * Bootstrap: subscribe to Tauri events, render the usage card, wire up context menu.
 */

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { store, type UsageSnapshot, type Settings } from './store.ts';
import { buildCard, renderSnapshot } from './components/usage-card.ts';
import {
  init as initContextMenu,
  setRateLimited,
  setOpacityCallback,
  setCurrentOpacity,
} from './components/context-menu.ts';

async function main(): Promise<void> {
  const appEl = document.getElementById('app');
  if (!appEl) throw new Error('Missing #app element');

  // ── Build the overlay card ─────────────────────────────────────────────────
  const card = buildCard();
  appEl.appendChild(card);

  // ── Wire up context menu ───────────────────────────────────────────────────
  initContextMenu();

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
      // Also tell the context menu so the slider opens at the right position.
      setCurrentOpacity(settings.opacity);
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

    // Update rate-limited status for context menu
    const isRateLimited =
      snap.status.type === 'stale' &&
      'detail' in snap.status &&
      (snap.status as { type: string; detail: string }).detail
        .toLowerCase()
        .includes('rate limit');
    setRateLimited(isRateLimited);

    // Updating the store triggers re-render via the subscription above.
    store.set(snap);
  });
}

main().catch(console.error);
