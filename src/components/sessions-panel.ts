/**
 * Sessions panel — full-window in-overlay view showing per-session token usage.
 *
 * Strategy (mirrors settings-panel.ts):
 *   The panel is a sibling of the overlay card inside #app, toggled via .visible.
 *   On open the window is grown to SESSIONS_VIEW_WIDTH × SESSIONS_VIEW_HEIGHT (only
 *   grown, never shrunk — Math.max).  On close the saved size is restored.  All
 *   resize calls are .catch()-guarded so a failed invoke never blocks the panel.
 *   The panel is internally scrollable (.sessions-view { overflow-y: auto }) as a
 *   safety net if the grow call fails or the display constrains the window.
 *   Sessions are re-fetched every POLL_INTERVAL_MS while the panel is open.
 *
 * Exports: init(), open(), close(), isOpen()
 */

import { invoke } from '@tauri-apps/api/core';
import { getCurrentWindow } from '@tauri-apps/api/window';

// ── Constants ────────────────────────────────────────────────────────────────

export const SESSIONS_VIEW_WIDTH = 320;
export const SESSIONS_VIEW_HEIGHT = 360;

/**
 * Re-poll interval in ms while the panel is open.
 *
 * This is an INDEPENDENT timer (not tied to the account-usage poller in
 * `poller.rs`).  `get_sessions` only reads local JSONL transcripts — no network,
 * no rate limit — so it can refresh quickly.  2s keeps token counts feeling live
 * while re-parsing live transcripts at a modest cadence.
 */
const POLL_INTERVAL_MS = 3000;

/** Green→orange freshness boundary: dots older than this threshold show orange instead of green. */
const FRESH_THRESHOLD_MS = 5 * 60 * 1000; // 5 min

// ── Types ─────────────────────────────────────────────────────────────────────

export interface SessionSummary {
  /** Stable node id: sessionId for a top-level session, agentId for a sub-agent. */
  id: string;
  /** Parent node id; null for a top-level session (forest root). */
  parentId: string | null;
  sessionId: string;
  project: string;
  agentName: string | null;
  model: string | null;
  lastActive: string;   // ISO 8601 timestamp
  inputTokens: number;
  outputTokens: number;
  cacheCreation: number;
  cacheRead: number;
  totalTokens: number;
  active: boolean;
}

/** Internal tree node built each render pass from the flat session list. */
interface TreeNode {
  session: SessionSummary;
  children: TreeNode[];
}

// ── Module state ─────────────────────────────────────────────────────────────

interface WindowSize { width: number; height: number; }

let panelEl: HTMLElement | null = null;
let _isOpen = false;
let savedSize: WindowSize | null = null;
let pollTimer: ReturnType<typeof setInterval> | null = null;

/**
 * Last successful fetch — lets a caret toggle re-render without a network call.
 * Updated on every successful invoke('get_sessions').
 */
let lastSessions: SessionSummary[] = [];

/**
 * Set of node ids the user has explicitly collapsed.
 * Absence = expanded, so newly-arrived nodes always default to visible.
 * Keyed by the stable `id` field so state survives the 5s re-poll DOM rebuild.
 */
const collapsed = new Set<string>();

// ── Public API ───────────────────────────────────────────────────────────────

/** Initialise the panel (append root element to #app). Call once at bootstrap. */
export function init(): void {
  const appEl = document.getElementById('app');
  if (!appEl) return;

  const root = document.createElement('div');
  root.id = 'sessions-root';
  root.className = 'sessions-panel';
  appEl.appendChild(root);
  panelEl = root;

  // Build the inner DOM
  buildPanel(root);

  // Keyboard dismissal. Unlike the settings panel, the sessions view does NOT
  // auto-close on focus loss / click-away — it stays open as a persistent
  // tracker until the user closes it explicitly (Escape or the close button).
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && _isOpen) close();
  });
}

/** Open the sessions panel: grow window, hide card, show panel, start polling. */
export async function open(): Promise<void> {
  if (_isOpen || !panelEl) return;
  _isOpen = true;

  // Save current window size, then grow if needed
  const prev = await invoke<WindowSize>('get_window_size').catch(() => null);
  if (prev) {
    savedSize = prev;
    const needW = Math.max(prev.width, SESSIONS_VIEW_WIDTH);
    const needH = Math.max(prev.height, SESSIONS_VIEW_HEIGHT);
    if (needW !== prev.width || needH !== prev.height) {
      await invoke('set_window_size', { width: needW, height: needH }).catch(() => { /* non-fatal */ });
    }
  }

  // Hide card, show panel
  hideCard(true);
  panelEl.classList.add('visible');

  // Load sessions immediately, then start re-poll (show Loading placeholder while fetching)
  renderSessions().catch(console.error);
  pollTimer = setInterval(() => {
    renderSessions().catch(console.error);
  }, POLL_INTERVAL_MS);
}

