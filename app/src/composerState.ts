import type { ContextRef } from './contextRefs';

export interface Attached {
  mimeType: string;
  data: string;
  url: string;
  name: string;
}

export interface Upload {
  id: number;
  name: string;
  sent: number;
  total: number;
}

export interface DraftSnapshot {
  text: string;
  imgs: Attached[];
  refs: ContextRef[];
}

export interface ReeditState {
  nonce: number;
  loadingImages: number;
  stashedDraft: DraftSnapshot | null;
  dirty: boolean;
}

export interface ComposerSnapshot {
  text: string;
  imgs: Attached[];
  refs: ContextRef[];
  sending: boolean;
  uploads: Upload[];
  pendingUploads: number;
  waitSend: boolean;
  reedit: ReeditState | null;
}

export interface ComposerHandlers {
  insertAtCaret: (s: string) => void;
  focus: () => void;
  focusAtEnd?: () => void;
}

interface ComposerEntry {
  text: string;
  imgs: Attached[];
  refs: ContextRef[];
  sending: boolean;
  uploads: Upload[];
  pendingUploads: number;
  waitSend: boolean;
  waitSendRunner: (() => void) | null;
  cancelledUploads: Set<number>;
  reedit: ReeditState | null;
  snap: ComposerSnapshot;
}

const entries = new Map<string, ComposerEntry>();
const subs = new Map<string, Set<() => void>>();
const activeHandlers = new Map<string, ComposerHandlers>();
let reeditNonce = 0;

const draftTextKey = (key: string) => `draft-${key}`;
const draftRefsKey = (key: string) => `draft-refs-${key}`;

function readDraftText(key: string): string {
  if (typeof sessionStorage === 'undefined') return '';
  try {
    return sessionStorage.getItem(draftTextKey(key)) ?? '';
  } catch {
    return '';
  }
}

function writeDraftText(key: string, v: string) {
  if (typeof sessionStorage === 'undefined') return;
  try {
    if (v) sessionStorage.setItem(draftTextKey(key), v);
    else sessionStorage.removeItem(draftTextKey(key));
  } catch {
    /* ignore */
  }
}

function readDraftRefs(key: string): ContextRef[] {
  if (typeof sessionStorage === 'undefined') return [];
  try {
    const raw = sessionStorage.getItem(draftRefsKey(key));
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? (parsed as ContextRef[]) : [];
  } catch {
    return [];
  }
}

function writeDraftRefs(key: string, list: ContextRef[]) {
  if (typeof sessionStorage === 'undefined') return;
  try {
    if (list.length) sessionStorage.setItem(draftRefsKey(key), JSON.stringify(list));
    else sessionStorage.removeItem(draftRefsKey(key));
  } catch {
    /* ignore */
  }
}

const EMPTY_IMGS: Attached[] = [];
const EMPTY_REFS: ContextRef[] = [];
const EMPTY_UPLOADS: Upload[] = [];
const EMPTY_SNAPSHOT: ComposerSnapshot = {
  text: '',
  imgs: EMPTY_IMGS,
  refs: EMPTY_REFS,
  sending: false,
  uploads: EMPTY_UPLOADS,
  pendingUploads: 0,
  waitSend: false,
  reedit: null,
};

function makeSnapshot(e: ComposerEntry): ComposerSnapshot {
  if (
    !e.text &&
    !e.imgs.length &&
    !e.refs.length &&
    !e.sending &&
    !e.uploads.length &&
    e.pendingUploads === 0 &&
    !e.waitSend &&
    !e.reedit
  ) {
    return EMPTY_SNAPSHOT;
  }
  return {
    text: e.text,
    imgs: e.imgs,
    refs: e.refs,
    sending: e.sending,
    uploads: e.uploads,
    pendingUploads: e.pendingUploads,
    waitSend: e.waitSend,
    reedit: e.reedit,
  };
}

function isEntryIdle(e: ComposerEntry): boolean {
  return (
    !e.text &&
    !e.imgs.length &&
    !e.refs.length &&
    !e.sending &&
    !e.uploads.length &&
    e.pendingUploads === 0 &&
    !e.waitSend &&
    !e.waitSendRunner &&
    e.cancelledUploads.size === 0 &&
    !e.reedit
  );
}

function createEntry(text: string, refs: ContextRef[]): ComposerEntry {
  const e: ComposerEntry = {
    text,
    imgs: EMPTY_IMGS,
    refs,
    sending: false,
    uploads: EMPTY_UPLOADS,
    pendingUploads: 0,
    waitSend: false,
    waitSendRunner: null,
    cancelledUploads: new Set(),
    reedit: null,
    snap: EMPTY_SNAPSHOT,
  };
  e.snap = makeSnapshot(e);
  return e;
}

function ensureEntry(key: string): ComposerEntry {
  let e = entries.get(key);
  if (!e) {
    e = createEntry(readDraftText(key), readDraftRefs(key));
    entries.set(key, e);
  }
  return e;
}

