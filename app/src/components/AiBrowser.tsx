import { useEffect, useMemo, useRef, useState } from 'react';
import { getT, useLang } from '../i18n';
import { errorToast, onBrowserView, sessionInfo, useStore } from '../store';
import { rpc } from '../transport';
import type { BrowserViewStatus } from '../types';
import { Icon } from './Icon';
import { openMenu } from './Overlay';

const t = getT('browserUse');

/** File panel tab of a session's AI browser: the live picture of the Chrome the AI drives on the session's host */
export const AI_PREFIX = 'aiweb:';
export const isAiTab = (tab: string) => tab.startsWith(AI_PREFIX);
export const aiTabSession = (tab: string) => tab.slice(AI_PREFIX.length);

/** Whether the AI is acting in a session's browser, for the tab bar (fed by the open live view) */
const busyMap = new Map<string, boolean>();
const busySubs = new Set<() => void>();
function setAiBusy(key: string, v: boolean) {
  if ((busyMap.get(key) ?? false) === v) return;
  busyMap.set(key, v);
  for (const fn of busySubs) fn();
}
export function useAiBrowserBusy(hostId: string, sessionId: string): boolean {
  const [, bump] = useState(0);
  useEffect(() => {
    const fn = () => bump((n) => n + 1);
    busySubs.add(fn);
    return () => void busySubs.delete(fn);
  }, []);
  return busyMap.get(`${hostId}/${sessionId}`) ?? false;
}

interface Meta {
  deviceWidth?: number;
  deviceHeight?: number;
}

/** Picture quality presets; null leaves the daemon's default for this connection (local: sharp, remote: light) */
const QUALITY: Record<string, { maxFps: number; maxWidth: number; quality: number }> = {
  low: { maxFps: 2, maxWidth: 640, quality: 40 },
  medium: { maxFps: 6, maxWidth: 960, quality: 55 },
  high: { maxFps: 20, maxWidth: 1280, quality: 70 },
};

const MOD = (e: { altKey: boolean; ctrlKey: boolean; metaKey: boolean; shiftKey: boolean }) =>
  (e.altKey ? 1 : 0) | (e.ctrlKey ? 2 : 0) | (e.metaKey ? 4 : 0) | (e.shiftKey ? 8 : 0);
const BUTTON = ['left', 'middle', 'right', 'back', 'forward'];

