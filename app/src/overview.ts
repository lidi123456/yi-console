// State behind the overview page (components/Home.tsx) that lives only on this device: which finished turns were
// seen, drafts of sessions to start later, and the recent activity feed. Kept out of store.ts, which only feeds it
// (`markSeen` on navigation, `recordChange` on session.updated).
import { useEffect, useState, useSyncExternalStore } from 'react';
import { isHostSession, type SessionInfo, type ToolKind } from './types';
import { lsGet, lsSet } from './util';

const key = (hostId: string, sid: string) => `${hostId}::${sid}`;

/** A project on the overview: one directory on one host */
export const projKey = (hostId: string, cwd: string) => `${hostId}::${cwd}`;

/** A tiny observable: bump() re-renders every subscriber of use() */
function signal() {
  const subs = new Set<() => void>();
  let version = 0;
  return {
    bump() {
      version++;
      subs.forEach((f) => f());
    },
    use() {
      return useSyncExternalStore(
        (cb) => {
          subs.add(cb);
          return () => subs.delete(cb);
        },
        () => version,
      );
    },
  };
}

// ---------- Seen turns ("To review" column) ----------

const SEEN_KEY = 'ov-seen';
const SEEN_MAX = 400;
const seenSig = signal();
let seen: Record<string, number> = lsGet(SEEN_KEY, {});
/** Turns that ended before this device first ran this version count as seen, so old sessions don't flood the column */
const since: number = (() => {
  const v = lsGet<number>('ov-seen-since', 0);
  if (v) return v;
  const now = Date.now();
  lsSet('ov-seen-since', now);
  return now;
})();

export function markSeen(items: { hostId: string; sessionId: string }[]) {
  if (!items.length) return;
  const now = Date.now();
  for (const it of items) seen[key(it.hostId, it.sessionId)] = now;
  const keys = Object.keys(seen);
  if (keys.length > SEEN_MAX) {
    keys.sort((a, b) => seen[a] - seen[b]);
    for (const k of keys.slice(0, keys.length - SEEN_MAX)) delete seen[k];
  }
  lsSet(SEEN_KEY, seen);
  seenSig.bump();
}

/** The last turn ended and the session wasn't opened on this device since */
export function unseenTurn(hostId: string, s: SessionInfo): boolean {
  const end = s.turn?.endedAt;
  return !!end && end > Math.max(since, seen[key(hostId, s.id)] ?? 0);
}

export const useSeenVersion = seenSig.use;

/** An error older than this no longer asks for attention */
const ERROR_FRESH_MS = 24 * 3600 * 1000;

/** The session waits for the user: an approval or question, a turn cut off by a restart, or a recent error.
 *  Drives the overview's first column and the count on the sidebar's Overview entry. */
export function needsAttention(s: SessionInfo, now = Date.now()): boolean {
  if (s.tool === 'files') return false;
  if (s.pendingPermissions > 0 || s.status === 'waiting-permission' || s.restartInterrupted) return true;
  return s.status === 'error' && now - s.updatedAt < ERROR_FRESH_MS;
}

// localStorage is shared between windows: pick up what another window marked
if (typeof window !== 'undefined') {
  window.addEventListener('storage', (e) => {
    if (e.key === SEEN_KEY) {
      seen = lsGet(SEEN_KEY, {});
      seenSig.bump();
    } else if (e.key === DRAFTS_KEY) {
      drafts = lsGet(DRAFTS_KEY, []);
      draftSig.bump();
    }
  });
}

// ---------- Drafts ----------

export interface Draft {
  id: string;
  text: string;
  hostId: string;
  cwd: string;
  tool: ToolKind;
  perm: string;
  at: number;
}

const DRAFTS_KEY = 'ov-drafts';
const DRAFTS_MAX = 50;
const draftSig = signal();
let drafts: Draft[] = lsGet(DRAFTS_KEY, []);

const saveDrafts = (list: Draft[]) => {
  drafts = list.slice(0, DRAFTS_MAX);
  lsSet(DRAFTS_KEY, drafts);
  draftSig.bump();
};

export function addDraft(d: Omit<Draft, 'id' | 'at'> & { id?: string; at?: number }) {
  saveDrafts([{ ...d, id: d.id ?? `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`, at: d.at ?? Date.now() }, ...drafts]);
}

export function removeDraft(id: string) {
  saveDrafts(drafts.filter((d) => d.id !== id));
}

export function useDrafts(): Draft[] {
  draftSig.use();
  return drafts;
}

// ---------- Activity feed ----------

/** A path under the session directory, relative to it */
export function relPath(p: string, cwd: string) {
  const c = cwd.replace(/[\\/]+$/, '');
  return p.startsWith(c + '/') || p.startsWith(c + '\\') ? p.slice(c.length + 1) : p;
}

export type FeedKind = 'permission' | 'question' | 'done' | 'rejected' | 'error' | 'limit';

export interface FeedItem {
  id: number;
  hostId: string;
  sessionId: string;
  kind: FeedKind;
  at: number;
  /** done: files edited in the turn; limit: when it continues */
  files?: number;
  resumeAt?: number;
  /** permission / rejected: the request's command / path / title */
  text?: string;
}

const FEED_MAX = 30;
const feedSig = signal();
let feed: FeedItem[] = [];
let feedSeq = 0;

const push = (it: Omit<FeedItem, 'id' | 'at'>) => {
  feed = [{ ...it, id: ++feedSeq, at: Date.now() }, ...feed].slice(0, FEED_MAX);
  feedSig.bump();
};

/** Called for every session.updated with the info held before it. Only changes after the app started are recorded:
 *  the feed is in memory, so it starts empty on every launch */
export function recordChange(hostId: string, prev: SessionInfo | undefined, next: SessionInfo) {
  if (!prev || !isHostSession(next)) return;
  const base = { hostId, sessionId: next.id };
  if (next.pending && next.pending.requestId !== prev.pending?.requestId) {
    const p = next.pending;
    push({ ...base, kind: p.question ? 'question' : 'permission', text: (p.detailKind === 'file' && p.detail ? relPath(p.detail, next.cwd) : p.detail) || p.title });
  }
  if (next.status === 'error' && prev.status !== 'error') push({ ...base, kind: 'error' });
  if (next.autoResume && !prev.autoResume) push({ ...base, kind: 'limit', resumeAt: next.autoResume.at });
  const end = next.turn?.endedAt;
  // A turn closed by the process exiting isn't "finished"
  if (end && end !== prev.turn?.endedAt && next.status !== 'error' && next.status !== 'exited' && !next.autoResume) {
    const rejected = next.turn?.rejected;
    push(rejected ? { ...base, kind: 'rejected', text: relPath(rejected, next.cwd) } : { ...base, kind: 'done', files: next.turn?.filesChanged ?? 0 });
  }
}

export function useFeed(): FeedItem[] {
  feedSig.use();
  return feed;
}

// ---------- Page visibility and timers ----------

const visSig = signal();
if (typeof document !== 'undefined') document.addEventListener('visibilitychange', () => visSig.bump());

/** Whether the page is visible; minimized windows and backgrounded phones are not */
export function usePageVisible(): boolean {
  visSig.use();
  return typeof document === 'undefined' || document.visibilityState === 'visible';
}

/** Date.now(), refreshed every `ms` while `active` and the page is visible (no timer otherwise) */
export function useNow(ms: number, active = true): number {
  const visible = usePageVisible();
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    if (!active || !visible) return;
    setNow(Date.now());
    const t = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(t);
  }, [ms, active, visible]);
  return now;
}