function notify(key: string, e: ComposerEntry) {
  e.snap = makeSnapshot(e);
  const s = subs.get(key);
  if (s && s.size > 0) {
    for (const fn of [...s]) fn();
  } else if (isEntryIdle(e)) {
    entries.delete(key);
  }
}

export function getComposerSnapshot(key: string): ComposerSnapshot {
  const existing = entries.get(key);
  if (existing) return existing.snap;
  const text = readDraftText(key);
  const refs = readDraftRefs(key);
  if (!text && refs.length === 0) return EMPTY_SNAPSHOT;
  const e = createEntry(text, refs);
  entries.set(key, e);
  return e.snap;
}

export function subscribeComposer(key: string, fn: () => void): () => void {
  let s = subs.get(key);
  if (!s) subs.set(key, (s = new Set()));
  s.add(fn);
  return () => {
    const cur = subs.get(key);
    if (!cur) return;
    cur.delete(fn);
    if (cur.size === 0) {
      subs.delete(key);
      const e = entries.get(key);
      if (e && isEntryIdle(e)) entries.delete(key);
    }
  };
}

function markReeditDirty(e: ComposerEntry) {
  if (e.reedit && !e.reedit.dirty) {
    e.reedit = { ...e.reedit, dirty: true };
  }
}

export function setComposerText(key: string, next: string | ((cur: string) => string)) {
  const e = ensureEntry(key);
  const val = typeof next === 'function' ? next(e.text) : next;
  if (val === e.text) return;
  e.text = val;
  markReeditDirty(e);
  writeDraftText(key, val);
  notify(key, e);
}

export function appendComposerText(key: string, fragment: string) {
  if (!fragment) return;
  setComposerText(key, (cur) => (cur && !/\s$/.test(cur) ? `${cur} ` : cur) + fragment);
}

export function setComposerImages(key: string, next: Attached[] | ((cur: Attached[]) => Attached[])) {
  const e = ensureEntry(key);
  const val = typeof next === 'function' ? next(e.imgs) : next;
  if (val === e.imgs) return;
  e.imgs = val;
  markReeditDirty(e);
  notify(key, e);
}

export function setComposerRefs(key: string, next: ContextRef[] | ((cur: ContextRef[]) => ContextRef[])) {
  const e = ensureEntry(key);
  const val = typeof next === 'function' ? next(e.refs) : next;
  if (val === e.refs) return;
  e.refs = val;
  markReeditDirty(e);
  writeDraftRefs(key, val);
  notify(key, e);
}

export function beginSend(key: string): boolean {
  const e = ensureEntry(key);
  if (e.sending) return false;
  e.sending = true;
  notify(key, e);
  return true;
}

export function completeSendSuccess(
  key: string,
  sent: { sentText: string; sentImgs: readonly Attached[]; sentRefs: readonly ContextRef[]; reeditNonce?: number },
) {
  const e = ensureEntry(key);
  if (e.text.trim() === sent.sentText) {
    e.text = '';
    writeDraftText(key, '');
  }
  if (sent.sentImgs.length > 0) {
    const imgSet = new Set(sent.sentImgs);
    e.imgs = e.imgs.filter((x) => !imgSet.has(x));
  }
  if (sent.sentRefs.length > 0) {
    const refIds = new Set(sent.sentRefs.map((r) => r.id));
    e.refs = e.refs.filter((r) => !refIds.has(r.id));
    writeDraftRefs(key, e.refs);
  }
  if (e.reedit && (sent.reeditNonce === undefined || e.reedit.nonce === sent.reeditNonce)) {
    e.reedit = null;
  }
  e.sending = false;
  notify(key, e);
}

export function endSend(key: string) {
  const e = entries.get(key);
  if (!e || !e.sending) return;
  e.sending = false;
  notify(key, e);
}

export function beginUploadJob(key: string) {
  const e = ensureEntry(key);
  e.pendingUploads += 1;
  notify(key, e);
}

export function addComposerUpload(key: string, up: Upload) {
  const e = ensureEntry(key);
  e.uploads = [...e.uploads, up];
  notify(key, e);
}

export function updateComposerUpload(key: string, id: number, sent: number) {
  const e = entries.get(key);
  if (!e) return;
  const idx = e.uploads.findIndex((x) => x.id === id);
  if (idx < 0 || e.uploads[idx].sent === sent) return;
  e.uploads = e.uploads.map((x) => (x.id === id ? { ...x, sent } : x));
  notify(key, e);
}

export function removeComposerUpload(key: string, id: number) {
  const e = entries.get(key);
  if (!e) return;
  const hadUpload = e.uploads.some((x) => x.id === id);
  const hadCancel = e.cancelledUploads.delete(id);
  if (!hadUpload && !hadCancel) return;
  if (hadUpload) e.uploads = e.uploads.filter((x) => x.id !== id);
  notify(key, e);
}

export function cancelComposerUpload(key: string, id: number) {
  const e = ensureEntry(key);
  e.cancelledUploads.add(id);
  e.uploads = e.uploads.filter((x) => x.id !== id);
  e.waitSend = false;
  e.waitSendRunner = null;
  notify(key, e);
}