export function AiBrowserPane({ hostId, sessionId }: { hostId: string; sessionId: string }) {
  const [, lang] = useLang();
  const connected = useStore((s) => s.hosts.find((h) => h.config.id === hostId)?.status.status === 'connected');
  const isLocal = useStore((s) => s.hosts.find((h) => h.config.id === hostId)?.config.kind === 'local');
  const enabledInfo = useStore((s) => !!s.sessions[hostId]?.find((x) => x.id === sessionId)?.browserUse);
  const sessionStatus = useStore((s) => s.sessions[hostId]?.find((x) => x.id === sessionId)?.status);
  const [status, setStatus] = useState<BrowserViewStatus | null>(null);
  const [hasFrame, setHasFrame] = useState(false);
  const [visible, setVisible] = useState(() => document.visibilityState === 'visible');
  const [quality, setQuality] = useState<string | null>(null);
  const [address, setAddress] = useState('');
  const [editing, setEditing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [promptText, setPromptText] = useState('');
  const [hasDisplay, setHasDisplay] = useState(false);
  const canvas = useRef<HTMLCanvasElement>(null);
  const box = useRef<HTMLDivElement>(null);
  const keys = useRef<HTMLTextAreaElement>(null);
  const meta = useRef<Meta>({});
  const composing = useRef(false);

  useEffect(() => {
    const on = () => setVisible(document.visibilityState === 'visible');
    document.addEventListener('visibilitychange', on);
    return () => document.removeEventListener('visibilitychange', on);
  }, []);

  // Size the canvas to fit the panel at the viewport's aspect ratio, so pointer positions map straight to the page
  const fit = () => {
    const c = canvas.current;
    const b = box.current;
    if (!c || !b || !c.width || !c.height) return;
    const s = Math.min(b.clientWidth / c.width, b.clientHeight / c.height);
    c.style.width = `${Math.floor(c.width * s)}px`;
    c.style.height = `${Math.floor(c.height * s)}px`;
  };
  useEffect(() => {
    const b = box.current;
    if (!b) return;
    const ro = new ResizeObserver(fit);
    ro.observe(b);
    return () => ro.disconnect();
  }, []);

  // Watch while the tab is on screen: frames and status come only to this connection
  const attached = connected && visible && enabledInfo;
  useEffect(() => {
    if (!attached) return;
    let alive = true;
    let drawing = false;
    let pending: { seq: number; data: string; metadata: Meta } | null = null;
    const draw = async () => {
      if (drawing || !pending) return;
      drawing = true;
      const f = pending;
      pending = null;
      try {
        const img = new Image();
        img.src = `data:image/jpeg;base64,${f.data}`;
        await img.decode();
        const c = canvas.current;
        if (alive && c) {
          if (c.width !== img.naturalWidth || c.height !== img.naturalHeight) {
            c.width = img.naturalWidth;
            c.height = img.naturalHeight;
            fit();
          }
          c.getContext('2d')?.drawImage(img, 0, 0);
          meta.current = f.metadata ?? {};
          setHasFrame(true);
        }
      } catch {
        // A frame that doesn't decode is skipped; the next one replaces it
      }
      // Drawn: the daemon may send the next frame now
      rpc(hostId, 'browser.view.ack', { sessionId, seq: f.seq }).catch(() => {});
      drawing = false;
      if (pending) void draw();
    };
    const off = onBrowserView((h, method, p) => {
      if (h !== hostId || p?.sessionId !== sessionId) return;
      if (method === 'browser.status') setStatus(p as BrowserViewStatus);
      else {
        pending = p;
        void draw();
      }
    });
    rpc(hostId, 'browser.view.attach', { sessionId, ...(quality ? QUALITY[quality] : {}) }).catch((e) => alive && errorToast(e));
    rpc<{ hasDisplay?: boolean }>(hostId, 'browserUse.status', { sessionId })
      .then((r) => alive && setHasDisplay(!!r.hasDisplay))
      .catch(() => {});
    return () => {
      alive = false;
      off();
      rpc(hostId, 'browser.view.detach', { sessionId }).catch(() => {});
    };
  }, [attached, hostId, sessionId, quality]); // eslint-disable-line react-hooks/exhaustive-deps

  // Like Claude in Chrome: the page glows while the AI acts, and stays lit briefly between its steps
  const [glow, setGlow] = useState(false);
  const lastTool = useRef('');
  if (status?.busy) lastTool.current = status.busy;
  useEffect(() => {
    if (status?.busy && !status.paused) {
      setGlow(true);
      return;
    }
    const timer = setTimeout(() => setGlow(false), 1500);
    return () => clearTimeout(timer);
  }, [status?.busy, status?.paused]);
  useEffect(() => {
    setAiBusy(`${hostId}/${sessionId}`, glow);
    return () => setAiBusy(`${hostId}/${sessionId}`, false);
  }, [glow, hostId, sessionId]);
  const stop = () => rpc(hostId, 'session.interrupt', { sessionId }).catch(errorToast);

  useEffect(() => {
    if (!editing) setAddress(status?.url && status.url !== 'about:blank' ? status.url : '');
  }, [status?.url, editing]);

  const control = async (action: string, params: Record<string, unknown> = {}) => {
    setBusy(true);
    try {
      await rpc(hostId, 'session.browserUse', { sessionId, action, ...params });
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  // Input is sent in order, one batch in flight (AGENTS.md: ordered input is queued in the frontend)
  const queue = useRef<object[]>([]);
  const sending = useRef(false);
  const lastMove = useRef(0);
  const flush = async () => {
    if (sending.current || !queue.current.length) return;
    sending.current = true;
    const events = queue.current.splice(0);
    try {
      await rpc(hostId, 'browser.view.input', { sessionId, events });
    } catch (e) {
      errorToast(e);
    } finally {
      sending.current = false;
      if (queue.current.length) void flush();
    }
  };
  const send = (ev: object) => {
    queue.current.push(ev);
    void flush();
  };

  /** Viewport CSS pixels of a pointer position over the canvas */
  const at = (e: { clientX: number; clientY: number }) => {
    const c = canvas.current!;
    const r = c.getBoundingClientRect();
    const w = meta.current.deviceWidth || c.width;
    const h = meta.current.deviceHeight || c.height;
    return { x: Math.round(((e.clientX - r.left) / r.width) * w), y: Math.round(((e.clientY - r.top) / r.height) * h) };
  };
  const canInput = !!status?.running && hasFrame;
  // Touch: a drag scrolls, a tap clicks (the page sees a desktop mouse, like the AI's clicks)
  const touch = useRef<{ x: number; y: number; moved: boolean } | null>(null);
  const pointerHandlers = {
    onPointerDown: (e: React.PointerEvent<HTMLCanvasElement>) => {
      if (!canInput) return;
      keys.current?.focus({ preventScroll: true });
      e.preventDefault();
      if (e.pointerType === 'touch') {
        touch.current = { x: e.clientX, y: e.clientY, moved: false };
        return;
      }
      e.currentTarget.setPointerCapture(e.pointerId);
      send({ type: 'mouse', event: 'down', ...at(e), button: BUTTON[e.button] ?? 'left', buttons: e.buttons, clickCount: e.detail || 1, modifiers: MOD(e) });
    },
    onPointerMove: (e: React.PointerEvent<HTMLCanvasElement>) => {
      if (!canInput) return;
      if (e.pointerType === 'touch') {
        const t0 = touch.current;
        if (!t0) return;
        const dx = t0.x - e.clientX;
        const dy = t0.y - e.clientY;
        if (!t0.moved && Math.hypot(dx, dy) < 8) return;
        const r = e.currentTarget.getBoundingClientRect();
        const scale = (meta.current.deviceWidth || e.currentTarget.width) / r.width;
        touch.current = { x: e.clientX, y: e.clientY, moved: true };
        send({ type: 'mouse', event: 'wheel', ...at(e), deltaX: dx * scale, deltaY: dy * scale });
        return;
      }
      // Hover matters (menus, tooltips) but not every pixel of it
      const now = performance.now();
      if (!e.buttons && now - lastMove.current < 50) return;
      lastMove.current = now;
      send({ type: 'mouse', event: 'move', ...at(e), buttons: e.buttons, modifiers: MOD(e) });
    },
    onPointerUp: (e: React.PointerEvent<HTMLCanvasElement>) => {
      if (!canInput) return;
      if (e.pointerType === 'touch') {
        const t0 = touch.current;
        touch.current = null;
        if (t0 && !t0.moved) {
          send({ type: 'mouse', event: 'down', ...at(e), button: 'left', buttons: 1, clickCount: 1 });
          send({ type: 'mouse', event: 'up', ...at(e), button: 'left', buttons: 0, clickCount: 1 });
        }
        return;
      }
      send({ type: 'mouse', event: 'up', ...at(e), button: BUTTON[e.button] ?? 'left', buttons: e.buttons, clickCount: e.detail || 1, modifiers: MOD(e) });
    },
    onWheel: (e: React.WheelEvent<HTMLCanvasElement>) => {
      if (!canInput) return;
      const k = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 600 : 1;
      send({ type: 'mouse', event: 'wheel', ...at(e), deltaX: e.deltaX * k, deltaY: e.deltaY * k, modifiers: MOD(e) });
    },
    onContextMenu: (e: React.MouseEvent) => e.preventDefault(),
  };
  // A hidden textarea takes the keyboard: plain keys go as key events, IME and pastes as committed text
  const keyHandlers = {
    onKeyDown: (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (!canInput || composing.current || e.nativeEvent.isComposing || e.keyCode === 229) return;
      e.preventDefault();
      const printable = e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey;
      const text = printable ? e.key : e.key === 'Enter' ? '\r' : '';
      send({ type: 'key', event: 'down', key: e.key, code: e.code, keyCode: e.keyCode, text, modifiers: MOD(e), repeat: e.repeat });
    },
    onKeyUp: (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (!canInput || composing.current || e.keyCode === 229) return;
      e.preventDefault();
      send({ type: 'key', event: 'up', key: e.key, code: e.code, keyCode: e.keyCode, modifiers: MOD(e) });
    },
    onCompositionStart: () => {
      composing.current = true;
    },
    onCompositionEnd: (e: React.CompositionEvent<HTMLTextAreaElement>) => {
      composing.current = false;
      if (canInput && e.data) send({ type: 'text', text: e.data });
      e.currentTarget.value = '';
    },
    // Text that arrives without key events (mobile keyboards, dictation)
    onInput: (e: React.FormEvent<HTMLTextAreaElement>) => {
      const el = e.currentTarget;
      if (composing.current) return;
      if (canInput && el.value) send({ type: 'text', text: el.value });
      el.value = '';
    },
    onPaste: (e: React.ClipboardEvent<HTMLTextAreaElement>) => {
      e.preventDefault();
      const text = e.clipboardData.getData('text/plain');
      if (canInput && text) send({ type: 'text', text });
    },
  };

  const label = useMemo(() => {
    if (!status) return '';
    if (!status.enabled) return t('viewOff');
    if (status.paused) return t('viewPaused');
    if (status.busy || glow) return t('viewAiActing', { tool: (status.busy || lastTool.current).replace(/^browser_/, '') });
    if (!status.running) return t('viewNotStarted');
    if (!status.headless) return t('viewHeaded');
    return status.loading ? t('viewLoading') : t('viewIdle');
  }, [status, glow, lang]); // eslint-disable-line react-hooks/exhaustive-deps

  const turn = sessionStatus === 'running' || sessionStatus === 'waiting-permission';
  if (!enabledInfo)
    return (
      <div className="wb">
        <div className="wp-empty">
          <div className="wp-empty-title">{t('viewOffTitle')}</div>
          <div className="muted small">{t('viewOffHint')}</div>
          <button className="btn primary" disabled={busy || turn || !connected} onClick={() => void control('enable')}>
            {t('enable')}
          </button>
          {turn && <div className="muted small">{t('waitForTurn')}</div>}
        </div>
      </div>
    );

  const tabs = status?.tabs ?? [];
  const tabMenu = (e: React.MouseEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    openMenu(
      { clientX: r.left, clientY: r.bottom + 4 },
      tabs.map((x) => ({ label: x.title || x.url, hint: x.id, active: x.current, run: () => void control('tab', { tab: x.id }) })),
    );
  };
  const qualityMenu = (e: React.MouseEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    openMenu(
      { clientX: r.right - 200, clientY: r.bottom + 4 },
      [
        { label: t('qualityAuto'), active: quality === null, run: () => setQuality(null) },
        { label: t('qualityLow'), active: quality === 'low', run: () => setQuality('low') },
        { label: t('qualityMedium'), active: quality === 'medium', run: () => setQuality('medium') },
        { label: t('qualityHigh'), active: quality === 'high', run: () => setQuality('high') },
      ],
    );
  };
  const running = !!status?.running;
  const dialog = status?.dialog;
  return (
    <div className="wb aiweb">
      <div className="wb-bar">
        <button className="icon-btn" title={t('viewBack')} disabled={!running || busy} onClick={() => void control('back')}>
          <Icon name="back" size={14} />
        </button>
        <button className="icon-btn" title={t('viewForward')} disabled={!running || busy} onClick={() => void control('forward')}>
          <Icon name="forward" size={14} />
        </button>
        <button className="icon-btn" title={t('viewReload')} disabled={!running || busy} onClick={() => void control('reload')}>
          <Icon name="refresh" size={14} />
        </button>
        <form
          className="wb-addr"
          onSubmit={(e) => {
            e.preventDefault();
            const v = address.trim();
            setEditing(false);
            (e.currentTarget.querySelector('input') as HTMLInputElement | null)?.blur();
            if (v) void control('open', { url: v });
          }}
        >
          {(status?.loading || status?.busy) && <span className="spinner" />}
          <input
            value={address}
            onChange={(e) => setAddress(e.target.value)}
            onFocus={(e) => {
              setEditing(true);
              e.target.select();
            }}
            onBlur={() => setEditing(false)}
            placeholder={t('viewAddress')}
            aria-label={t('viewAddress')}
            spellCheck={false}
          />
        </form>
        {tabs.length > 1 && (
          <button className="icon-btn aiweb-tabs" title={t('viewTabs', { count: tabs.length })} onClick={tabMenu}>
            {tabs.length}
          </button>
        )}
        <button className="icon-btn" title={t('quality')} onClick={qualityMenu}>
          <Icon name="settings" size={14} />
        </button>
        {status?.paused ? (
          <button className="icon-btn" title={t('viewResume')} disabled={busy} onClick={() => void control('resume')}>
            <Icon name="play" size={14} />
          </button>
        ) : (
          <button className="icon-btn" title={t('viewPause')} disabled={busy} onClick={() => void control('pause')}>
            <Icon name="pause" size={14} />
          </button>
        )}
        {isLocal && hasDisplay && running && (
          <button
            className="icon-btn"
            title={status?.headless ? t('viewHeadedButton') : t('viewHeadlessButton')}
            disabled={busy}
            onClick={() => void control(status?.headless ? 'headed' : 'headless')}
          >
            <Icon name={status?.headless ? 'external' : 'minimize'} size={14} />
          </button>
        )}
      </div>
      <div className={`aiweb-status ${status?.busy || glow ? 'acting' : ''} ${status?.paused ? 'paused' : ''}`} role="status">
        <span className="aiweb-dot" />
        <span className="truncate">{label}</span>
        {status?.frame && <span className="muted truncate">{t('viewInFrame', { frame: status.frame })}</span>}
        {status?.title && <span className="muted truncate aiweb-title">{status.title}</span>}
      </div>
      {dialog && (
        <div className="aiweb-dialog" role="alertdialog">
          <div>
            <strong>{t('viewDialog', { kind: dialog.kind })}</strong> {dialog.message}
          </div>
          {dialog.kind === 'prompt' && <input value={promptText} placeholder={dialog.defaultPrompt} onChange={(e) => setPromptText(e.target.value)} />}
          <div className="dlg-actions">
            <button className="btn" onClick={() => send({ type: 'dialog', accept: false })}>
              {t('viewDismiss')}
            </button>
            <button className="btn primary" onClick={() => send({ type: 'dialog', accept: true, ...(dialog.kind === 'prompt' ? { text: promptText || dialog.defaultPrompt } : {}) })}>
              {t('viewAccept')}
            </button>
          </div>
        </div>
      )}
      <div className="aiweb-body" ref={box}>
        <div className={`aiweb-stage ${hasFrame && running ? '' : 'hidden'} ${glow ? 'glow' : ''}`}>
          <canvas ref={canvas} className="aiweb-canvas" tabIndex={-1} aria-label={t('viewCanvas')} {...pointerHandlers} />
          {status?.pointer && hasFrame && <AiCursor p={status.pointer} w={meta.current.deviceWidth || canvas.current?.width || 1} h={meta.current.deviceHeight || canvas.current?.height || 1} />}
        </div>
        {/* Outside the stage so it shows while the first frame is still coming */}
        {glow && (
          <div className="aiweb-acting" role="status">
            <span className="aiweb-acting-dot" />
            <span>{t('viewActingBanner')}</span>
            <button className="aiweb-stop" title={t('viewStopHint')} onClick={() => void stop()}>
              <Icon name="stop" size={12} /> {t('viewStop')}
            </button>
          </div>
        )}
        <textarea ref={keys} className="aiweb-keys" aria-label={t('viewKeyboard')} autoCapitalize="off" autoComplete="off" spellCheck={false} {...keyHandlers} />
        {!(hasFrame && running) && (
          <div className="wp-empty aiweb-empty">
            {!connected || !status ? (
              <div className="muted small">{t('viewConnecting')}</div>
            ) : !running ? (
              <>
                <div className="wp-empty-title">{t('viewNotStartedTitle')}</div>
                <div className="muted small">{t('viewNotStartedHint')}</div>
                <button className="btn" disabled={busy} onClick={() => void control('start')}>
                  {t('viewStart')}
                </button>
              </>
            ) : (
              <div className="muted small">{t('viewWaitingFrame')}</div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

/** The AI's mouse drawn over the picture (screencast frames carry no cursor); a ring pulses on each click */
function AiCursor({ p, w, h }: { p: { x: number; y: number; clicks: number }; w: number; h: number }) {
  const style = { left: `${(p.x / w) * 100}%`, top: `${(p.y / h) * 100}%` };
  return (
    <div className="aiweb-cursor" style={style} aria-hidden="true">
      {p.clicks > 0 && <span key={p.clicks} className="aiweb-click" />}
      <svg width="18" height="22" viewBox="0 0 18 22">
        <path d="M1 1 L1 17 L5.5 13 L8.5 20 L11.5 18.8 L8.6 12 L14.5 12 Z" fill="#ea580c" stroke="#fff" strokeWidth="1.5" strokeLinejoin="round" />
      </svg>
    </div>
  );
}

/** Open a URL in the session's AI browser, turning the browser on first if needed */
export async function handToAiBrowser(hostId: string, sessionId: string, url: string) {
  const info = sessionInfo(hostId, sessionId);
  if (!info?.browserUse) await rpc(hostId, 'session.browserUse', { sessionId, action: 'enable' });
  await rpc(hostId, 'session.browserUse', { sessionId, action: 'open', url });
}
