/**
 * Lightweight state store — holds the last received snapshot and
 * notifies subscribers when it changes. No external dependencies.
 */

export type Plan = 'free' | 'pro' | 'max5x' | 'max20x' | 'max' | 'unknown';

export interface QuotaWindow {
  key: string;
  label: string;
  utilization: number; // 0–100
  resets_at: string | null; // ISO-8601 UTC
}

export interface ExtraUsage {
  enabled: boolean;
  used_credits: number | null;
  monthly_limit: number | null;
  utilization: number | null;
}

export interface Profile {
  display_name: string | null;
  email: string | null;
  has_claude_max: boolean;
  has_claude_pro: boolean;
  rate_limit_tier: string | null;
  has_extra_usage_enabled: boolean;
  subscription_status: string | null;
}

export type SourceStatusType =
  | { type: 'live' }
  | { type: 'stale'; detail: string }
  | { type: 'degraded' }
  | { type: 'auth_expired' }
  | { type: 'loading' }
  | { type: 'error'; detail: string };

export interface UsageSnapshot {
  plan: Plan;
  profile: Profile | null;
  windows: QuotaWindow[];
  extra_usage: ExtraUsage | null;
  status: SourceStatusType;
  fetched_at: string; // ISO-8601
  next_poll_in: number; // seconds
}

// ── Settings ──────────────────────────────────────────────────────────────────

/** Persisted application settings (mirrors `settings.rs:Settings`). */
export interface Settings {
  opacity: number;
  /** "small" | "medium" | "large" | "default" */
  size_preset: string;
  /** null = auto-detect; "free"|"pro"|"max5x"|"max20x"|"max" = override */
  plan_override: string | null;
}

// ── Store ──────────────────────────────────────────────────────────────────────

type Subscriber = (snapshot: UsageSnapshot) => void;

class UsageStore {
  private snapshot: UsageSnapshot | null = null;
  private subscribers: Set<Subscriber> = new Set();

  get(): UsageSnapshot | null {
    return this.snapshot;
  }

  set(snap: UsageSnapshot): void {
    this.snapshot = snap;
    for (const sub of this.subscribers) {
      sub(snap);
    }
  }

  subscribe(fn: Subscriber): () => void {
    this.subscribers.add(fn);
    // Immediately call with current state if available
    if (this.snapshot) fn(this.snapshot);
    return () => this.subscribers.delete(fn);
  }
}

export const store = new UsageStore();
