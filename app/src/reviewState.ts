import { useSyncExternalStore } from 'react';
import type { HubEvent, ReviewSessionMeta, SessionInfo, ToolKind } from './types';

export type ReviewSeverity = 'high' | 'medium' | 'low';
export type FindingDisposition = 'open' | 'adopted' | 'dismissed';

export interface ReviewMetaFallback extends ReviewSessionMeta {
  sessionId: string;
  parentSessionId: string;
  tool: ToolKind;
  cwd: string;
}

/** Resolve review metadata from persisted `SessionInfo.review` first, falling back to in-memory launch metadata. */
export function resolveReviewMeta(
  info?: SessionInfo | null,
  fallback?: ReviewMetaFallback | null,
): ReviewSessionMeta | null {
  if (info?.review) return info.review;
  return fallback ?? null;
}

export interface ReviewIssue {
  id: string;
  severity: ReviewSeverity;
  title: string;
  file?: string;
  line?: number;
  summary: string;
  suggestion?: string;
  rawFile?: string;
}

export interface FindingRefMeta {
  findingId: string;
  file?: string;
  line?: number;
  title?: string;
}

export interface FindingThreadMessage {
  seq: number;
  role: 'user' | 'assistant';
  text: string;
  ts: number;
  findingId: string;
  file?: string;
  line?: number;
  title?: string;
}

export interface ReviewSnapshot {
  issues: ReviewIssue[];
  prose: string;
  latestText: string;
  hasStructuredBlock: boolean;
  /** 1-based round count of structured review results observed */
  round: number;
  messageSeq: number;
}

export interface FileIssueBadge {
  high: number;
  medium: number;
  low: number;
  total: number;
  open: number;
  adopted: number;
  dismissed: number;
}

const ACTIVE_REVIEW_KEY_PREFIX = 'yi:review-active:';
const DISPOSITION_KEY_PREFIX = 'yi:review-disp:';
const DISMISS_REASONS_KEY_PREFIX = 'yi:review-notes:';

let storeVersion = 0;
let storageHooked = false;
const listeners = new Set<() => void>();

function notifyListeners() {
  storeVersion += 1;
  for (const fn of listeners) {
    try {
      fn();
    } catch {
      /* ignore */
    }
  }
}

function ensureStorageListener() {
  if (storageHooked || typeof window === 'undefined') return;
  storageHooked = true;
  window.addEventListener('storage', (e) => {
    const k = e.key;
    if (!k || k.startsWith(ACTIVE_REVIEW_KEY_PREFIX) || k.startsWith(DISPOSITION_KEY_PREFIX) || k.startsWith(DISMISS_REASONS_KEY_PREFIX)) {
      notifyListeners();
    }
  });
}

export function subscribeReviewStore(fn: () => void): () => void {
  ensureStorageListener();
  listeners.add(fn);
  return () => listeners.delete(fn);
}

export function getReviewStoreVersion(): number {
  return storeVersion;
}

/** Subscribe a React component to reactive disposition / active-review changes. */
export function useReviewStoreVersion(): number {
  return useSyncExternalStore(subscribeReviewStore, getReviewStoreVersion);
}

function safeReadStorage(key: string): string | null {
  try {
    return typeof localStorage !== 'undefined' ? localStorage.getItem(key) : null;
  } catch {
    return null;
  }
}

function safeWriteStorage(key: string, val: string | null) {
  try {
    if (typeof localStorage === 'undefined') return;
    if (val === null) localStorage.removeItem(key);
    else localStorage.setItem(key, val);
  } catch {
    /* ignore */
  }
}

export function getActiveReviewId(hostId: string, parentSessionId: string): string | null {
  if (!hostId || !parentSessionId) return null;
  return safeReadStorage(`${ACTIVE_REVIEW_KEY_PREFIX}${hostId}:${parentSessionId}`) || null;
}

export function setActiveReviewId(hostId: string, parentSessionId: string, reviewSid: string | null) {
  if (!hostId || !parentSessionId) return;
  const key = `${ACTIVE_REVIEW_KEY_PREFIX}${hostId}:${parentSessionId}`;
  const prev = safeReadStorage(key) || null;
  if (prev === reviewSid) return;
  safeWriteStorage(key, reviewSid);
  notifyListeners();
}

