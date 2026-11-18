import { useEffect, useRef, useState } from 'react';
import { canZoom, getFontPrefs, zoomFactor } from '../fontPrefs';

// Web tab as a native child webview of the main window (app/src-tauri/src/webtab.rs). A placeholder div keeps
// the space in the layout; every few frames its rectangle is sent to the backend.
//
// A native view paints above everything in the main webview, so whatever of the app's own lies on top of the
// placeholder (menu, toast, dropdown) would vanish under it. Instead, the parts of the view under those
// elements are cut out of its window region: the app's element shows through the hole and takes clicks, and
// the rest of the page stays live. Something covering most of the view (a modal with its backdrop) swaps the
// view for a screen shot of the page until it closes, like VS Code's integrated browser does.
//
// Webviews outlive the component: unmounting (switching tabs, closing the panel) only hides and parks them by
// tab key, so coming back doesn't reload the page. At most POOL parked webviews are kept.

/** Desktop app on Windows: the only platform with multiple webviews per window enabled (Cargo.toml) */
export const NATIVE_WEB = canZoom && /Windows/.test(navigator.userAgent);

export type WebEvent = { kind: 'loading' | 'loaded' | 'title' | 'newWindow' | 'blocked' | 'error'; url?: string; title?: string };

const handlers = new Map<string, (e: WebEvent) => void>();
let seq = 0;
const POOL = 6;
/** Parked webviews by tab key, oldest first */
const pool = new Map<string, string>();
/** Web tabs open in each file panel (`registerWebTabs`): a webview whose tab is gone is closed, not parked */
const openTabs = new Map<string, Set<string>>();
const isOpen = (k: string) => [...openTabs.values()].some((s) => s.has(k));

async function invoke<T = unknown>(cmd: string, args: Record<string, unknown> = {}) {
  const { invoke } = await import('@tauri-apps/api/core');
  return invoke<T>(cmd, args);
}

if (NATIVE_WEB) {
  // Webviews of a previous page load (reload, HMR) are orphans now
  void invoke('webtab_reset').catch(() => {});
  void import('@tauri-apps/api/event').then(({ listen }) =>
    listen<WebEvent & { label: string }>('webtab', (e) => handlers.get(e.payload.label)?.(e.payload)),
  );
}

type Rect = { x: number; y: number; w: number; h: number };
type Cover = { full: boolean; holes: Rect[] };

const clear = (c: string) => c === 'transparent' || /rgba\(.*,\s*0\)$/.test(c);
/** The element draws something of its own (background, shadow, border, backdrop blur) */
function painted(cs: CSSStyleDeclaration) {
  return !clear(cs.backgroundColor) || cs.boxShadow !== 'none' || (cs.backdropFilter && cs.backdropFilter !== 'none') || (cs.borderStyle !== 'none' && parseFloat(cs.borderWidth) > 0 && !clear(cs.borderColor));
}

/**
 * What of the app lies over the placeholder: elementFromPoint on a grid, and for each hit the outermost
 * ancestor that paints something (so a transparent click-catching scrim, like the context menu's, doesn't count;
 * the menu inside it does). Rects are relative to the placeholder, in CSS px.
 */
