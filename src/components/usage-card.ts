/**
 * Main usage card component.
 * Renders the overlay card from a UsageSnapshot: header, bars, footer.
 * Uses differential DOM updates to avoid full repaints.
 */

import { invoke } from '@tauri-apps/api/core';
import { getCurrentWindow } from '@tauri-apps/api/window';
import type { UsageSnapshot } from '../store.ts';
import { createWindowBar, updateWindowBar } from './window-bar.ts';
import { CountdownManager } from '../countdown.ts';
import { renderOverusageSection } from './overusage-section.ts';
import { open as openSettings } from './settings-panel.ts';

const countdownMgr = new CountdownManager();
countdownMgr.start();

// ── SVG icons for header buttons ─────────────────────────────────────────────

const REFRESH_SVG = `<svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
  <polyline points="23 4 23 10 17 10"></polyline>
  <polyline points="1 20 1 14 7 14"></polyline>
  <path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15"></path>
</svg>`;

const GEAR_SVG = `<svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
  <circle cx="12" cy="12" r="3"></circle>
  <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"></path>
</svg>`;

// ── Rate-limited state for the header refresh button ─────────────────────────

let _headerRefreshRateLimited = false;

/**
 * Update the header refresh button's disabled state.
 * Called from main.ts whenever a new snapshot arrives.
 */
export function setHeaderRefreshRateLimited(limited: boolean): void {
  _headerRefreshRateLimited = limited;
  const btn = document.getElementById('header-refresh') as HTMLButtonElement | null;
  if (!btn) return;
  if (limited) {
    btn.classList.add('disabled');
    btn.setAttribute('disabled', '');
    btn.title = 'Rate limited — cannot refresh';
  } else {
    btn.classList.remove('disabled');
    btn.removeAttribute('disabled');
    btn.title = 'Refresh now';
  }
}

/** Build the initial card shell (called once). */
export function buildCard(): HTMLElement {
  const card = document.createElement('div');
  card.className = 'overlay-card';
  card.innerHTML = `
    <div class="card-header">
      <span class="plan-badge" id="plan-badge">—</span>
      <span class="account-name" id="account-name"></span>
      <div class="header-right-cluster">
        <button class="header-icon-btn" id="header-refresh" title="Refresh now" aria-label="Refresh now">${REFRESH_SVG}</button>
        <button class="header-icon-btn" id="header-settings" title="Settings" aria-label="Settings">${GEAR_SVG}</button>
        <span class="status-dot loading" id="status-dot" title="Loading…"></span>
      </div>
    </div>
    <div class="card-body" id="card-body">
      <div class="state-message">
        <span class="icon">⏳</span>
        <span>Loading usage data…</span>
      </div>
    </div>
    <div class="card-footer" id="card-footer" style="display:none"></div>
    <div class="app-version"></div>
  `;

  // ── Header button event listeners ─────────────────────────────────────────
  const refreshBtn = card.querySelector<HTMLButtonElement>('#header-refresh');
  refreshBtn?.addEventListener('click', () => {
    if (_headerRefreshRateLimited) return;
    invoke('request_refresh').catch(console.error);
  });

  const settingsBtn = card.querySelector<HTMLButtonElement>('#header-settings');
  settingsBtn?.addEventListener('click', () => {
    openSettings().catch(console.error);
  });

  // Drag-to-move: left-click drag anywhere on card except interactive elements.
  card.addEventListener('mousedown', async (e: MouseEvent) => {
    if (e.button !== 0) return;
    const target = e.target as HTMLElement;
    // Don't drag when clicking interactive elements — use closest() so clicks
    // landing on inner <svg>/<path> nodes inside a button are also ignored.
    if (
      target.tagName === 'INPUT' ||
      target.closest('button') !== null ||
      target.closest('#context-menu') !== null
    ) {
      return;
    }
    await getCurrentWindow().startDragging();
  });

  return card;
}

