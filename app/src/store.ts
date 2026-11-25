import { lsGet, lsSet } from './util';
import { clearComposerState, dismissComposerReedit } from './composerState';
// Global state: hosts, session list and the current view live in one small store;
// each session's event stream is stored separately (large, changes often) and subscribers are notified per session.

import { useSyncExternalStore } from 'react';
import { getLang, getT, onLangChange, trErr } from './i18n';
import { fmtAt } from './limit';
import { hubCall, onHubEvent, rpc, isTauri } from './transport';
import { markSeen, recordChange } from './overview';
import { dropRemoteHost, findBot, mapHost, refreshBots, refreshRemote, refreshRemoteHost, remoteKey, sourceOf } from './bots';
import { isHostSession, type AccountLoginUpdate, type ClientSettings, type HostConfig, type HostStatus, type HubEvent, type NotifyPrefs, type SessionInfo, type Snippet, type ToolInfo, type ToolJob, type ToolKind, type UserMessageSummary } from './types';

const t = getT('store');

export type View =
  | { kind: 'home' }
  | { kind: 'hosts'; editId?: string }
  /** seq: scroll to this message after opening (jumped here from global search) */
  | { kind: 'session'; hostId: string; sessionId: string; seq?: number }
  | { kind: 'search'; q?: string }
  /** cwd: only history sessions under this directory (including subdirectories) */
  | { kind: 'history'; hostId: string; cwd?: string }
  | { kind: 'files'; hostId: string; path?: string }
  | { kind: 'stats'; hostId: string }
  | { kind: 'ports'; hostId: string }
  | { kind: 'memory'; hostId: string; cwd?: string }
  | { kind: 'settings'; hostId: string }
  | { kind: 'prefs' }
  /** A Bot's details. src: the Bots source (bots.ts), '' or absent for this app */
  | { kind: 'bot'; botId: string; src?: string }
  /** Bot connectors and their instances */
  | { kind: 'connectors'; src?: string; review?: string }
  /** Every event a Bot received */
  | { kind: 'botEvents'; botId: string; src?: string }
  /** New session page (`NewSession.tsx`) */
  | ({ kind: 'new' } & NewSessionParams);

/** What the new session page opens with */
export interface NewSessionParams {
  hostId: string;
  cwd?: string;
  tool?: string;
  resumeId?: string;
  title?: string;
  /** Continue with another tool: the original session's tool (the new session can't pick it), the prefilled first
   *  message, the original session (`session.create` handoffFrom), its permission mode (kept if the new tool has it,
   *  else default, never raised) and tools to avoid as the default pick (sessions of theirs wait for a limit reset) */
  handoff?: { from: ToolKind; prompt: string; fromId?: string; perm?: string | null; avoid?: ToolKind[] };
  /** Create a Bot of this Bots source ('' for this app) instead of a session: the first message is its role */
  bot?: { src: string };
}

export interface HostEntry {
  config: HostConfig;
  status: HostStatus;
}

export interface Toast {
  id: number;
  text: string;
  kind: 'info' | 'error' | 'loading';
  action?: { label: string; run: () => void };
}

export interface Upgrade {
  /** waiting: waiting for the turn to end; uploading / restarting: upgrading, UI temporarily unusable */
  phase: 'waiting' | 'uploading' | 'restarting' | 'done' | 'error';
  message: string;
  progress?: number | null;
}

export interface State {
  hosts: HostEntry[];
  localSupported: boolean;
  sessions: Record<string, SessionInfo[]>;
  view: View;
  deploy: Record<string, string[]>;
  /** yi-agentd upgrade progress, per host */
  upgrades: Record<string, Upgrade>;
  /** Tool upgrade jobs (`tools.updating`), per host and tool */
  toolJobs: Record<string, Record<string, ToolJob>>;
  toasts: Toast[];
  transport: 'open' | 'closed' | 'connecting';
  sidebarOpen: boolean;
  /** This device's settings (proxy, notifications), stored in the client data directory */
  client: ClientSettings | null;
  detectedProxy: string | null;
  /** Ctrl+K command palette */
  palette: boolean;
  /** Desktop: sidebar collapsed */
  sidebarHidden: boolean;
  /** Whether back / forward are possible (title bar buttons) */
  canBack: boolean;
  canForward: boolean;
}

let state: State = {
  hosts: [],
  localSupported: true,
  sessions: {},
  view: { kind: 'home' },
  deploy: {},
  upgrades: {},
  toolJobs: {},
  toasts: [],
  transport: 'connecting',
  sidebarOpen: false,
  client: null,
  detectedProxy: null,
  palette: false,
  sidebarHidden: (() => {
    try {
      return localStorage.getItem('yi-console-sidebar-hidden') === '1';
    } catch {
      return false;
    }
  })(),
  canBack: false,
  canForward: false,
};

const subs = new Set<() => void>();

export function getState() {
  return state;
}

export function setState(patch: Partial<State> | ((s: State) => Partial<State>)) {
  const p = typeof patch === 'function' ? patch(state) : patch;
  state = { ...state, ...p };
  for (const f of subs) f();
}

export function useStore<T>(sel: (s: State) => T): T {
  return useSyncExternalStore(
    (cb) => {
      subs.add(cb);
      return () => subs.delete(cb);
    },
    () => sel(state),
  );
}

// ---------- Session event streams ----------

export interface SessionData {
  events: HubEvent[];
  seqs: Set<number>;
  /** Lightweight index of all user messages in the session (for prompt outline navigation) */
  userMessages: UserMessageSummary[];
  userMessagesLoaded: boolean;
  userMessagesLoading: boolean;
  /** Older remote yi-agentd lacks session.userMessages; fall back to loaded events */
  userMessagesUnsupported: boolean;
  /** Latest cleared event seq so in-flight RPCs don't reintroduce pre-clear prompts */
  clearSeq: number;
  /** Streaming assistant text: id -> text received so far */
  live: Map<string, string>;
  /** Streaming thinking */
  liveThinking: Map<string, string>;
  loaded: boolean;
  loading: boolean;
  /** Opening only fetched the most recent part; older events are still on yi-agentd */
  hasMore: boolean;
  loadingOlder: boolean;
  version: number;
  /** Incremented only when diff-bearing events (tool_call diffs, cleared, force reload) change */
  diffVersion: number;
  /** Consumer of terminal output (attached by TerminalView); reset=true means clear the screen before writing the snapshot */
  term?: (data: Uint8Array, reset: boolean) => void;
  termBacklog: Uint8Array[];
}

