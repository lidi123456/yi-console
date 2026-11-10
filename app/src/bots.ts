// Bots: always-on orchestrating sessions and the tasks they spawn. The engine runs in the desktop app (crates/bots);
// this module keeps the list it reports (`bots.list`) and refetches on `bots.changed`. On phones and yi-web
// `bots.list` is unknown, so `list` stays null.
//
// Remote control: a desktop app registers with the daemon on its machine, which lists it in `clients.list`. Any
// client connected to that daemon (a phone over the relay, yi-web, another desktop) sees it as a *source* and sends
// commands through `client.call`; the desktop pushes `client.event` on changes. A source is '' for this app's own
// engine, or `<host id>|<client id>` for a remote desktop reached through host <host id>.

import { createContext, useContext, useSyncExternalStore } from 'react';
import { hubCall, rpc } from './transport';
import type { SessionInfo, ToolKind } from './types';

export type Accept = 'off' | 'observe' | 'auto';

export interface BotConfig {
  id: string;
  name: string;
  enabled: boolean;
  workspaces: { host: string; cwd: string }[];
  bot: { host: string; cwd: string; tool: ToolKind; model?: string | null; effort?: string | null; permissionMode?: string | null };
  task: {
    tool: ToolKind;
    /** Other tools the Bot may pick per task; model and effort apply to `tool` only */
    tools?: ToolKind[];
    model?: string | null;
    effort?: string | null;
    permissionMode: string;
    worktree: boolean;
    maxConcurrent: number;
    timeoutMinutes: number;
    /** Days an ended task keeps its session; 0: forever */
    keepDays?: number;
  };
  triggers: string[];
  notify: string[];
  mcp: { bot: string[]; task: string[] };
  accept: Accept;
  allow: { instance: string; ids: string[] }[];
  budget: { tasksPerDay: number; tokensPerDay?: number };
  routines?: Routine[];
  approval?: { reply: boolean; write: boolean };
  /** ISO weekdays (1 = Monday, empty = every day) and HH:MM times; null = always */
  activeHours?: { days: number[]; start: string; end: string } | null;
  prompt: { role: string; communication: string; delivery: string };
  promptRevision: number;
  compactAt: number;
}

export interface Routine {
  id: string;
  name: string;
  /** Five-field cron, local time */
  schedule: string;
  prompt: string;
  enabled: boolean;
}

/** One run of a routine (crates/bots store.rs RoutineRun) */
export interface RoutineRun {
  id: string;
  botId: string;
  routineId: string;
  name: string;
  /** queued / running / done / nothing / failed / skipped */
  state: string;
  manual: boolean;
  summary: string;
  startedAt: number;
  endedAt?: number | null;
}

/** An outgoing action the Bot drafted that waits for the owner (crates/bots store.rs ApprovalRow) */
export interface Approval {
  id: string;
  botId: string;
  /** reply / tool */
  kind: string;
  /** pending / approved / rejected / expired */
  state: string;
  instanceId: string;
  eventId?: string | null;
  thread: string;
  text: string;
  tool?: string | null;
  input?: unknown;
  /** Plugin instance name, and the message a reply answers */
  source?: string;
  quoteFrom?: string;
  quote?: string;
  result?: string | null;
  createdAt: number;
  decidedAt?: number | null;
}

export type TaskState = 'queued' | 'working' | 'needsYou' | 'ready' | 'failed' | 'done' | 'stopped';

export interface TaskRow {
  id: string;
  botId: string;
  host: string;
  sessionId?: string | null;
  state: TaskState;
  title: string;
  eventIds: string[];
  threads: string[];
  cwd: string;
  workspace: string;
  /** `dirty`: kept after the task ended because something in it is uncommitted */
  worktree?: { path: string; branch: string; repo: string; dirty?: boolean } | null;
  spec: Record<string, unknown>;
  /** `auto`: written by the engine from the last message of a turn that ended without a report (state `waiting`) */
  lastReport?: { state: string; summary: string; question?: string; links?: string[]; auto?: boolean; at: number } | null;
  hold: boolean;
  manual: boolean;
  createdAt: number;
  startedAt?: number | null;
  endedAt?: number | null;
  /** Session (and a clean worktree) removed */
  cleanedAt?: number | null;
}

export interface Attention {
  id: string;
  botId: string;
  taskId?: string | null;
  text: string;
  urgency: string;
  at: number;
}

/** A configuration change the Bot asked for that waits for the owner (auto accept, allow list, permission modes) */
export interface PendingChange {
  id: string;
  summary: string;
  items: string[];
}

/** Secrets of one plugin instance for the owner to enter: required ones that are missing, or ones the Bot asked
 *  for with request_secret (then with `requestId` and `reason`). The values never reach the Bot */
export interface SecretCard {
  instanceId: string;
  name: string;
  pluginId: string;
  enabled: boolean;
  requestId?: string | null;
  reason?: string | null;
  fields: { key: string; title?: string | null; titleEn?: string | null; description?: string | null; descriptionEn?: string | null; missing: boolean }[];
}