/** Update the card in-place with new snapshot data. */
export function renderSnapshot(card: HTMLElement, snap: UsageSnapshot): void {
  const badge = card.querySelector<HTMLElement>('#plan-badge');
  const accountEl = card.querySelector<HTMLElement>('#account-name');
  const dot = card.querySelector<HTMLElement>('#status-dot');
  const body = card.querySelector<HTMLElement>('#card-body');
  const footer = card.querySelector<HTMLElement>('#card-footer');

  if (!badge || !dot || !body || !footer || !accountEl) return;

  // ── Plan badge ────────────────────────────────────────────────────────────
  const planLabel = planDisplayLabel(snap.plan);
  badge.textContent = planLabel;
  badge.className = 'plan-badge ' + planBadgeClass(snap.plan);

  // ── Account name ──────────────────────────────────────────────────────────
  accountEl.textContent =
    snap.profile?.display_name ?? snap.profile?.email ?? '';

  // ── Status dot ────────────────────────────────────────────────────────────
  const { dotClass, dotTitle } = statusDotInfo(snap.status);
  dot.className = 'status-dot ' + dotClass;
  dot.title = dotTitle;

  // ── Body ──────────────────────────────────────────────────────────────────
  const statusType = snap.status.type;

  if (statusType === 'loading') {
    body.innerHTML = `
      <div class="state-message">
        <span class="icon">⏳</span>
        <span>Loading usage data…</span>
      </div>`;
    footer.style.display = 'none';
    return;
  }

  if (statusType === 'auth_expired') {
    body.innerHTML = `
      <div class="state-message">
        <span class="icon">🔑</span>
        <span>Sign in to Claude Code<br>to see usage data.</span>
      </div>`;
    footer.style.display = 'none';
    return;
  }

  if (snap.windows.length === 0) {
    body.innerHTML = `
      <div class="state-message">
        <span class="icon">📊</span>
        <span>Waiting for Claude usage…</span>
      </div>`;
    footer.style.display = 'none';
    return;
  }

  // ── Quota bars (differential update) ─────────────────────────────────────
  const isDegraded = statusType === 'degraded';
  const existingBars = new Map<string, HTMLElement>();
  for (const child of Array.from(body.children)) {
    const el = child as HTMLElement;
    const key = el.dataset.key;
    if (key) existingBars.set(key, el);
  }

  // Remove bars that are no longer in the snapshot
  const newKeys = new Set(snap.windows.map((w) => w.key));
  for (const [key, el] of existingBars) {
    if (!newKeys.has(key)) el.remove();
  }

  // Update or create bars in order, collect all in orderedEls.
  countdownMgr.clear();
  const orderedEls: HTMLElement[] = [];

  for (const w of snap.windows) {
    let el: HTMLElement;
    if (existingBars.has(w.key)) {
      el = existingBars.get(w.key)!;
      updateWindowBar(el, w, isDegraded);
    } else {
      el = createWindowBar(w, isDegraded);
    }
    orderedEls.push(el);

    // Register countdown element for 1s ticker
    const countdownEl = el.querySelector<HTMLElement>('.quota-countdown');
    if (countdownEl) countdownMgr.register(w.key, countdownEl);
  }

  // Clear and re-append in canonical order (handles reordering and new/removed bars).
  body.innerHTML = '';
  for (const el of orderedEls) {
    body.appendChild(el);
  }

  // Sync countdown timestamps with the latest snapshot data.
  countdownMgr.updateTimestamps(snap.windows);

  // ── Stale indicator ───────────────────────────────────────────────────────
  const existingStale = body.querySelector('.stale-info');
  if (statusType === 'stale') {
    const detail = (snap.status as { type: 'stale'; detail: string }).detail;
    if (existingStale) {
      existingStale.textContent = `⚠ ${detail}`;
    } else {
      const staleEl = document.createElement('div');
      staleEl.className = 'stale-info';
      staleEl.textContent = `⚠ ${detail}`;
      body.appendChild(staleEl);
    }
  } else {
    existingStale?.remove();
  }

  // ── Footer (extra usage) ──────────────────────────────────────────────────
  // Render whenever extra_usage is present in the snapshot — even when disabled —
  // so the on/off state is always visible. The early-returns above for loading /
  // auth_expired / no-windows already hide the footer in those branches.
  //
  // INVARIANT (issue 3): overusage is rendered into #card-footer so it is always
  // the last/bottom element of the card, independent of plan or subscription state.
  // All quota bars render into #card-body (flex:1, scrollable); #card-footer is a
  // separate flex-shrink:0 sibling that always sits below #card-body in the column
  // flex layout (.overlay-card { flex-direction: column }).  No reordering is needed.
  if (snap.extra_usage) {
    renderOverusageSection(footer, snap.extra_usage, snap.profile);
    footer.style.display = 'block';
  } else {
    footer.style.display = 'none';
  }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

function planDisplayLabel(plan: string): string {
  switch (plan) {
    case 'free': return 'FREE';
    case 'pro': return 'PRO';
    case 'max5x': return 'MAX 5×';
    case 'max20x': return 'MAX 20×';
    case 'max': return 'MAX';
    default: return '—';
  }
}

function planBadgeClass(plan: string): string {
  if (plan === 'free') return 'free';
  if (plan === 'pro') return 'pro';
  if (plan.startsWith('max')) return 'max';
  return '';
}

function statusDotInfo(status: UsageSnapshot['status']): { dotClass: string; dotTitle: string } {
  switch (status.type) {
    case 'live': return { dotClass: 'live', dotTitle: 'Live data' };
    case 'stale': return { dotClass: 'stale', dotTitle: `Stale: ${status.detail}` };
    case 'degraded': return { dotClass: 'degraded', dotTitle: 'Degraded — local estimate' };
    case 'auth_expired': return { dotClass: 'expired', dotTitle: 'Auth expired' };
    case 'loading': return { dotClass: 'loading', dotTitle: 'Loading…' };
    case 'error': return { dotClass: 'error', dotTitle: `Error: ${status.detail}` };
    default: return { dotClass: 'loading', dotTitle: '' };
  }
}
