/**
 * Right-click context menu.
 * Wires up opacity slider, size presets, plan override, refresh, and quit.
 *
 * Issue 5b — grow-then-restore:
 *   On contextmenu, the overlay window is temporarily grown to fit the menu (using
 *   get_window_size / set_window_size Tauri commands from Unit D).  The prior size is
 *   saved and restored in hide().  The CSS max-height/overflow-y safety net (issue 5c)
 *   ensures the menu is still scrollable even if the resize calls fail.
 *
 * Issue 6 — focus-loss dismissal:
 *   Clicks outside the tiny WebView never produce a DOM click event, so the existing
 *   outside-click handler can't close the menu.  We add getCurrentWindow().onFocusChanged
 *   plus a belt-and-braces window blur listener so the menu closes whenever the overlay
 *   loses OS focus.  A short guard (BLUR_GUARD_MS) prevents the transient blur that the
 *   window-resize itself may emit from immediately closing the menu.
 */

import { invoke } from '@tauri-apps/api/core';
import { getCurrentWindow } from '@tauri-apps/api/window';

export interface ContextMenuOptions {
  isRateLimited: boolean;
  currentOpacity: number;
  onOpacityChange: (v: number) => void;
}

let menuEl: HTMLElement | null = null;
let currentOptions: ContextMenuOptions = {
  isRateLimited: false,
  currentOpacity: 0.92,
  onOpacityChange: () => {},
};

// ── Grow-then-restore state (issue 5b) ───────────────────────────────────────
interface WindowSize { width: number; height: number; }

/** Size saved before we grew the window; restored in hide(). */
let savedSize: WindowSize | null = null;

/** Minimum dimensions required to show the full menu without scrolling. */
const MENU_MIN_W = 200;
const MENU_MIN_H = 360;

// ── Blur-guard (issue 6) ─────────────────────────────────────────────────────
/**
 * Milliseconds after menu-open during which blur/focus-loss is ignored.
 * Prevents the transient blur that the grow-resize emits from auto-closing
 * the menu immediately after it opens.
 */
const BLUR_GUARD_MS = 300;
let menuOpenedAt = 0;

/** Build the context menu DOM (once). */
function buildMenu(): HTMLElement {
  const menu = document.createElement('div');
  menu.className = 'context-menu';
  menu.id = 'context-menu';
  menu.innerHTML = buildMenuHtml();
  document.body.appendChild(menu);

  // Opacity slider
  const slider = menu.querySelector<HTMLInputElement>('#opacity-slider');
  if (slider) {
    slider.addEventListener('input', (e) => {
      const val = parseFloat((e.target as HTMLInputElement).value) / 100;
      currentOptions.onOpacityChange(val);
      // Apply directly to #app element for instant feedback
      const appEl = document.getElementById('app');
      if (appEl) appEl.style.opacity = String(val);
      invoke('set_opacity', { opacity: val }).catch(console.error);
    });
    // Prevent drag-to-move from triggering when using the slider
    slider.addEventListener('mousedown', (e) => e.stopPropagation());
  }

  // Size presets
  for (const preset of ['small', 'medium', 'large', 'default']) {
    const el = menu.querySelector<HTMLElement>(`[data-size="${preset}"]`);
    el?.addEventListener('click', () => {
      invoke('set_size_preset', { preset }).catch(console.error);
      hide();
    });
  }

  // Plan override
  for (const plan of ['auto', 'free', 'pro', 'max5x', 'max20x', 'max']) {
    const el = menu.querySelector<HTMLElement>(`[data-plan="${plan}"]`);
    el?.addEventListener('click', () => {
      invoke('set_plan_override', { plan: plan === 'auto' ? null : plan }).catch(console.error);
      hide();
    });
  }

  // Refresh now
  const refreshBtn = menu.querySelector<HTMLElement>('#menu-refresh');
  refreshBtn?.addEventListener('click', () => {
    if (currentOptions.isRateLimited) return;
    invoke('request_refresh').catch(console.error);
    hide();
  });

  // Quit
  const quitBtn = menu.querySelector<HTMLElement>('#menu-quit');
  quitBtn?.addEventListener('click', () => {
    invoke('quit_app').catch(console.error);
  });

  return menu;
}

function buildMenuHtml(): string {
  return `
    <div class="menu-label">Opacity</div>
    <div class="opacity-slider-wrap">
      <input type="range" id="opacity-slider" class="opacity-slider" min="20" max="100" value="92" />
      <div class="opacity-slider-label">Adjust transparency</div>
    </div>
    <div class="menu-separator"></div>
    <div class="menu-label">Size</div>
    <div class="menu-item" data-size="small">Small (220×160)</div>
    <div class="menu-item" data-size="medium">Medium (280×220)</div>
    <div class="menu-item" data-size="large">Large (340×280)</div>
    <div class="menu-item" data-size="default">Reset to default</div>
    <div class="menu-separator"></div>
    <div class="menu-label">Plan Override</div>
    <div class="menu-item" data-plan="auto">Auto-detect</div>
    <div class="menu-item" data-plan="free">Free</div>
    <div class="menu-item" data-plan="pro">Pro</div>
    <div class="menu-item" data-plan="max5x">Max 5×</div>
    <div class="menu-item" data-plan="max20x">Max 20×</div>
    <div class="menu-item" data-plan="max">Max (unspecified)</div>
    <div class="menu-separator"></div>
    <div class="menu-item" id="menu-refresh">Refresh Now</div>
    <div class="menu-separator"></div>
    <div class="menu-item danger" id="menu-quit">Quit</div>
  `;
}