export interface BotEntry {
  config: BotConfig;
  pendingChange?: PendingChange | null;
  session: { host: string; sessionId: string } | null;
  /** disabled / starting / idle / running / compacting / error */
  state: string;
  error?: string | null;
  /** Events and reports waiting for the Bot */
  waiting: number;
  context?: number | null;
  attention: Attention[];
  /** Drafts waiting for the owner */
  approvals?: Approval[];
  /** Plugin instances the Bot uses that wait for the owner's secrets */
  secrets?: SecretCard[];
  /** Latest run per routine id */
  lastRuns?: Record<string, RoutineRun>;
  tokensToday?: number;
  tasks: TaskRow[];
}

export interface EventRow {
  id: string;
  instanceId: string;
  botId: string;
  key: string;
  thread: string;
  sender: string;
  senderName: string;
  senderBot: boolean;
  addressed: boolean;
  text: string;
  url?: string | null;
  at: number;
  disposition: string;
  taskId?: string | null;
  note: string;
}

/** A message on its way out through a plugin (crates/bots store.rs OutboxRow) */
export interface OutboxRow {
  key: string;
  instanceId: string;
  /** reply / notify */
  kind: string;
  botId?: string | null;
  thread: string;
  replyTo?: string | null;
  text: string;
  /** pending / sent / gone / failed */
  state: string;
  attempts: number;
  error?: string | null;
  nextAt: number;
  createdAt: number;
}

export interface PluginManifest {
  id: string;
  version: string;
  /** builtin / wasm */
  runtime: string;
  main?: string;
  name: { zh: string; en: string };
  description: { zh: string; en: string };
  capabilities: string[];
  permissions: { net: string[]; exec: string[] };
  config: { type?: string; required?: string[]; properties?: Record<string, SchemaProp> };
  prompt?: string | null;
  singleton?: boolean;
  /** Shipped with the app: updated with it, can't be uninstalled */
  bundled?: boolean;
}

export interface SchemaProp {
  type?: string;
  title?: string;
  description?: string;
  enum?: string[];
  default?: unknown;
  items?: { enum?: string[]; type?: string };
  'x-secret'?: boolean;
  'x-ui'?: string;
}

export interface PluginInstance {
  id: string;
  pluginId: string;
  name: string;
  config: Record<string, unknown>;
  enabled: boolean;
}

/** A desktop app reached through one of this client's hosts */
export interface RemoteBots {
  /** `<host id>|<client id>` */
  key: string;
  hostId: string;
  clientId: string;
  /** Device name */
  name: string;
  /** null while loading or when the call failed */
  list: BotEntry[] | null;
  error?: string;
}

interface BotsState {
  /** null: this client runs no Bots (phone, yi-web) */
  list: BotEntry[] | null;
  clientId: string | null;
  remotes: RemoteBots[];
}

let st: BotsState = { list: null, clientId: null, remotes: [] };
const subs = new Set<() => void>();

function set(next: BotsState) {
  st = next;
  for (const f of subs) f();
}

export function useBots<T>(sel: (s: BotsState) => T): T {
  return useSyncExternalStore(
    (cb) => {
      subs.add(cb);
      return () => subs.delete(cb);
    },
    () => sel(st),
  );
}

export function botsState() {
  return st;
}

let inflight: Promise<void> | null = null;
let again = false;

/** Refetch the list; calls during a fetch are folded into one more fetch */
export function refreshBots(): Promise<void> {
  if (inflight) {
    again = true;
    return inflight;
  }
  inflight = hubCall<{ bots: BotEntry[]; clientId: string }>('bots.list', {})
    // A remote listed before the id was known may be this app itself
    .then((r) => set({ ...st, list: r.bots, clientId: r.clientId, remotes: st.remotes.filter((x) => x.clientId !== r.clientId) }))
    .catch(() => set({ ...st, list: null, clientId: null }))
    .finally(() => {
      inflight = null;
      if (again) {
        again = false;
        refreshBots();
      }
    });
  return inflight;
}

/**
 * A Bot's own session or one of its tasks whose Bot is listed here (this app's engine, or a reachable remote
 * desktop): shown under the Bots section instead of under the host
 */
export function isListedBotSession(s: SessionInfo): boolean {
  return !!s.bot && sourceOf(s.bot.clientId) !== null;
}

/** The source that runs Bots of desktop `clientId`: '' for this app, a remote key, or null when not listed */
export function sourceOf(clientId: string | null | undefined): string | null {
  if (!clientId) return null;
  if (st.clientId && clientId === st.clientId) return '';
  return st.remotes.find((r) => r.clientId === clientId && r.list)?.key ?? null;
}

function listOf(src: string): BotEntry[] {
  return (src ? remoteOf(src)?.list : st.list) ?? [];
}

export function remoteKey(hostId: string, clientId: string) {
  return `${hostId}|${clientId}`;
}

function splitKey(src: string): [string, string] {
  const i = src.indexOf('|');
  return [src.slice(0, i), src.slice(i + 1)];
}

/** Run a Bots command on this app (src '') or on a remote desktop */
export function botCall<T = unknown>(cmd: string, args: Record<string, unknown> = {}, src = ''): Promise<T> {
  if (!src) return hubCall<T>(cmd, args);
  const [hostId, clientId] = splitKey(src);
  return rpc<T>(hostId, 'client.call', { clientId, cmd, args });
}