function coverOf(el: HTMLElement, r: DOMRect): Cover {
  const step = 28;
  const nx = Math.max(3, Math.min(40, Math.ceil(r.width / step)));
  const ny = Math.max(3, Math.min(40, Math.ceil(r.height / step)));
  const seen = new Map<Element, Element | null>();
  const roots = new Set<Element>();
  for (let i = 0; i < nx; i++) {
    for (let j = 0; j < ny; j++) {
      const hit = document.elementFromPoint(r.left + ((i + 0.5) * r.width) / nx, r.top + ((j + 0.5) * r.height) / ny);
      if (!hit || hit === el || el.contains(hit)) continue;
      let root = seen.get(hit);
      if (root === undefined) {
        root = null;
        let top: Element | null = null;
        for (let n: Element | null = hit; n && n !== document.body && !n.contains(el); n = n.parentElement) {
          top = n;
          if (painted(getComputedStyle(n))) root = n;
        }
        // Bare text or an image with no box of its own still shows; a lone transparent scrim doesn't
        if (!root && top !== hit) root = hit;
        seen.set(hit, root);
      }
      if (root) roots.add(root);
    }
  }
  const holes: Rect[] = [];
  let area = 0;
  for (const n of roots) {
    const b = n.getBoundingClientRect();
    const pad = getComputedStyle(n).boxShadow !== 'none' ? 10 : 0;
    const x1 = Math.max(r.left, b.left - pad);
    const y1 = Math.max(r.top, b.top - pad);
    const x2 = Math.min(r.right, b.right + pad);
    const y2 = Math.min(r.bottom, b.bottom + pad);
    if (x2 <= x1 || y2 <= y1) continue;
    holes.push({ x: x1 - r.left, y: y1 - r.top, w: x2 - x1, h: y2 - y1 });
    area += (x2 - x1) * (y2 - y1);
  }
  return { full: area > 0.6 * r.width * r.height, holes };
}

