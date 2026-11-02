import { lsGet, lsSet } from './util';
// 全局状态：主机、会话列表、当前视图放在一个小 store 里；
// 每个会话的事件流单独存放（量大、变化频繁），按会话粒度通知订阅者。

import { useSyncExternalStore } from 'react';
import { tr, trErr } from './i18n';
import { hubCall, onHubEvent, rpc, isTauri } from './transport';
import type { ClientSettings, HostConfig, HostStatus, HubEvent, NotifyPrefs, SessionInfo, Snippet, ToolKind } from './types';

export type View =
  | { kind: 'home' }
  | { kind: 'hosts'; editId?: string }
  /** seq：打开后滚到这条消息（全局搜索跳过来） */
  | { kind: 'session'; hostId: string; sessionId: string; seq?: number }
  | { kind: 'search'; q?: string }
  /** cwd：只看这个目录（含子目录）下的历史会话 */
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
  kind: 'info' | 'error';
  action?: { label: string; run: () => void };
}

export interface Upgrade {
  /** waiting：等回合结束；uploading / restarting：升级中，界面暂时不能用 */
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
  /** yi-agentd 升级进度，按主机 */
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
    /** 换个工具继续：原会话的工具（新会话不能再选它）和预填的第一条消息 */
    handoff?: { from: ToolKind; prompt: string };
  } | null;
  /** 这台设备的设置（代理、通知），存在客户端数据目录 */
  client: ClientSettings | null;
  detectedProxy: string | null;
  /** Ctrl+K 命令面板 */
  palette: boolean;
  /** 桌面端收起侧边栏 */
  sidebarHidden: boolean;
  /** 能否后退 / 前进（标题栏按钮） */
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

// ---------- 会话事件流 ----------

export interface SessionData {
  events: HubEvent[];
  seqs: Set<number>;
  /** 流式中的助手文本：id -> 已收到的文本 */
  live: Map<string, string>;
  /** 流式中的思考过程 */
  liveThinking: Map<string, string>;
  loaded: boolean;
  loading: boolean;
  /** 打开时只取了最近一段，更早的还在 yi-agentd 上 */
  hasMore: boolean;
  loadingOlder: boolean;
  version: number;
  /** 终端输出的消费者（TerminalView 挂上来），reset=true 表示先清屏再写快照 */
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
    // 最终文本到了，清掉对应的流式缓冲（claude 的块 id 是 "消息id:序号"）
    if (e.kind === 'assistant_text' || e.kind === 'thinking') {
      const m = e.kind === 'thinking' ? d.liveThinking : d.live;
      for (const id of [...m.keys()]) {
        if (e.id === id || e.id.startsWith(id + ':')) m.delete(id);
      }
    }
    // 思考结束、开始输出正文时，流式思考也就结束了
    if (e.kind === 'assistant_text' || e.kind === 'tool_call') d.liveThinking.clear();
    if (e.kind === 'turn_end' || (e.kind === 'status' && (e.status === 'exited' || e.status === 'error'))) {
      d.live.clear();
      d.liveThinking.clear();
    }
    d.events.push(e);
    changed = true;
  }
  if (changed) {
    // 积压和实时事件可能交错到达，按 seq 排一下
    d.events.sort((a, b) => a.seq - b.seq);
    // /clear 之前的都已删掉：只留最后一次清空之后的，也不用再往前翻
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

/** 打开会话时先取最近多少条事件（系统设置「对话外观」里可调，每台设备自己记）；往上翻每次也取这么多 */
export const TAIL_OPTIONS = [100, 200, 500, 1000];
export const DEFAULT_TAIL = 200;
export function historyTail(): number {
  const n = Number(lsGet('history-tail', DEFAULT_TAIL));
  return TAIL_OPTIONS.includes(n) ? n : DEFAULT_TAIL;
}
export function setHistoryTail(n: number) {
  lsSet('history-tail', n);
}

/** 往前翻：取当前最早一条之前的 historyTail() 条 */
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
    // 第一次打开只取最近 historyTail() 条（默认 200），长会话（工具输出多）经 SSH/中继也能很快打开；更早的往上翻时再取
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
 * 按界面上实际有的最大 seq 补拉打开过的会话。重连时 ClientHub 也会补，但它按自己收到的 seq 算：
 * 手机切后台时 WebView 被暂停，事件到了 Rust 却没进界面，或者重连补拉失败被它取消了订阅，都会让会话停在旧内容。
 * 失败就标成未加载，下次打开（或正在看的会话页）重新加载
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

// 回到前台（手机从后台切回、桌面从最小化恢复）时补一次
if (typeof document !== 'undefined') {
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState !== 'visible') return;
    for (const h of state.hosts) if (h.status.status === 'connected') catchUpHost(h.config.id);
  });
}

