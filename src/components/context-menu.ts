/**
 * Right-click context menu.
 * Three items only: Opacity slider, Refresh now, Quit.
 *
 * Size presets and Plan Override have been moved to the settings panel (Unit C).
 * The grow-then-restore window-resize hack has been removed — the slim menu
 * (≈100px tall) fits inside even the smallest window, so no resize is needed.
 *
 * Focus-loss dismissal:
 *   Clicks outside the tiny WebView never produce a DOM click event, so the
 *   outside-click handler can't close the menu.  We use getCurrentWindow().onFocusChanged
 *   plus a belt-and-braces window.blur listener so the menu closes whenever the
 *   overlay loses OS focus.  A small BLUR_GUARD_MS prevents an immediate self-dismiss
 *   from the transient focus change that can occur when the menu first appears.
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

// ── Blur-guard ───────────────────────────────────────────────────────────────
/**
 * Milliseconds after menu-open during which blur/focus-loss is ignored.
 * The grow-resize that used to require 300 ms is gone; 50 ms is sufficient
 * to absorb any transient focus event from the menu appearing in the DOM.
 */
const BLUR_GUARD_MS = 50;
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
      // Keep the cached value in sync so any other reader stays correct
      currentOptions.currentOpacity = val;
      invoke('set_opacity', { opacity: val }).catch(console.error);
    });
    // Prevent drag-to-move from triggering when using the slider
    slider.addEventListener('mousedown', (e) => e.stopPropagation());
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
    <div class="menu-item" id="menu-refresh">Refresh Now</div>
    <div class="menu-separator"></div>
    <div class="menu-item danger" id="menu-quit">Quit</div>
  `;
}

export function show(x: number, y: number, options: ContextMenuOptions): void {
  if (!menuEl) {
    menuEl = buildMenu();
  }

  currentOptions = options;

  // Update rate-limited state on the refresh item
  const refreshBtn = menuEl.querySelector<HTMLElement>('#menu-refresh');
  if (refreshBtn) {
    refreshBtn.className = 'menu-item' + (options.isRateLimited ? ' disabled' : '');
    refreshBtn.title = options.isRateLimited ? 'Rate limited — cannot refresh' : '';
  }

  // Sync slider to the live applied opacity on #app (single source of truth).
  // Fall back to getComputedStyle if the inline style hasn't been set yet (CSS default 0.92).
  const slider = menuEl.querySelector<HTMLInputElement>('#opacity-slider');
  if (slider) {
    const appEl = document.getElementById('app');
    const rawOpacity = appEl
      ? (appEl.style.opacity || getComputedStyle(appEl).opacity)
      : String(options.currentOpacity);
    const liveOpacity = parseFloat(rawOpacity) || options.currentOpacity;
    slider.value = String(Math.round(liveOpacity * 100));
  }

  // Record open timestamp for the blur guard
  menuOpenedAt = Date.now();

  // Position the menu, keeping it on-screen
  menuEl.style.left = `${x}px`;
  menuEl.style.top = `${y}px`;
  menuEl.classList.add('visible');

  // Adjust if off-screen (CSS max-height/overflow-y safety net handles vertical overflow)
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
  // Reset blur guard so the next open arms it fresh.
  menuOpenedAt = 0;
}

/** Wire up context-menu activation; call once during init. */
export function init(): void {
  // ── Contextmenu trigger ───────────────────────────────────────────────────
  document.addEventListener('contextmenu', (e) => {
    e.preventDefault();
    const opts: ContextMenuOptions = {
      isRateLimited: currentOptions.isRateLimited,
      currentOpacity: currentOptions.currentOpacity,
      onOpacityChange: currentOptions.onOpacityChange,
    };
    show(e.clientX, e.clientY, opts);
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

  // ── Focus-loss dismissal ──────────────────────────────────────────────────
  // Clicks on the desktop or another app never reach the WebView's DOM, so the
  // inside-click handler above cannot close the menu.  Close it whenever the
  // OS reports that the overlay window lost focus.

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