type StoredDispositions = Record<string, { status: FindingDisposition; refId?: string }>;

function readDispMap(hostId: string, reviewSid: string): StoredDispositions {
  const raw = safeReadStorage(`${DISPOSITION_KEY_PREFIX}${hostId}:${reviewSid}`);
  if (!raw) return {};
  try {
    const parsed = JSON.parse(raw);
    return parsed && typeof parsed === 'object' ? (parsed as StoredDispositions) : {};
  } catch {
    return {};
  }
}

function writeDispMap(hostId: string, reviewSid: string, map: StoredDispositions) {
  const keys = Object.keys(map);
  if (keys.length === 0) {
    safeWriteStorage(`${DISPOSITION_KEY_PREFIX}${hostId}:${reviewSid}`, null);
  } else {
    safeWriteStorage(`${DISPOSITION_KEY_PREFIX}${hostId}:${reviewSid}`, JSON.stringify(map));
  }
  notifyListeners();
}

export function getFindingDisposition(hostId: string, reviewSid: string, findingId: string): FindingDisposition {
  if (!hostId || !reviewSid || !findingId) return 'open';
  const map = readDispMap(hostId, reviewSid);
  return map[findingId]?.status ?? 'open';
}

export function getAllDispositions(hostId: string, reviewSid: string): Record<string, FindingDisposition> {
  if (!hostId || !reviewSid) return {};
  const map = readDispMap(hostId, reviewSid);
  const out: Record<string, FindingDisposition> = {};
  for (const [k, v] of Object.entries(map)) {
    if (v?.status) out[k] = v.status;
  }
  return out;
}

export function setFindingDisposition(
  hostId: string,
  reviewSid: string,
  findingId: string,
  status: FindingDisposition,
  refId?: string,
) {
  if (!hostId || !reviewSid || !findingId) return;
  const map = readDispMap(hostId, reviewSid);
  if (status === 'open') {
    if (!(findingId in map)) return;
    delete map[findingId];
  } else {
    const prev = map[findingId];
    if (prev && prev.status === status && prev.refId === refId) return;
    map[findingId] = refId ? { status, refId } : { status };
  }
  writeDispMap(hostId, reviewSid, map);
}

/** Restore `adopted` -> `open` when a review pill is removed from Composer's ContextRefBar before sending. */
export function unadoptFindingByRef(hostId: string, ref: { sideSessionId?: string; reviewFindingId?: string; id?: string }) {
  if (!hostId) return;
  const reviewSid = ref.sideSessionId;
  const fid = ref.reviewFindingId;
  if (reviewSid && fid) {
    const map = readDispMap(hostId, reviewSid);
    if (map[fid]?.status === 'adopted') {
      delete map[fid];
      writeDispMap(hostId, reviewSid, map);
    }
    return;
  }
  if (!ref.id) return;
  // Fallback scan by refId for older/partial refs
  try {
    if (typeof localStorage === 'undefined') return;
    const prefix = `${DISPOSITION_KEY_PREFIX}${hostId}:`;
    for (let i = 0; i < localStorage.length; i++) {
      const k = localStorage.key(i);
      if (!k || !k.startsWith(prefix)) continue;
      const sid = k.slice(prefix.length);
      const map = readDispMap(hostId, sid);
      let changed = false;
      for (const [f, entry] of Object.entries(map)) {
        if (entry?.status === 'adopted' && entry.refId === ref.id) {
          delete map[f];
          changed = true;
        }
      }
      if (changed) writeDispMap(hostId, sid, map);
    }
  } catch {
    /* ignore */
  }
}

/** Optional short user note stored per finding (reserved for UI annotations). */
export function getFindingDismissKey(hostId: string, reviewSid: string): string {
  return `${DISMISS_REASONS_KEY_PREFIX}${hostId}:${reviewSid}`;
}

/** Event bus for Diff card highlight / scroll-to-finding from ReviewSidePane or Overview bar. */
export interface FocusFindingPayload {
  hostId: string;
  parentSessionId: string;
  reviewSessionId: string;
  findingId: string;
  file?: string;
  line?: number;
}

type FocusListener = (p: FocusFindingPayload) => void;
const focusListeners = new Set<FocusListener>();
let lastFocusPayload: (FocusFindingPayload & { ts: number }) | null = null;