// ---------- 主机与会话列表 ----------

export async function refreshHosts() {
  const r = await hubCall<{ hosts: HostEntry[]; localSupported: boolean }>('hosts.list');
  const hosts = r.hosts.map((h) => ({ ...h, status: h.status ?? { status: 'disconnected' as const } }));
  setState({ hosts, localSupported: r.localSupported });
  for (const h of hosts) if (h.status.status === 'connected') refreshSessions(h.config.id);
  return hosts;
}

export async function refreshSessions(hostId: string) {
  try {
    const list = await rpc<SessionInfo[]>(hostId, 'session.list');
    setState((s) => ({ sessions: { ...s.sessions, [hostId]: list } }));
  } catch {
    /* 主机未连接 */
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

/** 按侧边栏的顺序排好的所有会话（切换上一个 / 下一个会话用） */
export function orderedSessions(): { hostId: string; s: SessionInfo }[] {
  const out: { hostId: string; s: SessionInfo }[] = [];
  for (const h of state.hosts) {
    const list = (state.sessions[h.config.id] ?? []).filter((x) => !x.parentId);
    const pinned = list.filter((x) => x.pinned);
    for (const s of [...pinned, ...list.filter((x) => !x.pinned)]) out.push({ hostId: h.config.id, s });
  }
  return out;
}

/** 主机设置里的提示词片段，按主机缓存 */
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

// ---------- 提示与通知 ----------

let toastId = 1;

export function toast(text: string, kind: Toast['kind'] = 'info', action?: Toast['action']) {
  const t: Toast = { id: toastId++, text, kind, action };
  setState((s) => ({ toasts: [...s.toasts.slice(-3), t] }));
  setTimeout(() => setState((s) => ({ toasts: s.toasts.filter((x) => x.id !== t.id) })), kind === 'error' ? 8000 : 4000);
}

/// 把各种报错变成一句话：Tauri 手机插件 reject 的是 {message, code} 这样的对象
export function errText(e: unknown): string {
  if (e instanceof Error) return trErr(e.message);
  if (typeof e === 'string') return trErr(e);
  if (e && typeof e === 'object') {
    const m = (e as { message?: unknown; error?: unknown }).message ?? (e as { error?: unknown }).error;
    if (typeof m === 'string' && m) return trErr(m);
    try {
      return JSON.stringify(e);
    } catch {
      /* 循环引用之类，退回 String */
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
    /* 旧版客户端核心没有这个命令 */
  }
}

/** 只改传进来的部分，返回保存后的完整设置 */
export async function saveClientSettings(patch: Partial<ClientSettings>) {
  const r = await hubCall<{ settings: ClientSettings; detectedProxy: string | null }>('client.setSettings', { settings: patch });
  setState({ client: { ...r.settings, notify: { ...DEFAULT_NOTIFY, ...(r.settings.notify ?? {}) } }, detectedProxy: r.detectedProxy });
  return r.settings;
}

let notifyAllowed: boolean | null = null;

/** 查询（必要时申请）系统通知权限；返回 granted / denied / unsupported */
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

/** 通知点哪个会话：点系统通知时把窗口拉到前面并打开它 */
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

/** 桌面端只有发通知时已经有 onAction 监听，点击才会回报，所以第一次发之前先注册 */
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
    /* 通知不可用时忽略 */
  }
}

// 页面前进后退：自己记一份，不走浏览器 history（Tauri 里没有地址栏，网页版刷新后也不想回到很久以前）
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
    /* 存储不可用 */
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
    /* 存储不可用 */
  }
}

// ---------- 事件入口 ----------

export function startEventLoop() {
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
      // seq 在每个会话里连续编号：接不上说明中间漏了，补一次（5 秒内不重复补）
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
      // 窗口在后面（被别的窗口挡住、最小化）也算不在看，所以用 hasFocus 而不只是 hidden
      const away = document.hidden || !document.hasFocus() || !viewing;
      const want = p.kind === 'permission' ? prefs.onPermission : prefs.onTurnEnd;
      if (prefs.enabled && want && (prefs.when === 'always' || away)) systemNotify(p.title, p.body || '', { hostId, sessionId: p.sessionId });
      // 应用内提示：在别的会话里时提醒一下
      if (!viewing) {
        toast(p.title, 'info', {
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
    /* 忽略 */
  }
}
