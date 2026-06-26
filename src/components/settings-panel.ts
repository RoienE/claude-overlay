/**
 * Settings panel — full-window in-overlay view.
 *
 * Strategy (Decision 1):
 *   The panel is a sibling of the overlay card inside #app, toggled via .visible.
 *   On open the window is grown to SETTINGS_VIEW_WIDTH × SETTINGS_VIEW_HEIGHT (only
 *   grown, never shrunk — Math.max).  On close the saved size is restored.  All
 *   resize calls are .catch()-guarded so a failed invoke never blocks the panel.
 *   The panel is internally scrollable (.settings-view { overflow-y: auto }) as a
 *   safety net if the grow call fails or the display constrains the window.
 *
 * Exports: init(), open(), close(), isOpen(), setCurrentSettings()
 */

import { invoke } from '@tauri-apps/api/core';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { checkForUpdates } from '../updater.ts';

// ── Constants ────────────────────────────────────────────────────────────────

export const SETTINGS_VIEW_WIDTH = 260;
export const SETTINGS_VIEW_HEIGHT = 320;

/** Short blur-guard in ms: prevents the resize-triggered blur from auto-closing. */
const BLUR_GUARD_MS = 350;

// ── Preset dimensions (must match window_ctl.rs preset_size()) ───────────────

const PRESET_SIZES: Record<string, { width: number; height: number }> = {
  small:   { width: 220, height: 160 },
  medium:  { width: 280, height: 220 },
  large:   { width: 340, height: 280 },
  default: { width: 260, height: 200 },
};

// ── Module state ─────────────────────────────────────────────────────────────

interface WindowSize { width: number; height: number; }

let panelEl: HTMLElement | null = null;
let _isOpen = false;
let savedSize: WindowSize | null = null;
let openedAt = 0;

/** Current settings — preselect controls when the panel is built / opened. */
let currentSettings = {
  opacity: 0.92,
  size_preset: 'default',
  plan_override: null as string | null,
  history_threshold_mins: 30,
};

// ── Public API ───────────────────────────────────────────────────────────────

/** Initialise the panel (append root element to #app). Call once at bootstrap. */
export function init(): void {
  const appEl = document.getElementById('app');
  if (!appEl) return;

  const root = document.createElement('div');
  root.id = 'settings-root';
  root.className = 'settings-panel';
  appEl.appendChild(root);
  panelEl = root;

  // Build the inner DOM
  buildPanel(root);

  // Keyboard dismissal
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && _isOpen) close();
  });

  // Focus-loss dismissal (mirrors context-menu pattern)
  const shouldDismiss = (): boolean => _isOpen && Date.now() - openedAt > BLUR_GUARD_MS;

  getCurrentWindow()
    .onFocusChanged(({ payload: focused }) => {
      if (!focused && shouldDismiss()) close();
    })
    .catch(console.error);

  window.addEventListener('blur', () => {
    if (shouldDismiss()) close();
  });
}

/** Open the settings panel: grow window, hide card, show panel. */
export async function open(): Promise<void> {
  if (_isOpen || !panelEl) return;
  _isOpen = true;

  // Sync controls to latest settings before showing
  syncControls();

  // Save current window size, then grow if needed
  const prev = await invoke<WindowSize>('get_window_size').catch(() => null);
  if (prev) {
    savedSize = prev;
    const needW = Math.max(prev.width, SETTINGS_VIEW_WIDTH);
    const needH = Math.max(prev.height, SETTINGS_VIEW_HEIGHT);
    if (needW !== prev.width || needH !== prev.height) {
      await invoke('set_window_size', { width: needW, height: needH }).catch(() => { /* non-fatal */ });
    }
  }

  // Arm blur guard AFTER resize settles
  openedAt = Date.now();

  // Hide card, show panel
  hideCard(true);
  panelEl.classList.add('visible');
}

/** Close the settings panel: hide panel, restore window size, show card. */
export function close(): void {
  if (!_isOpen || !panelEl) return;
  _isOpen = false;

  panelEl.classList.remove('visible');
  hideCard(false);

  // Restore saved window size
  if (savedSize) {
    const { width, height } = savedSize;
    savedSize = null;
    invoke('set_window_size', { width, height }).catch(() => { /* non-fatal */ });
  }

  openedAt = 0;
}

/** Returns true if the panel is currently visible. */
export function isOpen(): boolean {
  return _isOpen;
}

/**
 * Preselect panel controls to match the loaded settings.
 * Call from main.ts after get_settings resolves.
 */