const sessionData = new Map<string, SessionData>();
const sessionSubs = new Map<string, Set<() => void>>();

export const skey = (hostId: string, sid: string) => `${hostId}::${sid}`;

export function getSession(key: string): SessionData {
  let d = sessionData.get(key);
  if (!d) {
    d = {
      events: [],
      seqs: new Set(),
      userMessages: [],
      userMessagesLoaded: false,
      userMessagesLoading: false,
      userMessagesUnsupported: false,
      clearSeq: 0,
      live: new Map(),
      liveThinking: new Map(),
      loaded: false,
      loading: false,
      hasMore: false,
      loadingOlder: false,
      version: 0,
      diffVersion: 0,
      termBacklog: [],
    };
    sessionData.set(key, d);
  }
  return d;
}

function mergeUserMessages(cur: UserMessageSummary[], incoming: UserMessageSummary[], clearSeq: number): UserMessageSummary[] {
  const bySeq = new Map<number, UserMessageSummary>();
  for (const m of cur) {
    if (m.seq > clearSeq) bySeq.set(m.seq, m);
  }
  for (const m of incoming) {
    if (m.seq > clearSeq) bySeq.set(m.seq, m);
  }
  return [...bySeq.values()].sort((a, b) => a.seq - b.seq);
}

function bump(key: string) {
  const d = getSession(key);
  d.version++;
  for (const f of sessionSubs.get(key) ?? []) f();
}

export function useSessionData(key: string): SessionData {
  useSyncExternalStore(
    (cb) => {
      let s = sessionSubs.get(key);
      if (!s) sessionSubs.set(key, (s = new Set()));
      s.add(cb);
      return () => s!.delete(cb);
    },
    () => getSession(key).version,
  );
  return getSession(key);
}

const EMPTY_EVENTS: HubEvent[] = [];

/** Subscribe only to session diff-bearing events / clears (avoids streaming text delta re-renders) */
export function useSessionDiffEvents(key: string, enabled: boolean): HubEvent[] {
  useSyncExternalStore(
    (cb) => {
      if (!enabled || !key) return () => {};
      let s = sessionSubs.get(key);
      if (!s) sessionSubs.set(key, (s = new Set()));
      s.add(cb);
      return () => s!.delete(cb);
    },
    () => (enabled && key ? getSession(key).diffVersion : -1),
  );
  return enabled && key ? getSession(key).events : EMPTY_EVENTS;
}

function addEvents(key: string, evs: HubEvent[]) {
  const d = getSession(key);
  let changed = false;
  let diffChanged = false;
  const addedUsers: UserMessageSummary[] = [];
  for (const e of evs) {
    if (e.kind === 'assistant_delta') {
      d.live.set(e.id, (d.live.get(e.id) ?? '') + e.delta);
      changed = true;
      continue;
    }
    if (e.kind === 'thinking_delta') {
      d.liveThinking.set(e.id, (d.liveThinking.get(e.id) ?? '') + e.delta);
      changed = true;
      continue;
    }
    if (e.seq > 0) {
      if (d.seqs.has(e.seq)) continue;
      d.seqs.add(e.seq);
    }
    // The final text arrived, clear the matching streaming buffer (claude block ids are "messageId:index")
    if (e.kind === 'assistant_text' || e.kind === 'thinking') {
      const m = e.kind === 'thinking' ? d.liveThinking : d.live;
      for (const id of [...m.keys()]) {
        if (e.id === id || e.id.startsWith(id + ':')) m.delete(id);
      }
    }
    // Once thinking ends and the reply body starts, streaming thinking is over too
    if (e.kind === 'assistant_text' || e.kind === 'tool_call') d.liveThinking.clear();
    if (e.kind === 'turn_end' || (e.kind === 'status' && (e.status === 'exited' || e.status === 'error'))) {
      d.live.clear();
      d.liveThinking.clear();
    }
    if (e.kind === 'user_message' && e.seq > 0 && e.seq > d.clearSeq) {
      addedUsers.push({
        seq: e.seq,
        ts: e.ts,
        text: e.text,
        images: e.images?.length || 0,
        origin: e.origin,
      });
    }
    if (e.kind === 'tool_call' && e.diffs?.length) diffChanged = true;
    d.events.push(e);
    changed = true;
  }
  if (changed) {
    // Backlog and live events may arrive interleaved, so sort by seq
    d.events.sort((a, b) => a.seq - b.seq);
    // Everything before /clear has been deleted: keep only what's after the last clear, and there's no need to page further back
    let cut = -1;
    for (let i = d.events.length - 1; i >= 0; i--) if (d.events[i].kind === 'cleared') { cut = i; break; }
    if (cut >= 0) {
      diffChanged = true;
      const clearedAt = d.events[cut].seq;
      d.clearSeq = Math.max(d.clearSeq, clearedAt);
      for (const e of d.events.slice(0, cut + 1)) d.seqs.delete(e.seq);
      d.seqs.add(clearedAt);
      d.events = d.events.slice(cut + 1);
      d.hasMore = false;
      d.userMessages = d.userMessages.filter((m) => m.seq > d.clearSeq);
      d.userMessagesLoaded = true;
      d.live.clear();
      d.liveThinking.clear();
      dismissComposerReedit(key);
    }
    if (addedUsers.length > 0) {
      d.userMessages = mergeUserMessages(d.userMessages, addedUsers, d.clearSeq);
    }
    if (diffChanged) d.diffVersion++;
    bump(key);
  }
}

function writeTerm(key: string, data: Uint8Array, reset: boolean) {
  const d = getSession(key);
  if (d.term) d.term(data, reset);
  else {
    if (reset) d.termBacklog = [];
    d.termBacklog.push(data);
  }
}

