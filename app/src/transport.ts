// Channel between the frontend and ClientHub: invoke/events in Tauri, yi-web's WebSocket in the browser.
import { getT } from './i18n';

const t = getT('transport');

type Listener = (ev: any) => void;

const listeners = new Set<Listener>();

export const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

export function onHubEvent(fn: Listener): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

function dispatch(ev: any) {
  for (const l of listeners) {
    try {
      l(ev);
    } catch (e) {
      console.error(e);
    }
  }
}

// ---------- Tauri ----------

let tauriReady: Promise<void> | null = null;

async function initTauri() {
  const { listen } = await import('@tauri-apps/api/event');
  await listen('hub-event', (e) => dispatch(e.payload));
}

// ---------- Web ----------

let ws: WebSocket | null = null;
let wsReady: Promise<void> | null = null;
let nextId = 1;
const pending = new Map<number, { res: (v: any) => void; rej: (e: Error) => void }>();
let webStatus: 'connecting' | 'open' | 'closed' = 'connecting';

function webToken(): string {
  const q = new URLSearchParams(location.search).get('token');
  try {
    if (q) sessionStorage.setItem('yi-console-token', q);
    return q || sessionStorage.getItem('yi-console-token') || '';
  } catch {
    return q || '';
  }
}

function connectWeb(): Promise<void> {
  if (wsReady && ws && ws.readyState <= 1) return wsReady;
  wsReady = new Promise((resolve, reject) => {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const sock = new WebSocket(`${proto}://${location.host}/ws?token=${encodeURIComponent(webToken())}`);
    ws = sock;
    webStatus = 'connecting';
    sock.onopen = () => {
      webStatus = 'open';
      dispatch({ type: 'transport', status: 'open' });
      resolve();
    };
    sock.onerror = () => reject(new Error(t('cannotConnectToYiConsole')));
    sock.onclose = () => {
      webStatus = 'closed';
      for (const [, p] of pending) p.rej(new Error(t('connectionClosed')));
      pending.clear();
      dispatch({ type: 'transport', status: 'closed' });
      // Reconnect automatically when the page server restarts
      setTimeout(() => connectWeb().catch(() => {}), 2000);
    };
    sock.onmessage = (m) => {
      const d = JSON.parse(m.data);
      if (d.event) return dispatch(d.event);
      const p = pending.get(d.id);
      if (!p) return;
      pending.delete(d.id);
      if (d.error) p.rej(new Error(d.error));
      else p.res(d.result);
    };
  });
  return wsReady;
}

export function transportStatus() {
  return isTauri ? 'open' : webStatus;
}

export async function hubCall<T = any>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
  if (isTauri) {
    if (!tauriReady) tauriReady = initTauri();
    await tauriReady;
    const { invoke } = await import('@tauri-apps/api/core');
    try {
      return await invoke<T>('hub_call', { cmd, args });
    } catch (e) {
      throw new Error(typeof e === 'string' ? e : (e as Error)?.message ?? String(e));
    }
  }
  await connectWeb();
  return new Promise<T>((res, rej) => {
    const id = nextId++;
    pending.set(id, { res, rej });
    ws!.send(JSON.stringify({ id, cmd, args }));
  });
}

/** Call a method on a remote yi-agentd */
export function rpc<T = any>(hostId: string, method: string, params: Record<string, unknown> = {}): Promise<T> {
  return hubCall<T>('rpc', { hostId, method, params });
}

export async function initTransport() {
  if (isTauri) {
    if (!tauriReady) tauriReady = initTauri();
    await tauriReady;
  } else {
    await connectWeb();
  }
}
