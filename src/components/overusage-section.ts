/**
 * Overusage statistics section component.
 *
 * Renders a richer "Overusage" block in the card footer that visually mirrors
 * the existing quota bars (window-bar.ts). Consumes ExtraUsage from the snapshot
 * — no backend changes required; the data already arrives in the snapshot.
 *
 * NOTE: used_credits and monthly_limit arrive from the API in CENTS (minor units).
 * The UI divides by 100 before display. The Rust model layer passes values through
 * without scaling (cents → cents), so the conversion is done exclusively here.
 *
 * Design (Phase 2 / Problem B):
 *  - Numbers and progress bar are ALWAYS shown when values are present, regardless
 *    of is_enabled. The on/off pill reflects the authoritative profile flag
 *    (profile.has_extra_usage_enabled), falling back to eu.enabled.
 *  - A "limit reached" state is shown when used_credits >= monthly_limit (both
 *    non-null, limit > 0). This is separate from the on/off indicator.
 */

import type { ExtraUsage, Profile } from '../store.ts';

// Single conversion-site constant; change to '$' or another symbol here only.
const CURRENCY = '€';

// ── View model ───────────────────────────────────────────────────────────────

export interface OverusageViewModel {
  enabled: boolean;
  currentOverusage: number | null; // used_credits (in CENTS — divide by 100 for display)
  allowedOverusage: number | null; // monthly_limit (in CENTS — divide by 100 for display)
  utilization: number | null;      // 0–100, fallback for the progress bar
}

/**
 * Pure helper: maps ExtraUsage fields to the view model.
 * Kept pure and exported so it is unit-testable without DOM.
 * Does NOT scale the cent values — raw cents are preserved in the view model;
 * only formatCredits divides by 100.
 */
export function deriveOverusage(eu: ExtraUsage): OverusageViewModel {
  return {
    enabled: eu.enabled,
    currentOverusage: eu.used_credits ?? null,
    allowedOverusage: eu.monthly_limit ?? null,
    utilization: eu.utilization ?? null,
  };
}

// ── Pure helpers (exported for unit-testing without DOM) ─────────────────────

/**
 * Resolve the authoritative on/off state for the overusage feature.
 *
 * Prefers profile.has_extra_usage_enabled (org-level authoritative signal)
 * when a profile is available; falls back to eu.enabled otherwise (e.g.
 * during degraded/early states where profile has not yet loaded).
 */
export function resolveEnabled(eu: ExtraUsage, profile: Profile | null): boolean {
  if (profile !== null) return profile.has_extra_usage_enabled;
  return eu.enabled;
}

/**
 * Returns true when the overage cap has been reached or exceeded.
 *
 * Conditions: both used and limit are non-null, limit > 0, used >= limit.
 * Returns false when either value is null or limit is 0 (no cap configured).
 */
export function isLimitReached(used: number | null, limit: number | null): boolean {
  if (used === null || limit === null || limit <= 0) return false;
  return used >= limit;
}

// ── Formatting helpers ───────────────────────────────────────────────────────

/**
 * Format a credit value for display.
 * Input is in CENTS (minor units); output divides by 100 and prefixes CURRENCY.
 * Examples: 2000 → "€20.00", 1921 → "€19.21", 0 → "€0.00", null → "—".
 */
export function formatCredits(value: number | null): string {
  if (value === null) return '—';
  return `${CURRENCY}${(value / 100).toFixed(2)}`;
}

export function getFillClass(utilization: number): string {
  if (utilization === 0) return 'progress-fill zero';
  if (utilization >= 90) return 'progress-fill danger';
  if (utilization >= 70) return 'progress-fill warn';
  return 'progress-fill';
}

/**
 * Compute the progress bar fill percentage (0–100).
 *
 * Priority:
 *  1. If both used and limit are present and limit > 0 → derive from ratio.
 *     The ratio is unit-agnostic (cents/cents cancels), so no /100 needed here.
 *  2. Else if utilization is present → use it directly.
 *  3. Otherwise → 0 (empty track).
 *
 * Exported as a pure helper for unit-testing without DOM.
 */
export function computeFill(
  used: number | null,
  limit: number | null,
  utilization: number | null,
): number {
  if (used !== null && limit !== null && limit > 0) {
    return Math.min(Math.max((used / limit) * 100, 0), 100);
  }
  if (utilization !== null) {
    return Math.min(Math.max(utilization, 0), 100);
  }
  return 0;
}

// ── DOM renderer ─────────────────────────────────────────────────────────────

/**
 * Build or update the overusage section inside the given footer element.
 * Renders whenever extra_usage is present in the snapshot.
 * Caller is responsible for showing/hiding the footer element itself.
 *
 * Numbers and the progress bar are ALWAYS rendered when values are present —
 * they are NOT gated on is_enabled. The on/off indicator reflects the
 * authoritative profile flag (has_extra_usage_enabled), falling back to
 * eu.enabled when profile is unavailable.
 *
 * States:
 *  - on:            enabled=true, limit not reached  → amber "on" pill
 *  - off:           enabled=false, limit not reached → muted "off" pill
 *  - limit reached: used >= limit (regardless of enabled) → red "limit reached" pill,
 *                   bar forced to 100% danger style
 *
 * Layout:
 *   Overusage  [on/off | limit reached]     €19.21  /  €20.00
 *   ============================================================
 */
export function renderOverusageSection(
  footer: HTMLElement,
  eu: ExtraUsage,
  profile: Profile | null,
): void {
  const vm = deriveOverusage(eu);
  const enabled = resolveEnabled(eu, profile);
  const limitReached = isLimitReached(vm.currentOverusage, vm.allowedOverusage);

  // ── On/off / limit-reached pill ───────────────────────────────────────────
  let pillClass: string;
  let pillText: string;
  if (limitReached) {
    pillClass = 'overusage-pill reached';
    pillText = 'limit reached';
  } else if (enabled) {
    pillClass = 'overusage-pill on';
    pillText = 'on';
  } else {
    pillClass = 'overusage-pill off';
    pillText = 'off';
  }

  // ── Values (right-aligned) — always rendered, never gated on enabled ──────
  const spentStr   = formatCredits(vm.currentOverusage);
  const allowedStr = formatCredits(vm.allowedOverusage);

  // ── Progress bar — always computed, never zeroed by enabled ───────────────
  // When the limit is reached we force the bar to 100% at danger style.
  const fill = limitReached
    ? 100
    : computeFill(vm.currentOverusage, vm.allowedOverusage, vm.utilization);
  const fillClass = limitReached ? 'progress-fill danger' : getFillClass(fill);

  // ── Section class — only mute when disabled AND there is no data ──────────
  // When real values are present (cap met) the section must NOT be greyed out.
  const hasData = vm.currentOverusage !== null || vm.allowedOverusage !== null;
  const sectionClass =
    !enabled && !hasData && !limitReached
      ? 'overusage-section disabled'
      : 'overusage-section';

  footer.innerHTML = `
    <div class="${sectionClass}">
      <div class="quota-bar-header overusage-header">
        <span class="overusage-label-group">
          <span class="quota-bar-label">Overusage</span>
          <span class="${pillClass}">${pillText}</span>
        </span>
        <span class="overusage-values">
          <span class="overusage-spent">${spentStr}</span>
          <span class="overusage-sep">/</span>
          <span class="overusage-allowed">${allowedStr}</span>
        </span>
      </div>
      <div class="progress-track overusage-progress">
        <div class="${fillClass}" style="width:${fill}%"></div>
      </div>
    </div>`;
}