export async function show(x: number, y: number, options: ContextMenuOptions): Promise<void> {
  if (!menuEl) {
    menuEl = buildMenu();
  }

  currentOptions = options;

  // Update rate-limited state
  const refreshBtn = menuEl.querySelector<HTMLElement>('#menu-refresh');
  if (refreshBtn) {
    refreshBtn.className = 'menu-item' + (options.isRateLimited ? ' disabled' : '');
    refreshBtn.title = options.isRateLimited ? 'Rate limited — cannot refresh' : '';
  }

  // Update slider value
  const slider = menuEl.querySelector<HTMLInputElement>('#opacity-slider');
  if (slider) {
    slider.value = String(Math.round(options.currentOpacity * 100));
  }

  // ── Issue 5b: grow the window if it is too small for the menu ──────────────
  // Wrap all resize calls in .catch() so a failed invoke never blocks the menu.
  const prev = await invoke<WindowSize>('get_window_size').catch(() => null);
  if (prev) {
    savedSize = prev;
    const needW = Math.max(prev.width, MENU_MIN_W);
    const needH = Math.max(prev.height, MENU_MIN_H);
    if (needW !== prev.width || needH !== prev.height) {
      await invoke('set_window_size', { width: needW, height: needH }).catch(() => {/* ignore */});
    }
  }

  // Record open timestamp for the blur guard AFTER the resize settle.
  menuOpenedAt = Date.now();

  // Position the menu, keeping it on-screen
  menuEl.style.left = `${x}px`;
  menuEl.style.top = `${y}px`;
  menuEl.classList.add('visible');

  // Adjust if off-screen (the CSS max-height safety net handles vertical overflow)
  const rect = menuEl.getBoundingClientRect();
  if (rect.right > window.innerWidth) {
    menuEl.style.left = `${x - rect.width}px`;
  }
  if (rect.bottom > window.innerHeight) {
    menuEl.style.top = `${y - rect.height}px`;
  }
}

export function hide(): void {
  if (!menuEl?.classList.contains('visible')) return;
  menuEl.classList.remove('visible');

  // ── Issue 5b: restore the window to its prior size ────────────────────────
  if (savedSize) {
    const { width, height } = savedSize;
    savedSize = null;
    invoke('set_window_size', { width, height }).catch(() => {/* ignore */});
  }

  // Reset blur guard so next open arms it fresh.
  menuOpenedAt = 0;
}

/** Wire up context-menu activation; call once during init. */
export function init(): void {
  // ── Contextmenu: show (with optional window grow) ─────────────────────────
  document.addEventListener('contextmenu', (e) => {
    e.preventDefault();
    const opts: ContextMenuOptions = {
      isRateLimited: false, // updated per-render
      currentOpacity: currentOptions.currentOpacity,
      onOpacityChange: currentOptions.onOpacityChange,
    };
    show(e.clientX, e.clientY, opts).catch(console.error);
  });

  // ── Inside-click dismissal (clicks within WebView, outside the menu) ───────
  document.addEventListener('click', (e) => {
    const menu = document.getElementById('context-menu');
    if (menu && !menu.contains(e.target as Node)) {
      hide();
    }
  });

  // ── Keyboard dismissal ────────────────────────────────────────────────────
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') hide();
  });

  // ── Issue 6: focus-loss dismissal ─────────────────────────────────────────
  // Clicks on the desktop or another app never reach the WebView's DOM, so the
  // inside-click handler above cannot close the menu.  We close it whenever the
  // OS reports that the overlay window lost focus.

  // Helper: ignore transient blur events emitted by the window resize itself
  // (the grow call in show() may cause a brief focus-lost/focus-gained cycle).
  const shouldDismissOnFocusLoss = (): boolean =>
    Date.now() - menuOpenedAt > BLUR_GUARD_MS;

  // Primary: Tauri JS window focus event.
  getCurrentWindow()
    .onFocusChanged(({ payload: focused }) => {
      if (!focused && shouldDismissOnFocusLoss()) {
        hide();
      }
    })
    .catch(console.error); // non-fatal if the API is unavailable

  // Belt-and-braces: DOM window blur (fires when the WebView loses focus).
  window.addEventListener('blur', () => {
    if (shouldDismissOnFocusLoss()) {
      hide();
    }
  });
}

/** Update rate-limited state externally (from the main render loop). */
export function setRateLimited(limited: boolean): void {
  currentOptions.isRateLimited = limited;
}

/** Set the opacity change callback (called from main bootstrap). */
export function setOpacityCallback(fn: (v: number) => void): void {
  currentOptions.onOpacityChange = fn;
}

/**
 * Set the current opacity value so the slider opens at the persisted value.
 * Call this from main bootstrap after loading settings (parallel to setOpacityCallback).
 * `show()` already syncs the slider to `currentOptions.currentOpacity`, so setting
 * it here ensures the menu reflects the saved value on first open.
 */
export function setCurrentOpacity(v: number): void {
  currentOptions.currentOpacity = v;
}