function b64(s: string): Uint8Array {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** How many recent events to fetch when opening a session (adjustable in system settings under "Chat appearance", remembered per device); paging back fetches this many each time */
export const TAIL_OPTIONS = [100, 200, 500, 1000];
export const DEFAULT_TAIL = 200;
export function historyTail(): number {
  const n = Number(lsGet('history-tail', DEFAULT_TAIL));
  return TAIL_OPTIONS.includes(n) ? n : DEFAULT_TAIL;
}
export function setHistoryTail(n: number) {
  lsSet('history-tail', n);
}

/** Fetch the lightweight index of all user messages for prompt outline navigation (silently degrades on older daemons) */
export async function loadUserMessages(hostId: string, sid: string, force = false): Promise<void> {
  const key = skey(hostId, sid);
  const d = getSession(key);
  if (d.userMessagesUnsupported && !force) return;
  if ((d.userMessagesLoaded || d.userMessagesLoading) && !force) return;
  if (d.loaded && !d.hasMore && !force) {
    const fromEvents: UserMessageSummary[] = [];
    for (const e of d.events) {
      if (e.kind === 'user_message' && e.seq > 0 && e.seq > d.clearSeq) {
        fromEvents.push({
          seq: e.seq,
          ts: e.ts,
          text: e.text,
          images: e.images?.length || 0,
          origin: e.origin,
        });
      }
    }
    d.userMessages = mergeUserMessages(d.userMessages, fromEvents, d.clearSeq);
    d.userMessagesLoaded = true;
    bump(key);
    return;
  }
  d.userMessagesLoading = true;
  bump(key);
  try {
    const r = await rpc<{ items?: UserMessageSummary[] }>(hostId, 'session.userMessages', { sessionId: sid });
    d.userMessages = mergeUserMessages(d.userMessages, r.items ?? [], d.clearSeq);
    d.userMessagesLoaded = true;
    d.userMessagesUnsupported = false;
  } catch (e) {
    const msg = errText(e).toLowerCase();
    if (msg.includes('未知方法') || msg.includes('unknown method') || msg.includes('method not found')) {
      d.userMessagesUnsupported = true;
    }
  } finally {
    d.userMessagesLoading = false;
    bump(key);
  }
}

/** Page back: fetch historyTail() events (or enough to reach targetSeq in one call) before the current earliest one */
export async function loadOlder(hostId: string, sid: string, targetSeq?: number) {
  const key = skey(hostId, sid);
  const d = getSession(key);
  if (!d.hasMore || d.loadingOlder) return;
  d.loadingOlder = true;
  bump(key);
  try {
    const first = d.events.find((e) => e.seq > 0)?.seq;
    const limit =
      targetSeq != null && first != null && first > targetSeq
        ? Math.min(20000, Math.max(historyTail(), first - targetSeq + 80))
        : historyTail();
    const r = await rpc(hostId, 'session.history', { sessionId: sid, beforeSeq: first, limit });
    d.hasMore = !!r.hasMore;
    addEvents(key, r.events);
  } finally {
    d.loadingOlder = false;
    bump(key);
  }
}

export async function loadSession(hostId: string, sid: string, force = false) {
  const key = skey(hostId, sid);
  const d = getSession(key);
  if ((d.loaded || d.loading) && !force) return;
  d.loading = true;
  bump(key);
  try {
    let since = 0;
    if (!force) for (const x of d.seqs) if (x > since) since = x;
    // The first open only fetches the latest historyTail() events (200 by default), so long sessions (lots of tool output) open quickly even over SSH/relay; older ones are fetched when paging back
    const r = await rpc(hostId, 'session.subscribe', { sessionId: sid, sinceSeq: since, ...(since === 0 ? { tail: historyTail() } : {}) });
    if (since === 0) d.hasMore = !!r.hasMore;
    if (force) {
      d.events = [];
      d.seqs.clear();
      d.userMessages = [];
      d.userMessagesLoaded = false;
      d.userMessagesUnsupported = false;
      d.clearSeq = 0;
      d.live.clear();
      d.liveThinking.clear();
      d.diffVersion++;
    }
    addEvents(key, r.events);
    if (r.terminal !== undefined) writeTerm(key, b64(r.terminal), true);
    upsertSession(hostId, r.info);
    d.loaded = true;
    if (!d.hasMore) {
      d.userMessagesLoaded = true;
    } else {
      loadUserMessages(hostId, sid).catch(() => {});
    }
  } finally {
    d.loading = false;
    bump(key);
  }
}

function maxSeq(d: SessionData) {
  let m = 0;
  for (const x of d.seqs) if (x > m) m = x;
  return m;
}

const catching = new Set<string>();
const lastCatch = new Map<string, number>();

/**
 * Catch up the opened sessions from the largest seq actually in the UI. ClientHub also catches up on reconnect, but it counts from the seq it received itself:
 * when the WebView is paused while the phone is in the background, events reach Rust but not the UI, or a failed catch-up on reconnect makes it drop the subscription, leaving the session stuck on old content.
 * On failure, mark it unloaded so it reloads next time it's opened (or on the session page currently being viewed)
 */
async function catchUp(hostId: string, sid: string) {
  const key = skey(hostId, sid);
  const d = sessionData.get(key);
  if (!d || !d.loaded || d.loading || catching.has(key)) return;
  catching.add(key);
  lastCatch.set(key, Date.now());
  try {
    const r = await rpc(hostId, 'session.subscribe', { sessionId: sid, sinceSeq: maxSeq(d) });
    addEvents(key, r.events ?? []);
    if (r.info) upsertSession(hostId, r.info);
  } catch {
    d.loaded = false;
    bump(key);
  } finally {
    catching.delete(key);
  }
}

function catchUpHost(hostId: string) {
  const prefix = `${hostId}::`;
  for (const key of sessionData.keys()) if (key.startsWith(prefix)) catchUp(hostId, key.slice(prefix.length));
}

// Catch up once when returning to the foreground (phone back from background, desktop restored from minimized)
if (typeof document !== 'undefined') {
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState !== 'visible') return;
    for (const h of state.hosts) if (h.status.status === 'connected') catchUpHost(h.config.id);
  });
}