export function setCurrentSettings(settings: {
  opacity: number;
  size_preset: string;
  plan_override: string | null;
  history_threshold_mins: number;
}): void {
  currentSettings = { ...settings };
  if (_isOpen) syncControls();
}

// ── DOM construction ─────────────────────────────────────────────────────────

function buildPanel(root: HTMLElement): void {
  root.innerHTML = `
    <div class="settings-view">
      <div class="settings-header-row">
        <span class="settings-title">Settings</span>
        <button class="settings-close" id="settings-close-btn" aria-label="Close settings">✕</button>
      </div>

      <div class="settings-group">
        <div class="settings-group-title">Size</div>
        <div class="settings-seg" id="size-seg">
          <button class="settings-opt" data-size="small">Small</button>
          <button class="settings-opt" data-size="medium">Medium</button>
          <button class="settings-opt" data-size="large">Large</button>
          <button class="settings-opt" data-size="default">Default</button>
        </div>
      </div>

      <div class="settings-group">
        <div class="settings-group-title">History</div>
        <div class="settings-seg" id="history-seg">
          <button class="settings-opt" data-mins="15">15m</button>
          <button class="settings-opt" data-mins="30">30m</button>
          <button class="settings-opt" data-mins="60">1h</button>
          <button class="settings-opt" data-mins="180">3h</button>
        </div>
      </div>

      <!-- Intentionally hidden for now; remove the hidden attribute to re-enable. -->
      <div class="settings-group" hidden>
        <div class="settings-group-title">Plan Override</div>
        <div class="settings-seg" id="plan-seg">
          <button class="settings-opt" data-plan="auto">Auto</button>
          <button class="settings-opt" data-plan="free">Free</button>
          <button class="settings-opt" data-plan="pro">Pro</button>
          <button class="settings-opt" data-plan="max5x">Max 5×</button>
          <button class="settings-opt" data-plan="max20x">Max 20×</button>
          <button class="settings-opt" data-plan="max">Max</button>
        </div>
      </div>

      <div class="settings-group">
        <div class="settings-group-title">Opacity</div>
        <div class="opacity-slider-wrap">
          <input type="range" id="settings-opacity-slider" class="opacity-slider"
                 min="20" max="100" value="92" />
          <div class="opacity-slider-label">Adjust transparency</div>
        </div>
      </div>

      <div class="settings-group">
        <div class="settings-group-title">Updates</div>
        <button class="settings-action" id="settings-check-update-btn">
          <span class="settings-action-icon"><svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><polyline points="23 4 23 10 17 10"></polyline><polyline points="1 20 1 14 7 14"></polyline><path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15"></path></svg></span>
          <span>Check for updates</span>
        </button>
      </div>

      <div class="settings-group">
        <button class="settings-quit danger" id="settings-quit-btn">Quit</button>
      </div>
    </div>
    <div class="app-version"></div>
  `;

  // ── Close button ──────────────────────────────────────────────────────────
  root.querySelector<HTMLButtonElement>('#settings-close-btn')
    ?.addEventListener('click', () => close());

  // ── Size segmented control ────────────────────────────────────────────────
  const sizeSeg = root.querySelector<HTMLElement>('#size-seg');
  sizeSeg?.querySelectorAll<HTMLButtonElement>('[data-size]').forEach((btn) => {
    btn.addEventListener('click', () => {
      const preset = btn.dataset.size!;
      invoke('set_size_preset', { preset }).catch(console.error);

      // Update the restore-target so closing returns to the chosen size
      const dims = PRESET_SIZES[preset] ?? PRESET_SIZES.default;
      savedSize = { ...dims };

      // Also grow the window now if the new preset is larger than current
      invoke<WindowSize>('get_window_size').then((cur) => {
        const needW = Math.max(cur.width, dims.width, SETTINGS_VIEW_WIDTH);
        const needH = Math.max(cur.height, dims.height, SETTINGS_VIEW_HEIGHT);
        if (needW !== cur.width || needH !== cur.height) {
          invoke('set_window_size', { width: needW, height: needH }).catch(() => { /* non-fatal */ });
        }
      }).catch(() => { /* non-fatal */ });

      // Highlight active
      setActive(sizeSeg, btn);
      currentSettings.size_preset = preset;
    });
  });

  // ── History segmented control ─────────────────────────────────────────────
  const historySeg = root.querySelector<HTMLElement>('#history-seg');
  historySeg?.querySelectorAll<HTMLButtonElement>('[data-mins]').forEach((btn) => {
    btn.addEventListener('click', () => {
      const minutes = parseInt(btn.dataset.mins!, 10);
      invoke('set_history_threshold', { minutes }).catch(console.error);
      setActive(historySeg, btn);
      currentSettings.history_threshold_mins = minutes;
    });
  });

  // ── Plan segmented control ────────────────────────────────────────────────
  const planSeg = root.querySelector<HTMLElement>('#plan-seg');
  planSeg?.querySelectorAll<HTMLButtonElement>('[data-plan]').forEach((btn) => {
    btn.addEventListener('click', () => {
      const sel = btn.dataset.plan!;
      const plan = sel === 'auto' ? null : sel;
      invoke('set_plan_override', { plan }).catch(console.error);
      setActive(planSeg, btn);
      currentSettings.plan_override = plan;
    });
  });

  // ── Opacity slider ────────────────────────────────────────────────────────
  const slider = root.querySelector<HTMLInputElement>('#settings-opacity-slider');
  if (slider) {
    slider.addEventListener('input', () => {
      const val = parseFloat(slider.value) / 100;
      const appEl = document.getElementById('app');
      if (appEl) appEl.style.opacity = String(val);
      invoke('set_opacity', { opacity: val }).catch(console.error);
      currentSettings.opacity = val;
    });
    // Prevent drag-to-move from triggering when using the slider
    slider.addEventListener('mousedown', (e) => e.stopPropagation());
  }

  // ── Check for updates button ──────────────────────────────────────────────
  root.querySelector<HTMLButtonElement>('#settings-check-update-btn')
    ?.addEventListener('click', () => {
      void checkForUpdates({ interactive: true });
    });

  // ── Quit button ───────────────────────────────────────────────────────────
  root.querySelector<HTMLButtonElement>('#settings-quit-btn')
    ?.addEventListener('click', () => {
      invoke('quit_app').catch(console.error);
    });
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/** Toggle .active on the correct option button within a segmented control. */
function setActive(seg: HTMLElement, activeBtn: HTMLButtonElement): void {
  seg.querySelectorAll<HTMLButtonElement>('.settings-opt').forEach((b) => {
    b.classList.toggle('active', b === activeBtn);
  });
}

/** Sync control values to currentSettings (called on open + after setCurrentSettings). */
function syncControls(): void {
  if (!panelEl) return;

  // Size
  const sizeSeg = panelEl.querySelector<HTMLElement>('#size-seg');
  if (sizeSeg) {
    const activeSize = sizeSeg.querySelector<HTMLButtonElement>(
      `[data-size="${currentSettings.size_preset}"]`
    );
    sizeSeg.querySelectorAll<HTMLButtonElement>('.settings-opt').forEach((b) => {
      b.classList.toggle('active', b === activeSize);
    });
  }

  // History
  const historySeg = panelEl.querySelector<HTMLElement>('#history-seg');
  if (historySeg) {
    const activeHistory = historySeg.querySelector<HTMLButtonElement>(
      `[data-mins="${currentSettings.history_threshold_mins}"]`
    );
    historySeg.querySelectorAll<HTMLButtonElement>('.settings-opt').forEach((b) => {
      b.classList.toggle('active', b === activeHistory);
    });
  }

  // Plan
  const planSeg = panelEl.querySelector<HTMLElement>('#plan-seg');
  if (planSeg) {
    const planKey = currentSettings.plan_override ?? 'auto';
    const activePlan = planSeg.querySelector<HTMLButtonElement>(`[data-plan="${planKey}"]`);
    planSeg.querySelectorAll<HTMLButtonElement>('.settings-opt').forEach((b) => {
      b.classList.toggle('active', b === activePlan);
    });
  }

  // Opacity — read the live applied opacity on #app (single source of truth).
  // Fall back to getComputedStyle if the inline style hasn't been set yet (CSS default 0.92).
  const slider = panelEl.querySelector<HTMLInputElement>('#settings-opacity-slider');
  if (slider) {
    const appEl = document.getElementById('app');
    const rawOpacity = appEl
      ? (appEl.style.opacity || getComputedStyle(appEl).opacity)
      : String(currentSettings.opacity);
    const liveOpacity = parseFloat(rawOpacity) || currentSettings.opacity;
    slider.value = String(Math.round(liveOpacity * 100));
  }
}

/** Show or hide the overlay card (first .overlay-card child of #app). */
function hideCard(hidden: boolean): void {
  const appEl = document.getElementById('app');
  if (!appEl) return;
  const card = appEl.querySelector<HTMLElement>('.overlay-card');
  if (card) card.style.display = hidden ? 'none' : '';
}