/** Close the sessions panel: hide panel, clear poll timer, restore window size, show card. */
export function close(): void {
  if (!_isOpen || !panelEl) return;
  _isOpen = false;

  // Clear re-poll to avoid leaked timers
  if (pollTimer !== null) {
    clearInterval(pollTimer);
    pollTimer = null;
  }

  panelEl.classList.remove('visible');
  hideCard(false);

  // Restore saved window size
  if (savedSize) {
    const { width, height } = savedSize;
    savedSize = null;
    invoke('set_window_size', { width, height }).catch(() => { /* non-fatal */ });
  }
}

/** Returns true if the panel is currently visible. */
export function isOpen(): boolean {
  return _isOpen;
}

// ── DOM construction ─────────────────────────────────────────────────────────

function buildPanel(root: HTMLElement): void {
  root.innerHTML = `
    <div class="sessions-view">
      <div class="settings-header-row">
        <span class="settings-title">Sessions</span>
        <button class="settings-close" id="sessions-close-btn" aria-label="Close sessions">✕</button>
      </div>
      <div class="sessions-list" id="sessions-list">
        <div class="sessions-loading">Loading…</div>
      </div>
    </div>
    <div class="app-version"></div>
  `;

  // Close button
  root.querySelector<HTMLButtonElement>('#sessions-close-btn')
    ?.addEventListener('click', () => close());

  // Delegated caret-click listener on the list container.
  // Reads data-caret-node-id from the clicked button, toggles it in `collapsed`,
  // and re-renders from the cached lastSessions — no network call.
  // The caret is a <button>, which the existing drag-to-move handler already skips
  // via `target.closest('button') !== null`.
  const listEl = root.querySelector<HTMLElement>('#sessions-list');
  listEl?.addEventListener('click', (e: MouseEvent) => {
    const btn = (e.target as HTMLElement).closest<HTMLElement>('[data-caret-node-id]');
    if (!btn) return;
    const nodeId = btn.dataset.caretNodeId;
    if (!nodeId) return;
    if (collapsed.has(nodeId)) {
      collapsed.delete(nodeId);
    } else {
      collapsed.add(nodeId);
    }
    renderFromCache();
  });

  // Drag-to-move: left-click drag anywhere on the panel except interactive
  // elements — mirrors the main overlay card so the window can be repositioned
  // from this view too.
  root.addEventListener('mousedown', async (e: MouseEvent) => {
    if (e.button !== 0) return;
    const target = e.target as HTMLElement;
    if (target.tagName === 'INPUT' || target.closest('button') !== null) {
      return;
    }
    await getCurrentWindow().startDragging();
  });
}

// ── Session rendering ─────────────────────────────────────────────────────────

async function renderSessions(): Promise<void> {
  if (!panelEl) return;
  const listEl = panelEl.querySelector<HTMLElement>('#sessions-list');
  if (!listEl) return;

  let sessions: SessionSummary[];
  try {
    sessions = await invoke<SessionSummary[]>('get_sessions');
  } catch (err) {
    listEl.innerHTML = `<div class="sessions-error">Failed to load sessions.</div>`;
    console.error('get_sessions failed:', err);
    return;
  }

  // Cache for caret-toggle re-renders (no second network call)
  lastSessions = sessions;

  if (sessions.length === 0) {
    listEl.innerHTML = `<div class="sessions-empty">No active sessions</div>`;
    return;
  }

  listEl.innerHTML = buildForestHtml(sessions);
}

/** Re-render from the cached last fetch — called on caret toggle, no network call. */
function renderFromCache(): void {
  if (!panelEl) return;
  const listEl = panelEl.querySelector<HTMLElement>('#sessions-list');
  if (!listEl) return;
  if (lastSessions.length === 0) {
    listEl.innerHTML = `<div class="sessions-empty">No active sessions</div>`;
    return;
  }
  listEl.innerHTML = buildForestHtml(lastSessions);
}

/**
 * Build the full tree HTML from a flat session list.
 *
 * Algorithm:
 *  1. Index all nodes into a Map<id, TreeNode>.
 *  2. Link each node under its parentId's node; nodes with no present parent
 *     become roots (orphan → root), so the tree is never broken by a missing
 *     ancestor.
 *  3. Sort roots and each sibling group by lastActive descending.
 *  4. Render recursively, guarded by a visited set and a depth cap.
 */
function buildForestHtml(sessions: SessionSummary[]): string {
  // Step 1: index
  const nodeMap = new Map<string, TreeNode>();
  for (const s of sessions) {
    nodeMap.set(s.id, { session: s, children: [] });
  }

  // Step 2: link — parent absent or parentId null → root
  const roots: TreeNode[] = [];
  for (const s of sessions) {
    const node = nodeMap.get(s.id)!;
    if (s.parentId !== null && s.parentId !== undefined && nodeMap.has(s.parentId)) {
      nodeMap.get(s.parentId)!.children.push(node);
    } else {
      roots.push(node);
    }
  }

  // Step 3: sort siblings by lastActive descending
  const byLastActiveDesc = (a: TreeNode, b: TreeNode): number =>
    new Date(b.session.lastActive).getTime() - new Date(a.session.lastActive).getTime();

  roots.sort(byLastActiveDesc);
  for (const node of nodeMap.values()) {
    if (node.children.length > 1) {
      node.children.sort(byLastActiveDesc);
    }
  }

  // Step 4: render
  const visited = new Set<string>();
  return roots.map((r) => renderNode(r, 0, visited)).join('');
}