export function isComposerUploadCancelled(key: string, id: number): boolean {
  return entries.get(key)?.cancelledUploads.has(id) ?? false;
}

export function armWaitSend(key: string, runner: () => void) {
  const e = ensureEntry(key);
  e.waitSend = true;
  e.waitSendRunner = runner;
  notify(key, e);
}

export function clearWaitSend(key: string) {
  const e = entries.get(key);
  if (!e || (!e.waitSend && !e.waitSendRunner)) return;
  e.waitSend = false;
  e.waitSendRunner = null;
  notify(key, e);
}

export function endUploadJob(key: string): (() => void) | null {
  const e = entries.get(key);
  if (!e) return null;
  e.pendingUploads = Math.max(0, e.pendingUploads - 1);
  let runner: (() => void) | null = null;
  if (e.pendingUploads === 0 && e.uploads.length === 0 && e.waitSend) {
    runner = e.waitSendRunner;
    e.waitSend = false;
    e.waitSendRunner = null;
  }
  notify(key, e);
  return runner;
}

export function registerActiveComposer(key: string, handlers: ComposerHandlers): () => void {
  activeHandlers.set(key, handlers);
  return () => {
    if (activeHandlers.get(key) === handlers) activeHandlers.delete(key);
  };
}

export function insertOrAppendComposerText(key: string, fragment: string) {
  if (!fragment) return;
  const h = activeHandlers.get(key);
  if (h) h.insertAtCaret(fragment);
  else appendComposerText(key, fragment);
}

export function focusComposer(key: string) {
  activeHandlers.get(key)?.focus();
}

export function focusComposerAtEnd(key: string) {
  const h = activeHandlers.get(key);
  if (h?.focusAtEnd) h.focusAtEnd();
  else h?.focus();
}

export function beginComposerReedit(
  key: string,
  payload: { text: string; refs: ContextRef[]; loadingImages?: number },
): { nonce: number; hasStashedDraft: boolean } {
  const e = ensureEntry(key);
  const stashedDraft: DraftSnapshot | null =
    e.reedit && !e.reedit.dirty
      ? e.reedit.stashedDraft
      : e.text.trim() || e.imgs.length > 0 || e.refs.length > 0
        ? { text: e.text, imgs: [...e.imgs], refs: [...e.refs] }
        : null;
  const nonce = ++reeditNonce;
  e.text = payload.text;
  e.imgs = [];
  e.refs = [...payload.refs];
  e.reedit = {
    nonce,
    loadingImages: payload.loadingImages ?? 0,
    stashedDraft,
    dirty: false,
  };
  writeDraftText(key, e.text);
  writeDraftRefs(key, e.refs);
  notify(key, e);
  return { nonce, hasStashedDraft: stashedDraft !== null };
}

export function finishReeditImages(key: string, nonce: number, imgs: Attached[]) {
  const e = entries.get(key);
  if (!e || !e.reedit || e.reedit.nonce !== nonce) return;
  if (imgs.length > 0) {
    e.imgs = [...imgs, ...e.imgs];
  }
  e.reedit = { ...e.reedit, loadingImages: 0 };
  notify(key, e);
}

export function restoreReeditDraft(key: string, expectedNonce?: number): boolean {
  const e = entries.get(key);
  if (!e?.reedit?.stashedDraft) return false;
  if (expectedNonce !== undefined && e.reedit.nonce !== expectedNonce) return false;
  const { text, imgs, refs } = e.reedit.stashedDraft;
  e.text = text;
  e.imgs = [...imgs];
  e.refs = [...refs];
  e.reedit = null;
  writeDraftText(key, e.text);
  writeDraftRefs(key, e.refs);
  notify(key, e);
  return true;
}

export function cancelComposerReedit(key: string) {
  const e = entries.get(key);
  if (!e?.reedit) return;
  e.text = '';
  e.imgs = EMPTY_IMGS;
  e.refs = EMPTY_REFS;
  e.reedit = null;
  writeDraftText(key, '');
  writeDraftRefs(key, []);
  notify(key, e);
}

export function dismissComposerReedit(key: string) {
  const e = entries.get(key);
  if (!e?.reedit) return;
  e.reedit = null;
  notify(key, e);
}

export function clearComposerState(key: string) {
  const e = entries.get(key);
  if (!e) {
    writeDraftText(key, '');
    writeDraftRefs(key, []);
    return;
  }
  for (const u of e.uploads) e.cancelledUploads.add(u.id);
  e.text = '';
  e.imgs = EMPTY_IMGS;
  e.refs = EMPTY_REFS;
  e.uploads = EMPTY_UPLOADS;
  e.waitSend = false;
  e.waitSendRunner = null;
  e.reedit = null;
  writeDraftText(key, '');
  writeDraftRefs(key, []);
  notify(key, e);
}

export function _resetComposerStateForTests() {
  entries.clear();
  subs.clear();
  activeHandlers.clear();
  reeditNonce = 0;
}