// ---------- Hosts and session list ----------

export async function refreshHosts() {
  const r = await hubCall<{ hosts: HostEntry[]; localSupported: boolean }>('hosts.list');
  const hosts = r.hosts.map((h) => ({ ...h, status: h.status ?? { status: 'disconnected' as const } }));
  setState({ hosts, localSupported: r.localSupported });
  for (const h of hosts) if (h.status.status === 'connected') {
    refreshSessions(h.config.id);
    reportLang(h.config.id);
    refreshRemoteHost(h.config.id);
  }
  return hosts;
}

/** Tell yi-agentd the UI language, so the "continue" it sends by itself after a usage limit matches it. Older daemons lack the method */
function reportLang(hostId: string) {
  rpc(hostId, 'settings.uiLang', { lang: getLang() }).catch(() => {});
}

export async function refreshSessions(hostId: string) {
  try {
    const list = await rpc<SessionInfo[]>(hostId, 'session.list');
    const before = new Map((state.sessions[hostId] ?? []).map((x) => [x.id, x]));
    for (const x of list) recordChange(hostId, before.get(x.id), x);
    setState((s) => ({ sessions: { ...s.sessions, [hostId]: list } }));
  } catch {
    /* host not connected */
  }
}

function upsertSession(hostId: string, info: SessionInfo) {
  recordChange(hostId, state.sessions[hostId]?.find((x) => x.id === info.id), info);
  setState((s) => {
    const list = [...(s.sessions[hostId] ?? [])];
    const i = list.findIndex((x) => x.id === info.id);
    if (i >= 0) list[i] = info;
    else list.unshift(info);
    list.sort((a, b) => b.updatedAt - a.updatedAt);
    return { sessions: { ...s.sessions, [hostId]: list } };
  });
}

export function hostStatus(hostId: string): HostStatus {
  return state.hosts.find((h) => h.config.id === hostId)?.status ?? {};
}

/** Re-detect installed tools on a host (yi-agentd rebuilds its PATH first) and update the host's tool list */
export async function redetectTools(hostId: string): Promise<void> {
  setHostTools(hostId, await rpc<ToolInfo[]>(hostId, 'tools.detect'));
}

export function setHostTools(hostId: string, tools: ToolInfo[]) {
  setState((s) => ({
    hosts: s.hosts.map((h) =>
      h.config.id === hostId && h.status.hello ? { ...h, status: { ...h.status, hello: { ...h.status.hello, tools } } } : h,
    ),
  }));
}

const accountListeners = new Set<(hostId: string, tool?: ToolKind) => void>();
const loginListeners = new Set<(hostId: string, upd: AccountLoginUpdate) => void>();

export function onAccountsUpdated(fn: (hostId: string, tool?: ToolKind) => void): () => void {
  accountListeners.add(fn);
  return () => accountListeners.delete(fn);
}

export function onAccountLogin(fn: (hostId: string, upd: AccountLoginUpdate) => void): () => void {
  loginListeners.add(fn);
  return () => loginListeners.delete(fn);
}

/** Live views of AI browsers: `browser.frame` / `browser.status` arrive only on connections that attached */
const browserViewListeners = new Set<(hostId: string, method: string, p: any) => void>();
export function onBrowserView(fn: (hostId: string, method: string, p: any) => void): () => void {
  browserViewListeners.add(fn);
  return () => browserViewListeners.delete(fn);
}

/** Host personal memories or pending proposals changed (`memory.changed`) */
const memoryListeners = new Set<(hostId: string) => void>();
export function onMemoryChanged(fn: (hostId: string) => void): () => void {
  memoryListeners.add(fn);
  return () => memoryListeners.delete(fn);
}

export function sessionInfo(hostId: string, sid: string): SessionInfo | undefined {
  return state.sessions[hostId]?.find((s) => s.id === sid);
}

/** All host sessions in sidebar order (for switching to the previous / next session and the command palette) */
export function orderedSessions(): { hostId: string; s: SessionInfo }[] {
  const out: { hostId: string; s: SessionInfo }[] = [];
  for (const h of state.hosts) {
    const list = (state.sessions[h.config.id] ?? []).filter(isHostSession);
    const pinned = list.filter((x) => x.pinned);
    for (const s of [...pinned, ...list.filter((x) => !x.pinned)]) out.push({ hostId: h.config.id, s });
  }
  return out;
}

/** Prompt snippets from host settings, cached per host */
const snippetCache = new Map<string, Promise<Snippet[]>>();

export function hostSnippets(hostId: string): Promise<Snippet[]> {
  let p = snippetCache.get(hostId);
  if (!p) {
    p = rpc<{ snippets?: Snippet[] }>(hostId, 'settings.get').then((r) => r.snippets ?? []);
    p.catch(() => snippetCache.delete(hostId));
    snippetCache.set(hostId, p);
  }
  return p;
}

export function invalidateSnippets(hostId: string) {
  snippetCache.delete(hostId);
}

// ---------- Toasts and notifications ----------

let toastId = 1;

/** Shows a toast. A 'loading' toast stays until dismissed; the returned function removes it. info/error auto-dismiss. */
export function toast(text: string, kind: Toast['kind'] = 'info', action?: Toast['action']): () => void {
  const t: Toast = { id: toastId++, text, kind, action };
  setState((s) => ({ toasts: [...s.toasts.slice(-3), t] }));
  const close = () => setState((s) => ({ toasts: s.toasts.filter((x) => x.id !== t.id) }));
  if (kind !== 'loading') setTimeout(close, kind === 'error' ? 8000 : action ? 6000 : 4000);
  return close;
}

