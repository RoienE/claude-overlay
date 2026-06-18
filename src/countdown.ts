/**
 * Local 1-second countdown ticker.
 *
 * Reads `resets_at` timestamps from the store and updates countdown
 * displays every second without any network cost.
 */

/** Format a duration in seconds as "Xh Ym" or "Ym Zs" or "Zs". */
export function formatCountdown(totalSeconds: number): string {
  if (totalSeconds <= 0) return 'resetting…';

  const h = Math.floor(totalSeconds / 3600);
  const m = Math.floor((totalSeconds % 3600) / 60);
  const s = totalSeconds % 60;

  if (h > 0) {
    return m > 0 ? `${h}h ${m}m` : `${h}h`;
  }
  if (m > 0) {
    return s > 0 ? `${m}m ${s}s` : `${m}m`;
  }
  return `${s}s`;
}

/** Calculate seconds until a UTC ISO-8601 timestamp. */
export function secondsUntil(isoUtc: string): number {
  const target = new Date(isoUtc).getTime();
  const now = Date.now();
  return Math.max(0, Math.floor((target - now) / 1000));
}

/** Manage countdown elements in the DOM, updated every second. */
export class CountdownManager {
  private timerId: ReturnType<typeof setInterval> | null = null;
  private countdownEls: Map<string, HTMLElement> = new Map();

  /** Register an element for a given window key with a target timestamp. */
  register(key: string, el: HTMLElement): void {
    this.countdownEls.set(key, el);
  }

  /** Remove all registered elements. */
  clear(): void {
    this.countdownEls.clear();
  }

  /** Start the 1-second tick (idempotent). */
  start(): void {
    if (this.timerId !== null) return;
    this.timerId = setInterval(() => this.tick(), 1000);
  }

  /** Stop the ticker. */
  stop(): void {
    if (this.timerId !== null) {
      clearInterval(this.timerId);
      this.timerId = null;
    }
  }

  /** Called by external code after a new snapshot arrives to update timestamps. */
  updateTimestamps(windows: Array<{ key: string; resets_at: string | null }>): void {
    for (const w of windows) {
      const el = this.countdownEls.get(w.key);
      if (!el) continue;
      if (!w.resets_at) {
        el.textContent = '';
        el.dataset.resetsAt = '';
      } else {
        el.dataset.resetsAt = w.resets_at;
        el.textContent = formatCountdown(secondsUntil(w.resets_at));
      }
    }
  }

  private tick(): void {
    for (const el of this.countdownEls.values()) {
      const resetsAt = el.dataset.resetsAt;
      if (!resetsAt) continue;
      el.textContent = formatCountdown(secondsUntil(resetsAt));
    }
  }
}

// Pure functions (formatCountdown, secondsUntil) are unit-testable with vitest.
// To add tests: pnpm add -D vitest && create src/__tests__/countdown.test.ts
