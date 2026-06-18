/**
 * Renders a single quota progress bar + countdown row.
 */

import type { QuotaWindow } from '../store.ts';
import { formatCountdown, secondsUntil } from '../countdown.ts';

/** Create (or update in-place) a quota bar DOM element. */
export function createWindowBar(w: QuotaWindow, isDegraded = false): HTMLElement {
  const wrap = document.createElement('div');
  wrap.className = 'quota-bar-wrap';
  wrap.dataset.key = w.key;

  const header = document.createElement('div');
  header.className = 'quota-bar-header';

  const label = document.createElement('span');
  label.className = 'quota-bar-label' + (isDegraded ? ' degraded' : '');
  label.textContent = w.label;

  const right = document.createElement('div');
  right.className = 'quota-bar-right';

  const pct = document.createElement('span');
  pct.className = 'quota-pct';
  pct.textContent = `${Math.round(w.utilization)}%`;

  const countdown = document.createElement('span');
  countdown.className = 'quota-countdown';
  countdown.dataset.resetsAt = w.resets_at ?? '';
  if (w.resets_at) {
    countdown.textContent = formatCountdown(secondsUntil(w.resets_at));
  }

  right.appendChild(pct);
  right.appendChild(countdown);
  header.appendChild(label);
  header.appendChild(right);

  const track = document.createElement('div');
  track.className = 'progress-track';

  const fill = document.createElement('div');
  fill.className = getFillClass(w.utilization);
  fill.style.width = `${Math.min(w.utilization, 100)}%`;

  track.appendChild(fill);
  wrap.appendChild(header);
  wrap.appendChild(track);

  return wrap;
}

/** Update an existing bar element with new data (avoids full re-render). */
export function updateWindowBar(el: HTMLElement, w: QuotaWindow, isDegraded = false): void {
  const label = el.querySelector<HTMLElement>('.quota-bar-label');
  if (label) {
    label.textContent = w.label;
    label.className = 'quota-bar-label' + (isDegraded ? ' degraded' : '');
  }

  const pct = el.querySelector<HTMLElement>('.quota-pct');
  if (pct) pct.textContent = `${Math.round(w.utilization)}%`;

  const countdown = el.querySelector<HTMLElement>('.quota-countdown');
  if (countdown) {
    countdown.dataset.resetsAt = w.resets_at ?? '';
    if (w.resets_at) {
      countdown.textContent = formatCountdown(secondsUntil(w.resets_at));
    } else {
      countdown.textContent = '';
    }
  }

  const fill = el.querySelector<HTMLElement>('.progress-fill');
  if (fill) {
    fill.className = getFillClass(w.utilization);
    fill.style.width = `${Math.min(w.utilization, 100)}%`;
  }
}

function getFillClass(utilization: number): string {
  if (utilization === 0) return 'progress-fill zero';
  if (utilization >= 90) return 'progress-fill danger';
  if (utilization >= 70) return 'progress-fill warn';
  return 'progress-fill';
}