/// Turn all kinds of errors into one sentence: Tauri mobile plugins reject with objects like {message, code}
export function errText(e: unknown): string {
  if (e instanceof Error) return trErr(e.message);
  if (typeof e === 'string') return trErr(e);
  if (e && typeof e === 'object') {
    const m = (e as { message?: unknown; error?: unknown }).message ?? (e as { error?: unknown }).error;
    if (typeof m === 'string' && m) return trErr(m);
    try {
      return JSON.stringify(e);
    } catch {
      /* circular references and such: fall back to String */
    }
  }
  return trErr(String(e));
}

export function errorToast(e: unknown) {
  toast(errText(e), 'error');
}

export const DEFAULT_NOTIFY: NotifyPrefs = { enabled: true, onTurnEnd: true, onPermission: true, when: 'away' };

export async function loadClientSettings() {
  try {
    const r = await hubCall<{ settings: ClientSettings; detectedProxy: string | null }>('client.getSettings');
    setState({ client: { ...r.settings, notify: { ...DEFAULT_NOTIFY, ...(r.settings.notify ?? {}) } }, detectedProxy: r.detectedProxy });
  } catch {
    /* older client core doesn't have this command */
  }
}

/** Only change the given parts; returns the full settings after saving */
export async function saveClientSettings(patch: Partial<ClientSettings>) {
  const r = await hubCall<{ settings: ClientSettings; detectedProxy: string | null }>('client.setSettings', { settings: patch });
  setState({ client: { ...r.settings, notify: { ...DEFAULT_NOTIFY, ...(r.settings.notify ?? {}) } }, detectedProxy: r.detectedProxy });
  return r.settings;
}

let notifyAllowed: boolean | null = null;

/** Query (and request if needed) the system notification permission; returns granted / denied / unsupported */
export async function notifyPermission(ask: boolean): Promise<'granted' | 'denied' | 'default' | 'unsupported'> {
  try {
    if (isTauri) {
      const n = await import('@tauri-apps/plugin-notification');
      let ok = await n.isPermissionGranted();
      if (!ok && ask) ok = (await n.requestPermission()) === 'granted';
      notifyAllowed = ok;
      return ok ? 'granted' : ask ? 'denied' : 'default';
    }
    if (!('Notification' in window)) return 'unsupported';
    if (Notification.permission === 'default' && ask) await Notification.requestPermission();
    return Notification.permission;
  } catch {
    return 'unsupported';
  }
}

/** Which session a notification points at: clicking the system notification brings the window to front and opens it */
type NotifyTarget = { hostId: string; sessionId: string; parentId?: string; sideChat?: boolean };

/** When the session page receives this, it opens the file panel and the panel opens the file left in pendingFiles */
export const OPEN_FILE = 'yi-console:open-file';
export interface OpenFileReq {
  path: string;
  line?: number;
  endLine?: number;
  diff?: boolean;
}
/** Same reason as pendingChanges: a closed panel mounts after the event and takes the queued requests then */
const pendingFiles = new Map<string, OpenFileReq[]>();
export function takeOpenFiles(hostId: string, sessionId: string): OpenFileReq[] {
  const k = skey(hostId, sessionId);
  const list = pendingFiles.get(k) ?? [];
  pendingFiles.delete(k);
  return list;
}
export function takeOpenFile(hostId: string, sessionId: string): OpenFileReq | undefined {
  const k = skey(hostId, sessionId);
  const list = pendingFiles.get(k);
  if (!list?.length) return undefined;
  const next = list.shift();
  if (list.length === 0) pendingFiles.delete(k);
  return next;
}

/** Side chats and review tabs have no file panel of their own; they live inside the parent's panel, so file and
 *  "changes" requests from them go to the parent session (bottom terminals keep their own id) */
export function panelSessionId(hostId: string, sessionId: string): string {
  const info = sessionInfo(hostId, sessionId);
  const parentId = info?.parentId;
  if (!parentId || !(info?.sideChat || info?.review)) return sessionId;
  return state.sessions[hostId]?.some((x) => x.id === parentId) ? parentId : sessionId;
}

function enqueueOpenFiles(hostId: string, targetId: string, reqs: OpenFileReq[]) {
  if (!reqs.length) return;
  const sessionId = panelSessionId(hostId, targetId);
  const k = skey(hostId, sessionId);
  lsSet(`file-panel-${k}`, true);
  const q = pendingFiles.get(k) ?? [];
  for (const r of reqs) {
    const idx = q.findIndex((x) => x.path === r.path);
    if (idx >= 0) q.splice(idx, 1);
    q.push(r);
  }
  pendingFiles.set(k, q);
  setTimeout(() => window.dispatchEvent(new CustomEvent(OPEN_FILE, { detail: { hostId, sessionId } })), 0);
}

/** Open a file (a link or context pill in chat) in the session's file panel, optionally at a line/range or diff view */
export function openFileInPanel(
  hostId: string,
  sessionId: string,
  path: string,
  opts?: number | { line?: number; endLine?: number; diff?: boolean },
) {
  const req: OpenFileReq =
    typeof opts === 'number' ? { path, line: opts } : { path, line: opts?.line, endLine: opts?.endLine, diff: opts?.diff };
  enqueueOpenFiles(hostId, sessionId, [req]);
}

/** Open multiple files sequentially in the session's file panel (last one becomes active) */
export function openFilesInPanel(hostId: string, sessionId: string, paths: string[]) {
  const unique = [...new Set(paths.filter(Boolean))];
  if (!unique.length) return;
  enqueueOpenFiles(
    hostId,
    sessionId,
    unique.map((path) => ({ path })),
  );
}

/** Embedded child panes (side chat / review tabs) currently mounted and visible inside their parent session's right panel */
const visibleChildSessions = new Map<string, number>();

export function registerVisibleChildSession(hostId: string, childSessionId: string): () => void {
  const k = skey(hostId, childSessionId);
  visibleChildSessions.set(k, (visibleChildSessions.get(k) ?? 0) + 1);
  return () => {
    const n = (visibleChildSessions.get(k) ?? 0) - 1;
    if (n > 0) visibleChildSessions.set(k, n);
    else visibleChildSessions.delete(k);
  };
}

export function isChildSessionVisible(hostId: string, childSessionId: string): boolean {
  return (visibleChildSessions.get(skey(hostId, childSessionId)) ?? 0) > 0;
}