export function NativeWeb({
  src,
  poolKey,
  reload,
  nav,
  onEvent,
}: {
  src: string;
  /** Tab key: a parked webview with this key is taken over instead of opening a new one */
  poolKey: string;
  reload: number;
  nav: { n: number; action: 'back' | 'forward' };
  onEvent: (e: WebEvent) => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const labelRef = useRef('');
  const ev = useRef(onEvent);
  ev.current = onEvent;
  const keyRef = useRef(poolKey);
  keyRef.current = poolKey;
  const ready = useRef<Promise<unknown> | null>(null);
  const shownSrc = useRef(src);
  // Screen shot shown while the view is covered by a modal
  const [still, setStill] = useState('');

  useEffect(() => {
    let label = pool.get(poolKey);
    const reused = !!label;
    if (label) {
      pool.delete(poolKey);
      ready.current = Promise.resolve();
    } else {
      // A fresh label per webview (StrictMode mounts twice; the first one is parked by then and reused)
      label = `webtab-${++seq}-${Date.now().toString(36)}`;
    }
    labelRef.current = label;
    handlers.set(label, (e) => ev.current(e));
    let dead = false;
    let raf = 0;
    let frame = 0;
    let last = '';
    let mode: 'live' | 'still' | 'hidden' = 'live';
    let shotAt = 0;
    let cached = '';
    const capture = () =>
      invoke<string>('webtab_capture', { label })
        .then((u) => (cached = u))
        .catch(() => cached);
    // Bounds updates go one at a time, and only the newest pending one is sent
    let busy = false;
    let pending: { b: Rect; visible: boolean; holes: Rect[] } | null = null;
    const flush = () => {
      if (busy || !pending || !ready.current) return;
      const p = pending;
      pending = null;
      busy = true;
      ready.current
        .then(() => invoke('webtab_bounds', { label, bounds: p.b, visible: p.visible, holes: p.holes }))
        .catch(() => {})
        .finally(() => {
          busy = false;
          if (!dead) flush();
        });
    };
    const zoom = () => (canZoom ? zoomFactor(getFontPrefs().zoom) : 1);
    const rect = () => {
      const el = ref.current;
      if (!el) return null;
      const r = el.getBoundingClientRect();
      const z = zoom();
      return { r, z, b: { x: r.left * z, y: r.top * z, w: r.width * z, h: r.height * z } };
    };
    const first = rect();
    if (!reused) {
      ready.current = invoke('webtab_open', { label, url: src, bounds: first?.b ?? { x: 0, y: 0, w: 1, h: 1 } }).catch((e) => {
        ev.current({ kind: 'error', title: String(e?.message ?? e) });
        throw e;
      });
      ready.current.catch(() => {});
    }
    const tick = () => {
      if (dead) return;
      raf = requestAnimationFrame(tick);
      if (frame++ % 3) return;
      const m = rect();
      if (!m) return;
      const { r, z, b } = m;
      const on = r.width > 2 && r.height > 2 && !document.hidden;
      const cover = on ? coverOf(ref.current!, r) : { full: false, holes: [] };
      const next = !on ? 'hidden' : cover.full ? 'still' : 'live';
      const holes = next === 'live' ? cover.holes.map((h) => ({ x: h.x * z, y: h.y * z, w: h.w * z, h: h.h * z })) : [];
      // Keep a recent picture of the page, so a modal can show it without waiting for a capture
      if (next === 'live' && !holes.length && Date.now() - shotAt > 2500) {
        shotAt = Date.now();
        void ready.current?.then(capture);
      }
      const key = `${next}|${b.x},${b.y},${b.w},${b.h}|${holes.map((h) => `${h.x},${h.y},${h.w},${h.h}`).join(';')}`;
      if (key === last) return;
      last = key;
      // What the native view does now: 1 live, h live with holes, 0 hidden or replaced by the screen shot (for tests)
      ref.current?.setAttribute('data-shown', next === 'live' ? (holes.length ? 'h' : '1') : '0');
      if (next === 'still' && mode !== 'still') {
        // The cached picture right away, then a fresh one before the live view goes away
        if (cached) setStill(cached);
        void ready.current
          ?.then(capture)
          .then((u) => {
            if (!dead && mode === 'still' && u) setStill(u);
          })
          .finally(() => {
            if (dead || mode !== 'still') return;
            pending = { b, visible: false, holes: [] };
            flush();
          });
      } else {
        if (next === 'live' && mode === 'still') setTimeout(() => !dead && setStill(''), 120);
        if (next === 'hidden') setStill('');
        pending = { b, visible: next === 'live', holes };
        flush();
      }
      mode = next;
    };
    raf = requestAnimationFrame(tick);
    return () => {
      dead = true;
      cancelAnimationFrame(raf);
      handlers.delete(label!);
      // Park it hidden, right away so a FilePanel effect closing the tab in the same commit finds it;
      // the oldest parked ones beyond POOL are closed
      const k = keyRef.current;
      if (!isOpen(k)) {
        void ready.current?.finally(() => invoke('webtab_close', { label }).catch(() => {}));
        return;
      }
      const old = pool.get(k);
      if (old && old !== label) void invoke('webtab_close', { label: old }).catch(() => {});
      pool.delete(k);
      pool.set(k, label!);
      while (pool.size > POOL) {
        const [ok, ol] = pool.entries().next().value as [string, string];
        pool.delete(ok);
        void invoke('webtab_close', { label: ol }).catch(() => {});
      }
      void ready.current?.then(() => invoke('webtab_bounds', { label, bounds: first?.b ?? { x: 0, y: 0, w: 1, h: 1 }, visible: false, holes: [] })).catch(() => {});
    };
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // A new address from the address bar
  useEffect(() => {
    if (src === shownSrc.current) return;
    shownSrc.current = src;
    void ready.current?.then(() => invoke('webtab_nav', { label: labelRef.current, action: 'go', url: src })).catch(() => {});
  }, [src]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    if (reload) void ready.current?.then(() => invoke('webtab_nav', { label: labelRef.current, action: 'reload' })).catch(() => {});
  }, [reload]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    if (nav.n) void ready.current?.then(() => invoke('webtab_nav', { label: labelRef.current, action: nav.action })).catch(() => {});
  }, [nav]); // eslint-disable-line react-hooks/exhaustive-deps

  return <div ref={ref} className="wb-native">{still && <img className="wb-still" src={still} alt="" draggable={false} />}</div>;
}

/** A file panel's open web tabs (keys `${hostId}|${tab}`); parked webviews of tabs it closed go. `null`: the panel is gone */
export function registerWebTabs(panel: string, keys: string[] | null) {
  const before = openTabs.get(panel);
  if (keys) openTabs.set(panel, new Set(keys));
  else openTabs.delete(panel);
  if (!keys) return;
  for (const k of before ?? []) {
    const l = pool.get(k);
    if (l && !isOpen(k)) {
      pool.delete(k);
      void invoke('webtab_close', { label: l }).catch(() => {});
    }
  }
}
