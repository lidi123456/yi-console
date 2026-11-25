import type { HubEvent } from './types.ts';

export const DESKTOP_SERVER = 'yi_computer_use';
export const BROWSER_SERVER = 'yi_browser';

/** A gateway's record of an AI tool's MCP call: the desktop (`yi_computer_use`) or the AI browser (`yi_browser`) */
export type GatewayEvent = HubEvent & { kind: 'computer_use' | 'browser_use' };

export type GatewayToolLike = {
  k: 'tool';
  name: string;
  title: string;
  input?: unknown;
  result?: { output: string; isError: boolean };
  desktop?: GatewayEvent;
};

export type GatewayItemLike =
  | GatewayToolLike
  | { k: 'desktop'; key: string; ev: GatewayEvent }
  | { k: string };

export type PendingGateway = {
  actionId: string;
  ev: GatewayEvent;
  /** The `user` or `turn` boundary item active when this actionId was first seen unmatched. */
  boundary: GatewayItemLike | null;
};

const TOOL_NAME_RE: Record<string, RegExp> = {
  [BROWSER_SERVER]: /^browser_[a-z_]+$/,
  [DESKTOP_SERVER]: /^[a-z][a-z0-9_]*$/,
};

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function matchServerTool(raw: string, server: string): string | null {
  const valid = TOOL_NAME_RE[server] ?? /^[a-z][a-z0-9_]*$/;
  const candidates: string[] = [];
  for (const prefix of [`mcp__${server}__`, `mcp_${server}_`, `${server}.`, `@${server}/`]) {
    if (raw.startsWith(prefix)) candidates.push(raw.slice(prefix.length));
  }
  const suffix = ` (${server})`;
  if (raw.endsWith(suffix)) candidates.push(raw.slice(0, -suffix.length));
  for (const c of candidates) {
    if (valid.test(c)) return c;
  }
  return null;
}

/** Extracts the exact built-in gateway tool name for `server` across Claude, Antigravity, Codex, Qoder, Kiro, and CodeBuddy */
export function gatewayToolName(tool: { name: string; title: string; input?: unknown }, server: string): string | null {
  const wrapped = (tool.input as { toolName?: unknown } | null | undefined)?.toolName;
  for (const s of [tool.name, typeof wrapped === 'string' ? wrapped : '', tool.title]) {
    if (!s) continue;
    const matched = matchServerTool(s, server);
    if (matched) return matched;
  }
  return null;
}

/** The yi_browser tool a row calls across all supported AI tools */
export function browserToolName(tool: { name: string; title: string; input?: unknown }): string | null {
  return gatewayToolName(tool, BROWSER_SERVER);
}

/** Whether a tool row is a call to the gateway MCP server for `ev` */
export function isDesktopTool(
  it: { name: string; title: string; input?: unknown },
  ev: { kind: 'computer_use' | 'browser_use'; tool: string },
): boolean {
  const server = ev.kind === 'browser_use' ? BROWSER_SERVER : DESKTOP_SERVER;
  const extracted = gatewayToolName(it, server);
  if (extracted !== null) return extracted === ev.tool;
  const wrapped = (it.input as { toolName?: unknown } | null | undefined)?.toolName;
  const id = `${it.name} ${it.title} ${typeof wrapped === 'string' ? wrapped : ''}`;
  if (!id.includes(server)) return false;
  return new RegExp(`(?:^|[^a-z0-9_])${escapeRegExp(ev.tool)}(?=$|[^a-z0-9_])`).test(id);
}

function lastTurnBoundary(items: readonly GatewayItemLike[]): number {
  for (let i = items.length - 1; i >= 0; i--) {
    const k = items[i].k;
    if (k === 'user' || k === 'turn') return i;
  }
  return -1;
}

function lastTurnBoundaryItem(items: readonly GatewayItemLike[]): GatewayItemLike | null {
  const idx = lastTurnBoundary(items);
  return idx >= 0 ? items[idx] : null;
}

function removeDesktopCard(
  items: GatewayItemLike[],
  actionId: string,
  adjustPlanIdx?: (removedIdx: number) => void,
): void {
  for (let i = items.length - 1; i >= 0; i--) {
    const item = items[i];
    if (item.k === 'desktop' && (item as { ev: GatewayEvent }).ev.actionId === actionId) {
      items.splice(i, 1);
      adjustPlanIdx?.(i);
    }
  }
}