/** Resolve a hidden child session (side chat, review tab, bottom terminal) to its parent session + embedded panel opener */
function resolveChildSession(
  hostId: string,
  sessionId: string,
  hint?: { parentId?: string; sideChat?: boolean },
): { view: Extract<View, { kind: 'session' }>; openPanel?: () => void } {
  const info = sessionInfo(hostId, sessionId);
  const parentId = info?.parentId ?? hint?.parentId;
  if (!parentId) return { view: { kind: 'session', hostId, sessionId } };
  const hostList = state.sessions[hostId];
  if (hostList && !hostList.some((x) => x.id === parentId)) {
    return { view: { kind: 'session', hostId, sessionId } };
  }
  const isSide = info ? !!(info.sideChat || info.review) : !!hint?.sideChat;
  if (isSide) {
    return {
      view: { kind: 'session', hostId, sessionId: parentId },
      openPanel: () => openFileInPanel(hostId, parentId, `side:${sessionId}`),
    };
  }
  if ((info && info.mode === 'structured' && info.tool !== 'shell' && info.tool !== 'files') || (!info && hint?.parentId)) {
    return {
      view: { kind: 'session', hostId, sessionId: parentId },
      openPanel: () => openFileInPanel(hostId, parentId, `review:${sessionId}`),
    };
  }
  if (info && info.tool === 'shell') {
    return {
      view: { kind: 'session', hostId, sessionId: parentId },
      openPanel: () => {
        lsSet(`term-panel-${skey(hostId, parentId)}`, true);
        lsSet(`term-tab-${parentId}`, sessionId);
      },
    };
  }
  return { view: { kind: 'session', hostId, sessionId } };
}

async function focusWindow() {
  if (isTauri) {
    const w = (await import('@tauri-apps/api/window')).getCurrentWindow();
    await w.unminimize();
    await w.show();
    await w.setFocus();
  } else window.focus();
}

function openSessionTarget(t: NotifyTarget) {
  const resolved = resolveChildSession(t.hostId, t.sessionId, { parentId: t.parentId, sideChat: t.sideChat });
  resolved.openPanel?.();
  navigate(resolved.view);
}

function openTarget(t: NotifyTarget) {
  focusWindow().catch(() => {});
  openSessionTarget(t);
}

/** On desktop, clicks are only reported if an onAction listener exists when the notification is sent, so register before the first one */
let actionListener: Promise<unknown> | null = null;
function listenNotifyClick(n: typeof import('@tauri-apps/plugin-notification')) {
  actionListener ??= n
    .onAction((e) => {
      const x = e.notification?.extra as Partial<NotifyTarget> | undefined;
      if (x?.hostId && x.sessionId) {
        openTarget({
          hostId: x.hostId,
          sessionId: x.sessionId,
          ...(typeof x.parentId === 'string' && x.parentId ? { parentId: x.parentId } : {}),
          ...(typeof x.sideChat === 'boolean' ? { sideChat: x.sideChat } : {}),
        });
      }
    })
    .catch(() => null);
  return actionListener;
}

export async function systemNotify(title: string, body: string, target?: NotifyTarget) {
  try {
    if (isTauri) {
      const n = await import('@tauri-apps/plugin-notification');
      if (notifyAllowed === null) {
        notifyAllowed = await n.isPermissionGranted();
        if (!notifyAllowed) notifyAllowed = (await n.requestPermission()) === 'granted';
      }
      if (!notifyAllowed) return;
      await listenNotifyClick(n);
      n.sendNotification({ title, body, extra: target });
    } else if ('Notification' in window) {
      if (Notification.permission === 'default') await Notification.requestPermission();
      if (Notification.permission !== 'granted') return;
      const note = new Notification(title, { body });
      if (target)
        note.onclick = () => {
          note.close();
          openTarget(target);
        };
    }
  } catch {
    /* ignore when notifications are unavailable */
  }
}

// Page back/forward: we keep our own record instead of browser history (Tauri has no address bar, and in the web build a refresh shouldn't go back to long ago)
const past: View[] = [];
const future: View[] = [];
const sameView = (a: View, b: View) => JSON.stringify(a) === JSON.stringify(b);
const syncNav = () => setState({ canBack: past.length > 0, canForward: future.length > 0 });

export function goBack() {
  const v = past.pop();
  if (!v) return;
  future.push(getState().view);
  show(v);
  syncNav();
}

export function goForward() {
  const v = future.pop();
  if (!v) return;
  past.push(getState().view);
  show(v);
  syncNav();
}

export function toggleSidebar() {
  const hidden = !getState().sidebarHidden;
  setState({ sidebarHidden: hidden });
  try {
    localStorage.setItem('yi-console-sidebar-hidden', hidden ? '1' : '0');
  } catch {
    /* storage unavailable */
  }
}

export function navigate(view: View) {
  let next = view;
  if (next.kind === 'session') {
    const resolved = resolveChildSession(next.hostId, next.sessionId);
    if (resolved.openPanel) {
      resolved.openPanel();
      next = resolved.view;
    }
  }
  const cur = getState().view;
  if (!sameView(cur, next)) {
    past.push(cur);
    if (past.length > 50) past.shift();
    future.length = 0;
    syncNav();
  }
  show(next);
}

/** Open the new session page */
export function openNewSession(p: NewSessionParams) {
  navigate({ kind: 'new', ...p });
}

/** Show a view in place of the current one, without a back entry (the new session page hands over to its session) */
export function replaceView(view: View) {
  let next = view;
  if (next.kind === 'session') {
    const resolved = resolveChildSession(next.hostId, next.sessionId);
    if (resolved.openPanel) {
      resolved.openPanel();
      next = resolved.view;
    }
  }
  future.length = 0;
  syncNav();
  show(next);
}