export function emitFocusFinding(p: FocusFindingPayload) {
  lastFocusPayload = { ...p, ts: Date.now() };
  for (const fn of focusListeners) {
    try {
      fn(p);
    } catch {
      /* ignore */
    }
  }
}

export function subscribeFocusFinding(fn: FocusListener): () => void {
  focusListeners.add(fn);
  return () => focusListeners.delete(fn);
}

export function consumeRecentFocusFinding(hostId: string, parentSessionId: string, maxAgeMs = 8000): FocusFindingPayload | null {
  if (!lastFocusPayload) return null;
  if (Date.now() - lastFocusPayload.ts > maxAgeMs) return null;
  if (lastFocusPayload.hostId !== hostId || lastFocusPayload.parentSessionId !== parentSessionId) return null;
  const res = lastFocusPayload;
  lastFocusPayload = null;
  return res;
}

/**
 * Normalize a file path emitted by an AI reviewer into repo-relative slash form.
 * Avoids regex lookbehind for Safari/WebKit 16 compatibility.
 */
export function normalizeReviewPath(
  raw: string | undefined,
  knownFiles?: Iterable<string>,
  cwd?: string,
): string | undefined {
  if (!raw) return undefined;
  let p = raw.trim().replace(/\\/g, '/');
  if (!p) return undefined;

  // Strip surrounding quotes or backticks occasionally produced by models
  if ((p.startsWith('`') && p.endsWith('`')) || (p.startsWith('"') && p.endsWith('"')) || (p.startsWith("'") && p.endsWith("'"))) {
    p = p.slice(1, -1).trim();
  }
  // Strip accidental trailing :line or :line:col (skip Windows drive letter like C:/...)
  const lineSuffix = /^((?:[A-Za-z]:)?[^:]+):(\d+)(?::\d+)?$/.exec(p);
  if (lineSuffix) p = lineSuffix[1];

  while (p.startsWith('./')) p = p.slice(2);

  if (cwd) {
    const cleanCwd = cwd.replace(/\\/g, '/').replace(/\/+$/, '');
    if (cleanCwd && p.startsWith(`${cleanCwd}/`)) {
      p = p.slice(cleanCwd.length + 1);
    }
  }

  const known = knownFiles ? Array.from(knownFiles, (k) => k.replace(/\\/g, '/').replace(/^\.\//, '')) : null;
  if (known && known.length > 0) {
    const knownSet = new Set(known);
    if (knownSet.has(p)) return p;
    if ((p.startsWith('a/') || p.startsWith('b/')) && knownSet.has(p.slice(2))) {
      return p.slice(2);
    }
    const strippedLead = p.replace(/^\/+/, '');
    if (knownSet.has(strippedLead)) return strippedLead;
    const suffixMatches = known.filter((k) => k.endsWith(`/${strippedLead}`) || strippedLead.endsWith(`/${k}`));
    if (suffixMatches.length === 1) return suffixMatches[0];
  }

  return p;
}

/** Parse structured findings from a reviewer's markdown output (returns null if no valid JSON block found). */
export function parseReviewIssues(
  text: string,
  opts?: { knownFiles?: Iterable<string>; cwd?: string; idPrefix?: string },
): { issues: ReviewIssue[]; prose: string } | null {
  if (!text) return null;
  const match =
    /```yi-review-json\s*\r?\n([\s\S]*?)```/i.exec(text) ??
    /```json\s*\r?\n(\s*\[[\s\S]*?\])\s*```/i.exec(text);
  if (!match) return null;
  try {
    const raw = JSON.parse(match[1].trim());
    const arr = Array.isArray(raw) ? raw : raw && typeof raw === 'object' && Array.isArray((raw as { issues?: unknown }).issues) ? (raw as { issues: unknown[] }).issues : null;
    if (!arr) return null;
    const prefix = opts?.idPrefix || 'ri';
    const issues: ReviewIssue[] = [];
    for (let i = 0; i < arr.length; i++) {
      const it = arr[i] as Record<string, unknown> | null;
      if (!it || typeof it !== 'object') continue;
      const sevRaw = String(it.severity ?? 'medium').toLowerCase();
      const severity: ReviewSeverity =
        sevRaw === 'high' || sevRaw === 'critical' || sevRaw === 'error'
          ? 'high'
          : sevRaw === 'low' || sevRaw === 'nit' || sevRaw === 'info'
            ? 'low'
            : 'medium';
      const title = String(it.title ?? '').trim();
      const summary = String(it.summary ?? it.description ?? '').trim();
      if (!title && !summary) continue;
      const rawFile = typeof it.file === 'string' && it.file.trim() ? it.file.trim() : undefined;
      let lineNum = Number(it.line);
      if (!Number.isFinite(lineNum) && rawFile) {
        const lineSuffix = /^((?:[A-Za-z]:)?[^:]+):(\d+)(?::\d+)?$/.exec(rawFile);
        if (lineSuffix) lineNum = Number(lineSuffix[2]);
      }
      const normFile = normalizeReviewPath(rawFile, opts?.knownFiles, opts?.cwd);
      issues.push({
        id: `${prefix}-${i}`,
        severity,
        title: title || summary.slice(0, 60),
        file: normFile,
        rawFile,
        line: Number.isFinite(lineNum) && lineNum > 0 ? Math.floor(lineNum) : undefined,
        summary,
        suggestion: typeof it.suggestion === 'string' && it.suggestion.trim() ? it.suggestion.trim() : undefined,
      });
    }
    const prose = text.replace(match[0], '').trim();
    return { issues, prose };
  } catch {
    return null;
  }
}

/** Extract the active findings snapshot across all finalized assistant turns in a review side chat. */
export function extractReviewSnapshot(
  events: HubEvent[],
  liveText: string,
  busy: boolean,
  opts?: { knownFiles?: Iterable<string>; cwd?: string },
): ReviewSnapshot {
  let latestAssistantText = '';
  let latestStructured: { issues: ReviewIssue[]; prose: string; rawText: string; seq: number; round: number } | null = null;
  let structuredCount = 0;

  for (const ev of events) {
    if (ev.kind !== 'assistant_text') continue;
    const text = ev.text ?? '';
    latestAssistantText = text;
    const parsed = parseReviewIssues(text, {
      ...opts,
      idPrefix: structuredCount === 0 ? 'ri' : `ri${structuredCount}`,
    });
    if (parsed) {
      structuredCount += 1;
      latestStructured = { ...parsed, rawText: text, seq: ev.seq, round: structuredCount };
    }
  }

  if (latestStructured) {
    return {
      issues: latestStructured.issues,
      prose: latestStructured.prose,
      latestText: latestStructured.rawText,
      hasStructuredBlock: true,
      round: latestStructured.round,
      messageSeq: latestStructured.seq,
    };
  }

  if (!busy && liveText.trim()) {
    const parsed = parseReviewIssues(liveText, opts);
    if (parsed) {
      return {
        issues: parsed.issues,
        prose: parsed.prose,
        latestText: liveText,
        hasStructuredBlock: true,
        round: 1,
        messageSeq: 0,
      };
    }
  }

  const proseFallback = latestAssistantText || (!busy ? liveText : '');
  return {
    issues: [],
    prose: proseFallback.trim(),
    latestText: proseFallback,
    hasStructuredBlock: false,
    round: 0,
    messageSeq: 0,
  };
}

function escAttr(s: string): string {
  return s.replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

function unescAttr(s: string): string {
  return s.replace(/&quot;/g, '"').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&amp;/g, '&');
}

function readAttr(attrs: string, name: string): string | undefined {
  const re = new RegExp(`(?:^|\\s)${name}="([^"]*)"`, 'i');
  const m = re.exec(attrs);
  return m ? unescAttr(m[1]) : undefined;
}

/** Tag a user follow-up message in the review side chat so it also renders inside the matching Diff card drawer. */
export function formatFindingFollowUp(ref: FindingRefMeta, question: string): string {
  const attrs = [`finding="${escAttr(ref.findingId)}"`];
  if (ref.file) attrs.push(`file="${escAttr(ref.file)}"`);
  if (ref.line && ref.line > 0) attrs.push(`line="${Math.floor(ref.line)}"`);
  if (ref.title) attrs.push(`title="${escAttr(ref.title)}"`);
  const body = question.trim();
  return body ? `<yi-review-ref ${attrs.join(' ')} />\n${body}` : `<yi-review-ref ${attrs.join(' ')} />`;
}

const REVIEW_REF_TAG_RE = /^\s*<yi-review-ref\s+([^>]*?)\s*\/>\s*/i;

export function parseFindingFollowUp(text: string): { ref: FindingRefMeta | null; text: string } {
  const m = REVIEW_REF_TAG_RE.exec(text);
  if (!m) return { ref: null, text };
  const attrs = m[1];
  const findingId = readAttr(attrs, 'finding');
  if (!findingId) return { ref: null, text };
  const lineRaw = Number(readAttr(attrs, 'line'));
  return {
    ref: {
      findingId,
      file: readAttr(attrs, 'file') || undefined,
      line: Number.isFinite(lineRaw) && lineRaw > 0 ? Math.floor(lineRaw) : undefined,
      title: readAttr(attrs, 'title') || undefined,
    },
    text: text.slice(m[0].length),
  };
}

/**
 * Group bidirectional finding follow-up turns by `findingId`.
 * A user message carrying `<yi-review-ref finding="..." />` starts/continues a finding thread;
 * any subsequent assistant messages (without a new structured `yi-review-json` block) attach to that thread
 * until another user message without `<yi-review-ref>` resets the thread context.
 */
export function extractFindingThreads(events: HubEvent[]): Map<string, FindingThreadMessage[]> {
  const threads = new Map<string, FindingThreadMessage[]>();
  let active: FindingRefMeta | null = null;

  for (const ev of events) {
    if (ev.kind === 'user_message') {
      const parsed = parseFindingFollowUp(ev.text ?? '');
      if (parsed.ref) {
        active = parsed.ref;
        const list = threads.get(parsed.ref.findingId) ?? [];
        list.push({
          seq: ev.seq,
          role: 'user',
          text: parsed.text.trim(),
          ts: ev.ts,
          findingId: parsed.ref.findingId,
          file: parsed.ref.file,
          line: parsed.ref.line,
          title: parsed.ref.title,
        });
        threads.set(parsed.ref.findingId, list);
      } else {
        active = null;
      }
      continue;
    }

    if (ev.kind === 'turn_end') {
      active = null;
      continue;
    }

    if (ev.kind === 'assistant_text' && active) {
      const text = (ev.text ?? '').trim();
      if (!text) continue;
      if (parseReviewIssues(text)) {
        active = null;
        continue;
      }
      const list = threads.get(active.findingId) ?? [];
      list.push({
        seq: ev.seq,
        role: 'assistant',
        text,
        ts: ev.ts,
        findingId: active.findingId,
        file: active.file,
        line: active.line,
        title: active.title,
      });
      threads.set(active.findingId, list);
    }
  }

  return threads;
}

/** Detect if the review side chat is currently streaming a response to a `<yi-review-ref>` follow-up. */
export function getActiveFindingStream(
  events: HubEvent[],
  liveText: string,
  busy: boolean,
): { findingId: string; liveText: string } | null {
  if (!busy) return null;
  let activeFindingId: string | null = null;
  for (const ev of events) {
    if (ev.kind === 'user_message') {
      const { ref } = parseFindingFollowUp(ev.text ?? '');
      activeFindingId = ref?.findingId ?? null;
    } else if (ev.kind === 'turn_end' || (ev.kind === 'assistant_text' && parseReviewIssues(ev.text ?? ''))) {
      activeFindingId = null;
    }
  }
  if (!activeFindingId) return null;
  return { findingId: activeFindingId, liveText };
}

/** Aggregate per-file issue counts (preserving severity and live open/adopted/dismissed status). */
export function computeFileIssueBadges(
  issues: ReviewIssue[],
  dispositions: Record<string, FindingDisposition>,
  knownFiles?: Iterable<string>,
  cwd?: string,
): Map<string, FileIssueBadge> {
  const map = new Map<string, FileIssueBadge>();
  for (const it of issues) {
    const file = normalizeReviewPath(it.file, knownFiles, cwd);
    if (!file) continue;
    let b = map.get(file);
    if (!b) {
      b = { high: 0, medium: 0, low: 0, total: 0, open: 0, adopted: 0, dismissed: 0 };
      map.set(file, b);
    }
    const disp = dispositions[it.id] ?? 'open';
    if (disp === 'dismissed') {
      b.dismissed += 1;
      continue;
    }
    if (disp === 'adopted') b.adopted += 1;
    else b.open += 1;
    b[it.severity] += 1;
    b.total += 1;
  }
  return map;
}