/**
 * Render one tree node and its subtree as an HTML string.
 *
 * Each row = a caret cell (▾/▸ or invisible spacer for leaf rows so alignment
 * is preserved) followed by the existing session-meta content (active dot,
 * project, agentName, model · relTime) and a session-tokens line below.
 * Children are wrapped in a .session-children container (indent + guide line)
 * and omitted entirely when the node is collapsed.
 *
 * Guards: visited Set prevents infinite loops on accidental cycles; depth cap
 * at 20 prevents stack overflow on pathological data.
 */
function renderNode(node: TreeNode, depth: number, visited: Set<string>): string {
  const { session: s } = node;
  if (visited.has(s.id) || depth > 20) return '';
  visited.add(s.id);

  const hasChildren = node.children.length > 0;
  const isCollapsed = collapsed.has(s.id);

  // Per-node display values — same info as the previous flat row
  const relTime = formatRelativeTime(s.lastActive);
  const total = formatTokenCount(s.totalTokens);
  const inp = formatTokenCount(s.inputTokens);
  const out = formatTokenCount(s.outputTokens);
  const cache = formatTokenCount(s.cacheCreation + s.cacheRead);

  const modelPrefix = s.model ? `${escapeHtml(s.model)} · ` : '';

  // Title: the project name is the same across an entire family, so only the
  // root row (depth 0) shows it.  Child rows show just the agent name.
  const agentLabel = s.agentName ? escapeHtml(s.agentName) : '';
  const titleHtml = depth === 0
    ? `<span class="session-project">${escapeHtml(s.project)}</span>` +
      (agentLabel ? `<span class="session-name"> · ${agentLabel}</span>` : '')
    : `<span class="session-name">${agentLabel}</span>`;

  // Status dot: green if last active < 5 min ago, orange otherwise.
  // Treat NaN or a negative age (clock skew) as fresh so we never show a
  // misleading orange on a brand-new or timestamp-less session.
  const ageMs = Date.now() - new Date(s.lastActive).getTime();
  const statusDot = (isNaN(ageMs) || ageMs < 0 || ageMs < FRESH_THRESHOLD_MS)
    ? '<span class="session-active-dot" title="Active"></span>'
    : '<span class="session-stale-dot" title="Idle"></span>';

  // Caret for parents; invisible same-width spacer for leaf rows (keeps column aligned)
  const safeId = escapeHtml(s.id);
  const caret = hasChildren
    ? `<button class="session-caret" data-caret-node-id="${safeId}">${isCollapsed ? '&#9658;' : '&#9660;'}</button>`
    : `<span class="session-caret leaf"></span>`;

  // Children block — omitted (not just hidden) when collapsed so DOM stays lean
  const childrenHtml = hasChildren && !isCollapsed
    ? `<div class="session-children">${node.children.map((c) => renderNode(c, depth + 1, visited)).join('')}</div>`
    : '';

  return (
    `<div class="session-row" data-node-id="${safeId}" data-depth="${depth}">` +
      `<div class="session-meta">` +
        `${caret}${statusDot}` +
        `${titleHtml}` +
        `<span class="session-secondary">${modelPrefix}${relTime}</span>` +
      `</div>` +
      `<div class="session-tokens">Total ${total} · in ${inp} · out ${out} · cache ${cache}</div>` +
    `</div>` +
    childrenHtml
  );
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/** Format a number compactly: 12345 → "12.3k", 1500000 → "1.5M". */
function formatTokenCount(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

/** Format an ISO 8601 timestamp as a short relative time ("just now", "5m ago"). */
function formatRelativeTime(iso: string): string {
  const diffMs = Date.now() - new Date(iso).getTime();
  if (isNaN(diffMs) || diffMs < 0) return 'just now';
  const diffSec = Math.floor(diffMs / 1000);
  if (diffSec < 60) return 'just now';
  const diffMin = Math.floor(diffSec / 60);
  if (diffMin < 60) return `${diffMin}m ago`;
  const diffHr = Math.floor(diffMin / 60);
  if (diffHr < 24) return `${diffHr}h ago`;
  const diffDay = Math.floor(diffHr / 24);
  return `${diffDay}d ago`;
}

/** Escape HTML special characters for safe inline rendering. */
function escapeHtml(str: string): string {
  return str
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

/** Show or hide the overlay card (first .overlay-card child of #app). */
function hideCard(hidden: boolean): void {
  const appEl = document.getElementById('app');
  if (!appEl) return;
  const card = appEl.querySelector<HTMLElement>('.overlay-card');
  if (card) card.style.display = hidden ? 'none' : '';
}