function show(view: View) {
  // A turn that ended while its session was open, or that the user opens now, has been seen (overview board)
  markSeen([state.view, view].flatMap((v) => (v.kind === 'session' ? [{ hostId: v.hostId, sessionId: v.sessionId }] : [])));
  setState({ view, sidebarOpen: false, palette: false });
  try {
    const { seq: _seq, ...keep } = view as View & { seq?: number };
    localStorage.setItem('yi-console-view', JSON.stringify(keep));
  } catch {
    /* storage unavailable */
  }
}

// ---------- Event entry point ----------

export function startEventLoop() {
  onLangChange(() => {
    for (const h of state.hosts) if (h.status.status === 'connected') reportLang(h.config.id);
  });
  onHubEvent((ev) => {
    switch (ev.type) {
      case 'transport':
        setState({ transport: ev.status });
        if (ev.status === 'open') {
          refreshHosts().catch(() => {});
          loadClientSettings();
          refreshBots();
        }
        break;
      case 'host.status': {
        const { type: _t, hostId, ...status } = ev;
        setState((s) => ({
          hosts: s.hosts.map((h) => (h.config.id === hostId ? { ...h, status } : h)),
        }));
        if (status.status === 'connected') {
          refreshSessions(hostId);
          catchUpHost(hostId);
          reportLang(hostId);
          refreshRemoteHost(hostId);
        } else dropRemoteHost(hostId);
        break;
      }
      case 'hosts.changed':
        refreshHosts().catch(() => {});
        break;
      case 'upgrade': {
        const { hostId, phase, message, progress } = ev;
        setState((s) => {
          const upgrades = { ...s.upgrades };
          if (phase === 'done') delete upgrades[hostId];
          else upgrades[hostId] = { phase, message, progress };
          return { upgrades };
        });
        if (phase === 'done') {
          const name = state.hosts.find((h) => h.config.id === hostId)?.config.name ?? hostId;
          toast(t('yiAgentdOnHasBeen', { name }));
        }
        break;
      }
      case 'deploy':
        setState((s) => ({ deploy: { ...s.deploy, [ev.hostId]: [...(s.deploy[ev.hostId] ?? []), ev.message] } }));
        toast(ev.message);
        break;
      case 'notification':
        onNotification(ev.hostId, ev.method, ev.params);
        break;
      case 'bots.changed':
        refreshBots();
        break;
      case 'bots.secretsNeeded':
        secretsNeeded(ev, '');
        break;
      case 'bots.confirm':
        confirmNeeded(ev, '');
        break;
      case 'bots.connectorProposal':
        connectorProposal(ev, '');
        break;
      case 'bots.taskUpgraded':
        taskUpgraded(ev, '');
        break;
      case 'bots.notify':
      case 'bots.attention': {
        botToast(ev, '');
        if (ev.type === 'bots.attention') refreshBots();
        break;
      }
    }
  });
}

/** Merge a tool upgrade job from `tools.latest` or a `tools.updating` event */
export function setToolJob(hostId: string, tool: string, job: ToolJob, line?: string) {
  setState((s) => {
    const prev = s.toolJobs[hostId]?.[tool];
    // Line events carry the job without its output, so keep appending to what we have
    const output = line !== undefined ? [...(prev?.output ?? []), line].slice(-200) : job.output;
    return { toolJobs: { ...s.toolJobs, [hostId]: { ...s.toolJobs[hostId], [tool]: { ...job, output } } } };
  });
}

/** A Bot proposed a connector a task built: open it for review when the Bot's session is on screen, otherwise offer it
 * in a toast */
function connectorProposal(ev: any, src: string) {
  const go = () => navigate({ kind: 'connectors', review: ev.token, src: src || undefined });
  const hostId = mapHost(src, ev.host, (id) => state.hosts.some((h) => h.config.id === id));
  const v = state.view;
  if (v.kind === 'session' && v.hostId === hostId && v.sessionId === ev.sessionId) go();
  else toast(t('pluginProposed', { name: ev.id }), 'info', { label: t('view'), run: go });
}

/** Connector configurations of a Bot wait for the owner's secrets: a card in the Bot's session takes them */
function secretsNeeded(ev: any, src: string) {
  const pending: { name: string }[] = ev.pending ?? [];
  if (pending.length === 0) return;
  const v = state.view;
  const s = findBot(ev.botId, src)?.session;
  const hostId = s ? mapHost(src, s.host, (id) => state.hosts.some((h) => h.config.id === id)) : null;
  if (s && v.kind === 'session' && v.hostId === hostId && v.sessionId === s.sessionId) return;
  toast(t('pluginSecretsNeeded', { names: pending.map((p) => p.name).join(', ') }), 'info', {
    label: t('fillIn'),
    run: () => (s && hostId ? navigate({ kind: 'session', hostId, sessionId: s.sessionId }) : navigate({ kind: 'bot', botId: ev.botId, src: src || undefined })),
  });
}

/** A Bot asked for a configuration change that needs the owner's confirmation (a card in its session bar) */
function confirmNeeded(ev: any, src: string) {
  const v = state.view;
  const s = findBot(ev.botId, src)?.session;
  const hostId = s ? mapHost(src, s.host, (id) => state.hosts.some((h) => h.config.id === id)) : null;
  if (s && v.kind === 'session' && v.hostId === hostId && v.sessionId === s.sessionId) return;
  toast(t('botWantsConfirm', { name: ev.name }), 'info', {
    label: t('view'),
    run: () => (s && hostId ? navigate({ kind: 'session', hostId, sessionId: s.sessionId }) : navigate({ kind: 'bot', botId: ev.botId, src: src || undefined })),
  });
}

/** A task upgraded in place from read-only `repo` to a new `worktree` session: follow it if currently open */
function taskUpgraded(ev: any, src: string) {
  const hostId = mapHost(src, ev.host, (id) => state.hosts.some((h) => h.config.id === id));
  const v = state.view;
  if (hostId && ev.sessionId && v.kind === 'session' && v.hostId === hostId && v.sessionId === ev.oldSessionId) {
    navigate({ kind: 'session', hostId, sessionId: ev.sessionId });
    toast(t('taskUpgraded'), 'info');
  }
}