/** The Bots source the components below work on (BotsSection, BotView, PluginsView) */
export const BotSource = createContext('');

export function useBotSource() {
  return useContext(BotSource);
}

/** `botCall` bound to the current source */
export function useBotCall() {
  const src = useContext(BotSource);
  return <T = unknown,>(cmd: string, args: Record<string, unknown> = {}) => botCall<T>(cmd, args, src);
}

/** The Bot list of a source */
export function useBotList(src: string): BotEntry[] | null {
  return useBots((s) => (src ? (s.remotes.find((r) => r.key === src)?.list ?? null) : s.list));
}

export function remoteOf(src: string): RemoteBots | undefined {
  return st.remotes.find((r) => r.key === src);
}

const remoteLoads = new Map<string, Promise<void>>();

/** Find the desktop apps behind a host (`clients.list`) and load their Bots */
export function refreshRemoteHost(hostId: string): Promise<void> {
  const running = remoteLoads.get(hostId);
  if (running) return running;
  // Know this app's own client id first, or it would be listed as a remote of itself
  const p = (st.clientId ? Promise.resolve() : (inflight ?? refreshBots()))
    .then(() => rpc<{ clientId: string; name: string }[]>(hostId, 'clients.list'))
    .catch(() => [] as { clientId: string; name: string }[])
    .then(async (clients) => {
      // This app's own engine is not a remote
      const found = clients.filter((c) => c.clientId !== st.clientId);
      const keep = st.remotes.filter((r) => r.hostId !== hostId);
      const mine: RemoteBots[] = found.map((c) => {
        const key = remoteKey(hostId, c.clientId);
        const old = st.remotes.find((r) => r.key === key);
        return { key, hostId, clientId: c.clientId, name: c.name, list: old?.list ?? null };
      });
      set({ ...st, remotes: [...keep, ...mine] });
      await Promise.all(mine.map((r) => refreshRemote(r.key)));
    })
    .finally(() => remoteLoads.delete(hostId));
  remoteLoads.set(hostId, p);
  return p;
}

/** Refetch one remote desktop's Bots */
export function refreshRemote(key: string): Promise<void> {
  return botCall<{ bots: BotEntry[] }>('bots.list', {}, key)
    .then((r) => patchRemote(key, { list: r.bots, error: undefined }))
    .catch((e) => patchRemote(key, { list: null, error: String(e?.message ?? e) }));
}

function patchRemote(key: string, p: Partial<RemoteBots>) {
  if (!st.remotes.some((r) => r.key === key)) return;
  set({ ...st, remotes: st.remotes.map((r) => (r.key === key ? { ...r, ...p } : r)) });
}

/** A host went away: its desktops can't be reached */
export function dropRemoteHost(hostId: string) {
  if (st.remotes.some((r) => r.hostId === hostId)) set({ ...st, remotes: st.remotes.filter((r) => r.hostId !== hostId) });
}

/**
 * A remote desktop's host id as seen from this client: the desktop's `local` is the host we reach it through, any
 * other host X is the forwarded `<host id>~X` (shared with paired phones). yi-web on the desktop's machine reaches
 * it through its own `local` and may have the same host X. null: not reachable from here
 */
export function mapHost(src: string, desktopHost: string, known: (id: string) => boolean): string | null {
  if (!src) return desktopHost;
  const [hostId] = splitKey(src);
  if (desktopHost === 'local') return hostId;
  const fwd = `${hostId}~${desktopHost}`;
  if (known(fwd)) return fwd;
  return hostId === 'local' && known(desktopHost) ? desktopHost : null;
}

/** The reverse of mapHost: this client's host id → the id the source's desktop uses; null when it can't reach it */
export function unmapHost(src: string, clientHost: string): string | null {
  if (!src) return clientHost;
  const [hostId] = splitKey(src);
  if (clientHost === hostId) return 'local';
  if (clientHost.startsWith(`${hostId}~`)) return clientHost.slice(hostId.length + 1);
  return hostId === 'local' ? clientHost : null;
}

export function findTask(taskId: string, src = ''): TaskRow | undefined {
  for (const b of listOf(src)) {
    const t = b.tasks.find((x) => x.id === taskId);
    if (t) return t;
  }
  return undefined;
}

export function findBot(botId: string, src = ''): BotEntry | undefined {
  return listOf(src).find((b) => b.config.id === botId);
}

export const ACTIVE_TASK: TaskState[] = ['queued', 'working', 'needsYou'];

/** The owner typed into a task's session: the task becomes manual (the Bot stops steering it until handed back) */
export function noteOwnerMessage(s: SessionInfo) {
  const src = sourceOf(s.bot?.clientId);
  if (src === null || s.bot?.role !== 'task' || !s.bot.taskId) return;
  const task = findTask(s.bot.taskId, src);
  if (!task || task.manual || !ACTIVE_TASK.includes(task.state)) return;
  botCall('tasks.manual', { id: task.id }, src).catch(() => {});
}

