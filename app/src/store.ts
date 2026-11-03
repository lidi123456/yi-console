import { lsGet, lsSet } from './util';
// Global state: hosts, session list and the current view live in one small store;
// each session's event stream is stored separately (large, changes often) and subscribers are notified per session.

import { useSyncExternalStore } from 'react';
import { getLang, onLangChange, tr, trErr } from './i18n';
import { fmtAt } from './limit';
import { hubCall, onHubEvent, rpc, isTauri } from './transport';
import type { ClientSettings, HostConfig, HostStatus, HubEvent, NotifyPrefs, SessionInfo, Snippet, ToolKind } from './types';

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
  | { kind: 'settings'; hostId: string }
  | { kind: 'prefs' };

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
  toasts: Toast[];
  transport: 'open' | 'closed' | 'connecting';
  sidebarOpen: boolean;
  newSession: {
    hostId: string;
    cwd?: string;
    tool?: string;
    resumeId?: string;
    title?: string;
    /** Continue with another tool: the original session's tool (the new session can't pick it) and the prefilled first message */
    handoff?: { from: ToolKind; prompt: string };
  } | null;
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
  toasts: [],
  transport: 'connecting',
  sidebarOpen: false,
  newSession: null,
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
      live: new Map(),
      liveThinking: new Map(),
      loaded: false,
      loading: false,
      hasMore: false,
      loadingOlder: false,
      version: 0,
      termBacklog: [],
    };
    sessionData.set(key, d);
  }
  return d;
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

function addEvents(key: string, evs: HubEvent[]) {
  const d = getSession(key);
  let changed = false;
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
      for (const e of d.events.slice(0, cut + 1)) d.seqs.delete(e.seq);
      d.seqs.add(d.events[cut].seq);
      d.events = d.events.slice(cut + 1);
      d.hasMore = false;
      d.live.clear();
      d.liveThinking.clear();
    }
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

/** Page back: fetch historyTail() events before the current earliest one */
export async function loadOlder(hostId: string, sid: string) {
  const key = skey(hostId, sid);
  const d = getSession(key);
  if (!d.hasMore || d.loadingOlder) return;
  d.loadingOlder = true;
  bump(key);
  try {
    const first = d.events.find((e) => e.seq > 0)?.seq;
    const r = await rpc(hostId, 'session.history', { sessionId: sid, beforeSeq: first, limit: historyTail() });
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
      d.live.clear();
      d.liveThinking.clear();
    }
    addEvents(key, r.events);
    if (r.terminal !== undefined) writeTerm(key, b64(r.terminal), true);
    upsertSession(hostId, r.info);
    d.loaded = true;
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
    setState((s) => ({ sessions: { ...s.sessions, [hostId]: list } }));
  } catch {
    /* host not connected */
  }
}

function upsertSession(hostId: string, info: SessionInfo) {
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

export function sessionInfo(hostId: string, sid: string): SessionInfo | undefined {
  return state.sessions[hostId]?.find((s) => s.id === sid);
}

/** All sessions in sidebar order (for switching to the previous / next session) */
export function orderedSessions(): { hostId: string; s: SessionInfo }[] {
  const out: { hostId: string; s: SessionInfo }[] = [];
  for (const h of state.hosts) {
    const list = (state.sessions[h.config.id] ?? []).filter((x) => !x.parentId);
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
  if (kind !== 'loading') setTimeout(close, kind === 'error' ? 8000 : 4000);
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
type NotifyTarget = { hostId: string; sessionId: string };

async function focusWindow() {
  if (isTauri) {
    const w = (await import('@tauri-apps/api/window')).getCurrentWindow();
    await w.unminimize();
    await w.show();
    await w.setFocus();
  } else window.focus();
}

function openTarget(t: NotifyTarget) {
  focusWindow().catch(() => {});
  navigate({ kind: 'session', hostId: t.hostId, sessionId: t.sessionId });
}

/** On desktop, clicks are only reported if an onAction listener exists when the notification is sent, so register before the first one */
let actionListener: Promise<unknown> | null = null;
function listenNotifyClick(n: typeof import('@tauri-apps/plugin-notification')) {
  actionListener ??= n
    .onAction((e) => {
      const x = e.notification?.extra as Partial<NotifyTarget> | undefined;
      if (x?.hostId && x.sessionId) openTarget({ hostId: x.hostId, sessionId: x.sessionId });
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
  const cur = getState().view;
  if (!sameView(cur, view)) {
    past.push(cur);
    if (past.length > 50) past.shift();
    future.length = 0;
    syncNav();
  }
  show(view);
}

function show(view: View) {
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
        }
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
          toast(tr(`${name} 的 yi-agentd 已升级到新版本`, `yi-agentd on ${name} has been upgraded`));
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
    }
  });
}

function onNotification(hostId: string, method: string, p: any) {
  switch (method) {
    case 'session.updated':
      upsertSession(hostId, p);
      break;
    case 'session.removed':
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
      const viewing = v.kind === 'session' && v.hostId === hostId && v.sessionId === p.sessionId;
      // A window behind others (covered, minimized) also counts as not watching, hence hasFocus rather than just hidden
      const away = document.hidden || !document.hasFocus() || !viewing;
      const want = p.kind === 'permission' ? prefs.onPermission : prefs.onTurnEnd;
      let title: string = p.title;
      let body: string = p.body || '';
      if (p.kind === 'rate_limit' && p.limit) {
        // Usage limit: built here so it follows this device's language
        const at: number | null = p.limit.resumeAt ?? null;
        const sess = state.sessions[hostId]?.find((x) => x.id === p.sessionId);
        title = tr(`${p.limit.name} 额度已用完`, `${p.limit.name}: usage limit reached`);
        body = [
          sess?.title,
          at
            ? tr(`${fmtAt(at)} 自动继续`, `Continues automatically at ${fmtAt(at)}`)
            : p.limit.probing
              ? tr('额度恢复后会自动继续', 'Continues automatically once the limit resets')
              : tr('不会自动继续', "Won't continue automatically"),
        ]
          .filter(Boolean)
          .join(' · ');
      }
      if (prefs.enabled && want && (prefs.when === 'always' || away)) systemNotify(title, body, { hostId, sessionId: p.sessionId });
      // In-app toast: remind the user when they're in another session
      if (!viewing) {
        toast(title, 'info', {
          label: tr('查看', 'View'),
          run: () => navigate({ kind: 'session', hostId, sessionId: p.sessionId }),
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