/** A Bot notification or a call for attention, from this app's engine (src '') or a remote desktop's */
function botToast(ev: any, src: string) {
  const title: string = ev.type === 'bots.notify' ? ev.title : t('botNeedsYou');
  const body: string = (ev.type === 'bots.notify' ? ev.body : ev.text) ?? '';
  const prefs = state.client?.notify ?? DEFAULT_NOTIFY;
  const away = document.hidden || !document.hasFocus();
  if (prefs.enabled && (prefs.when === 'always' || away)) systemNotify(title, body);
  const botId: string | undefined = ev.botId;
  const go = botId ? { label: t('view'), run: () => navigate({ kind: 'bot', botId, src: src || undefined }) } : undefined;
  toast(body ? `${title} · ${body}` : title, 'info', go);
}

function onNotification(hostId: string, method: string, p: any) {
  switch (method) {
    // Desktop apps registered with this daemon: their Bots are listed and controlled from here
    case 'clients.changed':
      refreshRemoteHost(hostId);
      break;
    case 'client.event': {
      const ev = p?.event;
      const key = remoteKey(hostId, p?.clientId);
      // This app's own events arrive directly; on the local daemon they are echoed back
      if (!ev || sourceOf(p?.clientId) === '') break;
      if (ev.type === 'bots.changed' || ev.type === 'bots.attention') refreshRemote(key);
      if (ev.type === 'bots.notify' || ev.type === 'bots.attention') botToast(ev, key);
      if (ev.type === 'bots.secretsNeeded') secretsNeeded(ev, key);
      if (ev.type === 'bots.confirm') confirmNeeded(ev, key);
      if (ev.type === 'bots.connectorProposal') connectorProposal(ev, key);
      if (ev.type === 'bots.taskUpgraded') taskUpgraded(ev, key);
      break;
    }
    case 'tools.updating':
      setToolJob(hostId, p.tool, p.job, p.line);
      if (Array.isArray(p.tools)) setHostTools(hostId, p.tools);
      break;
    case 'accounts.updated':
      redetectTools(hostId).catch(() => {});
      for (const fn of accountListeners) fn(hostId, p?.tool);
      break;
    case 'browser.frame':
    case 'browser.status':
      for (const fn of browserViewListeners) fn(hostId, method, p);
      break;
    case 'memory.changed':
      for (const fn of memoryListeners) fn(hostId);
      break;
    case 'tools.accountLogin':
      if (p) for (const fn of loginListeners) fn(hostId, p as AccountLoginUpdate);
      break;
    case 'session.updated':
      upsertSession(hostId, p);
      break;
    case 'session.removed':
      clearComposerState(skey(hostId, p.sessionId));
      setState((s) => ({
        sessions: { ...s.sessions, [hostId]: (s.sessions[hostId] ?? []).filter((x) => x.id !== p.sessionId) },
      }));
      break;
    case 'session.event': {
      const key = skey(hostId, p.sessionId);
      const d = sessionData.get(key);
      const before = d?.loaded ? maxSeq(d) : 0;
      addEvents(key, [p.event]);
      // seq is consecutive within each session: a gap means something was missed, so catch up once (not again within 5 s)
      const seq = p.event?.seq ?? 0;
      if (before > 0 && seq > before + 1 && Date.now() - (lastCatch.get(key) ?? 0) > 5000) catchUp(hostId, p.sessionId);
      break;
    }
    case 'session.backlog': {
      const key = skey(hostId, p.sessionId);
      addEvents(key, p.events ?? []);
      if (p.terminal !== undefined) writeTerm(key, b64(p.terminal), true);
      if (p.info) upsertSession(hostId, p.info);
      break;
    }
    case 'pty.output':
      writeTerm(skey(hostId, p.sessionId), b64(p.data), false);
      break;
    case 'notify': {
      const prefs = state.client?.notify ?? DEFAULT_NOTIFY;
      const v = state.view;
      const sess = sessionInfo(hostId, p.sessionId);
      const parentId = sess?.parentId ?? (typeof p.parentId === 'string' && p.parentId ? p.parentId : undefined);
      const isReview = !!(sess?.review ?? p.review);
      const sideChat = !!(sess?.sideChat ?? p.sideChat ?? isReview);
      const viewingParent = v.kind === 'session' && v.hostId === hostId && !!parentId && v.sessionId === parentId;
      const viewing =
        v.kind === 'session' &&
        v.hostId === hostId &&
        (v.sessionId === p.sessionId || (viewingParent && isChildSessionVisible(hostId, p.sessionId)));
      // A window behind others (covered, minimized) also counts as not watching, hence hasFocus rather than just hidden
      const away = document.hidden || !document.hasFocus() || !viewing;
      const want = p.kind === 'permission' ? prefs.onPermission : prefs.onTurnEnd;
      let title: string = p.title;
      let body: string = p.body || '';
      if (p.kind === 'rate_limit' && p.limit) {
        // Usage limit: built here so it follows this device's language
        const at: number | null = p.limit.resumeAt ?? null;
        title = t('usageLimitReached', { name: p.limit.name });
        body = [
          sess?.title,
          at
            ? t('continuesAutomaticallyAt', { v: fmtAt(at) })
            : p.limit.probing
              ? t('continuesAutomaticallyOnceTheLimit')
              : t('wonTContinueAutomatically'),
        ]
          .filter(Boolean)
          .join(' · ');
      }
      const target: NotifyTarget = {
        hostId,
        sessionId: p.sessionId,
        ...(parentId ? { parentId } : {}),
        ...(sideChat ? { sideChat: true } : {}),
      };
      if (prefs.enabled && want && (prefs.when === 'always' || away)) systemNotify(title, body, target);
      // In-app toast: remind the user when they're in another session (review side chats stay quiet while viewing their parent session)
      if (!viewing && !(isReview && viewingParent)) {
        toast(title, 'info', {
          label: t('view'),
          run: () => openSessionTarget(target),
        });
      }
      break;
    }
  }
}

export function restoreView() {
  try {
    const v = JSON.parse(localStorage.getItem('yi-console-view') || 'null');
    if (v && v.kind) setState({ view: v });
  } catch {
    /* ignore */
  }
}