function clearPendingGateway(
  items: GatewayItemLike[],
  pending: PendingGateway[],
  actionId: string,
  adjustPlanIdx?: (removedIdx: number) => void,
): void {
  for (let i = pending.length - 1; i >= 0; i--) {
    if (pending[i].actionId === actionId) pending.splice(i, 1);
  }
  removeDesktopCard(items, actionId, adjustPlanIdx);
}

/** Finds the tool call in the current turn that owns `ev`: earliest unmatched exact match, else latest pending call */
export function findGatewayTargetTool<T extends GatewayToolLike>(
  items: readonly GatewayItemLike[],
  ev: GatewayEvent,
): T | undefined {
  const boundary = lastTurnBoundary(items);
  for (let i = boundary + 1; i < items.length; i++) {
    const it = items[i];
    if (it.k === 'tool' && !(it as GatewayToolLike).desktop && isDesktopTool(it as GatewayToolLike, ev)) {
      return it as T;
    }
  }
  for (let i = items.length - 1; i > boundary; i--) {
    const it = items[i];
    if (it.k === 'tool' && !(it as GatewayToolLike).desktop && !(it as GatewayToolLike).result) {
      return it as T;
    }
  }
  return undefined;
}

/** Records or updates an unmatched gateway event while preserving first-seen queue order and turn boundary */
export function recordOrphanGateway(
  items: GatewayItemLike[],
  pending: PendingGateway[],
  ev: GatewayEvent,
): void {
  const idx = pending.findIndex((entry) => entry.actionId === ev.actionId);
  if (idx >= 0) pending[idx].ev = ev;
  else pending.push({ actionId: ev.actionId, ev, boundary: lastTurnBoundaryItem(items) });

  if (!ev.images?.length && ev.status === 'running') return;
  const old = items.findIndex((item) => item.k === 'desktop' && (item as { ev: GatewayEvent }).ev.actionId === ev.actionId);
  const next: GatewayItemLike = { k: 'desktop', key: `desktop-${ev.actionId}`, ev };
  if (old >= 0) items[old] = next;
  else items.push(next);
}

/** Routes a gateway event to its bound or current-turn target tool row, or records it in first-seen pending order */
export function applyGatewayEvent<T extends GatewayToolLike>(
  items: GatewayItemLike[],
  pending: PendingGateway[],
  desktopTools: Map<string, T>,
  ev: GatewayEvent,
  adjustPlanIdx?: (removedIdx: number) => void,
): void {
  const bound = desktopTools.get(ev.actionId);
  if (bound) {
    bound.desktop = ev;
    return;
  }
  const existing = pending.find((entry) => entry.actionId === ev.actionId);
  const currentBoundary = lastTurnBoundaryItem(items);
  // Only match within the same turn in which the action first arrived
  if (!existing || existing.boundary === currentBoundary) {
    const tool = findGatewayTargetTool<T>(items, ev);
    if (tool) {
      tool.desktop = ev;
      desktopTools.set(ev.actionId, tool);
      clearPendingGateway(items, pending, ev.actionId, adjustPlanIdx);
      return;
    }
  }
  recordOrphanGateway(items, pending, ev);
}

/** Claims the earliest unmatched same-turn gateway event in FIFO order when its `tool_call` arrives */
export function claimEarlyGatewayForTool<T extends GatewayToolLike>(
  items: GatewayItemLike[],
  pending: PendingGateway[],
  desktopTools: Map<string, T>,
  tool: T,
  adjustPlanIdx?: (removedIdx: number) => void,
): boolean {
  if (tool.desktop) return false;
  const currentBoundary = lastTurnBoundaryItem(items);
  const idx = pending.findIndex(
    (entry) =>
      entry.boundary === currentBoundary &&
      !desktopTools.has(entry.actionId) &&
      isDesktopTool(tool, entry.ev),
  );
  if (idx < 0) return false;
  const [entry] = pending.splice(idx, 1);
  removeDesktopCard(items, entry.actionId, adjustPlanIdx);
  tool.desktop = entry.ev;
  desktopTools.set(entry.actionId, tool);
  return true;
}
