import { FitAddon } from '@xterm/addon-fit';
import { Unicode11Addon } from '@xterm/addon-unicode11';
import { WebglAddon } from '@xterm/addon-webgl';
import { Terminal, type ITheme } from '@xterm/xterm';
import '@xterm/xterm/css/xterm.css';
import { useEffect, useRef, useState } from 'react';
import { readClipboard, writeClipboard } from '../clipboard';
import { tr } from '../i18n';
import { getSession, loadSession, skey } from '../store';
import { rpc } from '../transport';
import { lsGet, lsSet, toB64 } from '../util';
import { openMenu } from './Overlay';
import { isAppDark, onThemeChange, useTheme } from '../theme';

// Dark: VS Code's integrated terminal (Dark Modern background, default ANSI colors); light: Windows Terminal's One Half Light
const DARK: ITheme = {
  background: '#1f1f1f',
  foreground: '#cccccc',
  cursor: '#aeafad',
  cursorAccent: '#1f1f1f',
  selectionBackground: '#264f78',
  black: '#000000',
  red: '#cd3131',
  green: '#0dbc79',
  yellow: '#e5e510',
  blue: '#2472c8',
  magenta: '#bc3fbc',
  cyan: '#11a8cd',
  white: '#e5e5e5',
  brightBlack: '#666666',
  brightRed: '#f14c4c',
  brightGreen: '#23d18b',
  brightYellow: '#f5f543',
  brightBlue: '#3b8eea',
  brightMagenta: '#d670d6',
  brightCyan: '#29b8db',
  brightWhite: '#e5e5e5',
};

const LIGHT: ITheme = {
  background: '#fafafa',
  foreground: '#383a42',
  cursor: '#383a42',
  cursorAccent: '#fafafa',
  selectionBackground: '#d4dae6',
  black: '#383a42',
  red: '#e45649',
  green: '#50a14f',
  yellow: '#c18301',
  blue: '#0184bc',
  magenta: '#a626a4',
  cyan: '#0997b3',
  white: '#a0a1a7',
  brightBlack: '#696c77',
  brightRed: '#e45649',
  brightGreen: '#50a14f',
  brightYellow: '#c18301',
  brightBlue: '#0184bc',
  brightMagenta: '#a626a4',
  brightCyan: '#0997b3',
  brightWhite: '#fafafa',
};

export type TermScheme = 'auto' | 'dark' | 'light';
export interface TermPrefs {
  scheme: TermScheme;
  fontSize: number;
}
const PREFS_EVENT = 'ah:term-prefs';
const mobile = () => window.matchMedia('(max-width: 760px)').matches;

export function termPrefs(): TermPrefs {
  return { scheme: 'auto', fontSize: mobile() ? 12 : 13, ...lsGet<Partial<TermPrefs>>('term-prefs', {}) };
}

export function setTermPrefs(p: Partial<TermPrefs>) {
  lsSet('term-prefs', { ...termPrefs(), ...p });
  window.dispatchEvent(new Event(PREFS_EVENT));
}

export function useTermPrefs() {
  const [p, setP] = useState(termPrefs);
  useEffect(() => {
    const f = () => setP(termPrefs());
    window.addEventListener(PREFS_EVENT, f);
    return () => window.removeEventListener(PREFS_EVENT, f);
  }, []);
  return p;
}

/** auto follows the UI theme (which may in turn follow the system) */
function isDark(scheme: TermScheme) {
  return scheme === 'dark' || (scheme === 'auto' && isAppDark());
}

// Phones have no Ctrl/Esc/arrow keys, so add a row of shortcut keys
const KEYS: [string, string][] = [
  ['Esc', '\x1b'],
  ['Tab', '\t'],
  ['Shift-Tab', '\x1b[Z'],
  ['Ctrl-C', '\x03'],
  ['Ctrl-D', '\x04'],
  ['Ctrl-Z', '\x1a'],
  ['Ctrl-L', '\x0c'],
  ['↑', '\x1b[A'],
  ['↓', '\x1b[B'],
  ['←', '\x1b[D'],
  ['→', '\x1b[C'],
  ['Enter', '\r'],
];

/** Terminal color scheme and font size buttons, on the right of the status bar under the terminal */
export function TermTools() {
  const prefs = useTermPrefs();
  useTheme();
  const dark = isDark(prefs.scheme);
  return (
    <div className="term-tools">
      <button title={dark ? tr('换成亮色', 'Switch to light theme') : tr('换成暗色', 'Switch to dark theme')} onClick={() => setTermPrefs({ scheme: dark ? 'light' : 'dark' })}>
        {dark ? tr('☀ 亮色', '☀ Light') : tr('☾ 暗色', '☾ Dark')}
      </button>
      <button title={tr('缩小字号', 'Decrease font size')} onClick={() => setTermPrefs({ fontSize: Math.max(10, prefs.fontSize - 1) })}>
        A−
      </button>
      <button title={tr('放大字号', 'Increase font size')} onClick={() => setTermPrefs({ fontSize: Math.min(24, prefs.fontSize + 1) })}>
        A+
      </button>
    </div>
  );
}

export function TerminalView({ hostId, sessionId, running }: { hostId: string; sessionId: string; running: boolean }) {
  const el = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const prefs = useTermPrefs();
  useTheme();
  const dark = isDark(prefs.scheme);
  const fitRef = useRef<() => void>(() => {});

  // Sessions in tmux scroll tmux's own history. position is the number of lines from the bottom, history is the total history lines
  const [scroll, setScroll] = useState<{ inTmux: boolean; position: number; history: number }>({ inTmux: false, position: 0, history: 0 });
  const scrollRef = useRef(scroll);
  scrollRef.current = scroll;

  // Each call is handled concurrently on the backend, so sending key by key would reorder them: only one request in flight at a time, and actions meanwhile are batched into the next.
  // Scrolling and input share one queue: typing while scrolled back first exits scroll mode, then sends the input
  type Op = { kind: 'input'; data: string } | { kind: 'scroll'; lines: number } | { kind: 'cancel' };
  const queue = useRef<{ ops: Op[]; inflight: boolean }>({ ops: [], inflight: false });
  const enqueue = (op: Op) => {
    const q = queue.current;
    const last = q.ops[q.ops.length - 1];
    if (last?.kind === 'input' && op.kind === 'input') last.data += op.data;
    else if (last?.kind === 'scroll' && op.kind === 'scroll') last.lines += op.lines;
    else q.ops.push(op);
    if (q.inflight) return;
    const flush = () => {
      const next = q.ops.shift();
      if (!next) {
        q.inflight = false;
        return;
      }
      q.inflight = true;
      const req =
        next.kind === 'input'
          ? rpc(hostId, 'pty.input', { sessionId, data: toB64(next.data) })
          : rpc<{ inTmux: boolean; position?: number; history?: number }>(hostId, 'pty.scroll', {
              sessionId,
              lines: next.kind === 'scroll' ? next.lines : 0,
              cancel: next.kind === 'cancel',
            }).then((r) => {
              if (!r.inTmux) return;
              // Scrolled back to the bottom (tmux exits copy mode automatically)
              if (!r.position) paging.current = false;
              setScroll({ inTmux: true, position: r.position ?? 0, history: r.history ?? 0 });
            });
      req.catch(() => {}).finally(flush);
    };
    flush();
  };
  // The query on mount often comes before the tmux session exists (the program waits for the first pty.resize before starting); after a "not in tmux" answer we'd never ask again,
  // and the scrollbar would never appear: until tmux is confirmed, ask again every 3 s, at most 20 times. After that, history growth is driven by output
  useEffect(() => {
    let n = 0;
    const ask = () => {
      if (scrollRef.current.inTmux || ++n > 20) return clearInterval(t);
      if (document.visibilityState !== 'visible') return;
      const q = queue.current;
      if (!q.inflight && !q.ops.length) enqueue({ kind: 'scroll', lines: 0 });
    };
    const t = setInterval(ask, 3000);
    return () => clearInterval(t);
  }, [hostId, sessionId]); // eslint-disable-line react-hooks/exhaustive-deps
  // Sending a scroll-up request counts as entering scroll mode without waiting for the backend; otherwise typing while the request is in flight wouldn't exit scroll mode first
  const paging = useRef(false);
  const send = (data: string) => {
    if (paging.current || scrollRef.current.position > 0) {
      paging.current = false;
      setScroll((x) => ({ ...x, position: 0 }));
      enqueue({ kind: 'cancel' });
    }
    enqueue({ kind: 'input', data });
  };
  const scrollBy = (lines: number) => {
    if (!lines || !scrollRef.current.inTmux) return;
    if (lines > 0) paging.current = true;
    enqueue({ kind: 'scroll', lines });
  };

  useEffect(() => {
    const p = termPrefs();
    const term = new Terminal({
      // Like Windows Terminal, prefer Cascadia Mono, with CJK falling back to YaHei / PingFang
      fontFamily:
        '"Cascadia Mono", "Cascadia Code", Consolas, "SF Mono", Menlo, "JetBrains Mono", "Microsoft YaHei UI", "PingFang SC", monospace',
      fontSize: p.fontSize,
      lineHeight: 1.2,
      cursorBlink: true,
      cursorStyle: 'bar',
      cursorWidth: 2,
      scrollback: 10000,
      theme: isDark(p.scheme) ? DARK : LIGHT,
      allowProposedApi: true,
      macOptionIsMeta: true,
      rightClickSelectsWord: false,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    // CJK and emoji count as two cells wide so the cursor doesn't drift
    term.loadAddon(new Unicode11Addon());
    term.unicode.activeVersion = '11';
    term.open(el.current!);
    // GPU rendering: crisper text and smoother scrolling; falls back to the default DOM renderer when unsupported
    try {
      const gl = new WebglAddon();
      gl.onContextLoss(() => gl.dispose());
      term.loadAddon(gl);
    } catch {
      /* WebGL not supported */
    }
    termRef.current = term;
    const stopIme = imeFollowFakeCursor(term);
    const key = skey(hostId, sessionId);
    const d = getSession(key);
    // When replaying past output, terminal queries the program sent back then (cursor position, terminal type, etc.) must not be answered again:
    // the answers would be typed into the shell as keyboard input (e.g. tmux's ESC[>c turning into 0;276;0c after the prompt)
    let replaying = 0;
    const swallow = () => replaying > 0;
    const hooks = [
      term.parser.registerCsiHandler({ final: 'c' }, swallow),
      term.parser.registerCsiHandler({ prefix: '>', final: 'c' }, swallow),
      term.parser.registerCsiHandler({ prefix: '>', final: 'q' }, swallow),
      term.parser.registerCsiHandler({ final: 'n' }, swallow),
      term.parser.registerCsiHandler({ prefix: '?', final: 'n' }, swallow),
      term.parser.registerCsiHandler({ intermediates: '$', final: 'p' }, swallow),
      term.parser.registerCsiHandler({ prefix: '?', intermediates: '$', final: 'p' }, swallow),
    ];
    const replay = (data: Uint8Array) => {
      replaying++;
      term.write(data, () => replaying--);
    };
    for (const chunk of d.termBacklog) replay(chunk);
    d.termBacklog = [];
    // Opened before: the old terminal content was destroyed with the component, so resubscribe to get a screen snapshot
    if (d.loaded) loadSession(hostId, sessionId, true).catch(() => {});
    d.term = (data, reset) => {
      if (reset) {
        term.reset();
        replay(data);
      } else term.write(data);
    };

    let lastSize = '';
    let resizeTimer: ReturnType<typeof setTimeout> | undefined;
    const doFit = () => {
      const buf = term.buffer.active;
      const atBottom = buf.viewportY >= buf.baseY;
      const top = buf.viewportY;
      try {
        fit.fit();
      } catch {
        return;
      }
      // When the size changes xterm parks the viewport at the top: go back to the bottom if we were there, otherwise back to the original line
      requestAnimationFrame(() => (atBottom ? term.scrollToBottom() : term.scrollToLine(top)));
      const size = `${term.cols}x${term.rows}`;
      if (size === lastSize) return;
      const first = !lastSize;
      lastSize = size;
      // Send the first one immediately (yi-agentd waits for this size before starting the program); after that wait until the size settles, so full-screen programs don't redraw dozens of times while dragging the window
      clearTimeout(resizeTimer);
      const push = () => rpc(hostId, 'pty.resize', { sessionId, cols: term.cols, rows: term.rows }).catch(() => {});
      if (first) push();
      else resizeTimer = setTimeout(push, 200);
    };
    fitRef.current = doFit;
    // Like Windows Terminal: Ctrl+C copies when there's a selection, Ctrl+Shift+C / V copy and paste, Ctrl+V pastes
    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== 'keydown') return true;
      const k = e.key.toLowerCase();
      if (e.shiftKey && (e.key === 'PageUp' || e.key === 'PageDown') && scrollRef.current.inTmux) {
        scrollBy((e.key === 'PageUp' ? 1 : -1) * Math.max(1, term.rows - 2));
        return false;
      }
      if ((e.ctrlKey || e.metaKey) && k === 'c' && (e.shiftKey || term.hasSelection())) {
        const t = term.getSelection();
        if (t) writeClipboard(t);
        term.clearSelection();
        return false;
      }
      if ((e.ctrlKey || e.metaKey) && k === 'v') {
        e.preventDefault();
        readClipboard(false).then((t) => t && term.paste(t));
        return false;
      }
      return true;
    });
    // Wheel: sessions in tmux scroll tmux's history; accumulate pixels and send once per full line
    let wheelAcc = 0;
    const cellHeight = () => (term.element?.querySelector('.xterm-screen')?.clientHeight ?? 0) / term.rows || 18;
    term.attachCustomWheelEventHandler((e) => {
      if (!scrollRef.current.inTmux || term.buffer.active.type === 'alternate') return true;
      const cell = cellHeight();
      wheelAcc += e.deltaMode === 1 ? e.deltaY * cell : e.deltaY;
      const lines = Math.trunc(wheelAcc / cell);
      if (lines) {
        wheelAcc -= lines * cell;
        scrollBy(-lines);
      }
      e.preventDefault();
      return false;
    });
    // Touch swipes on mobile: also scroll tmux history in tmux (swipe down for older lines, swipe up to return to the bottom)
    let touchY: number | null = null;
    let touchAcc = 0;
    const boxEl = el.current!;
    const onTouchStart = (e: TouchEvent) => {
      if (e.touches.length === 1 && scrollRef.current.inTmux && term.buffer.active.type !== 'alternate') {
        touchY = e.touches[0].clientY;
        touchAcc = 0;
      } else {
        touchY = null;
      }
    };
    const onTouchMove = (e: TouchEvent) => {
      if (touchY === null || e.touches.length !== 1 || !scrollRef.current.inTmux || term.buffer.active.type === 'alternate') return;
      const y = e.touches[0].clientY;
      const dy = y - touchY;
      touchY = y;
      const cell = cellHeight();
      touchAcc += dy;
      const lines = Math.trunc(touchAcc / cell);
      if (lines) {
        touchAcc -= lines * cell;
        if (lines > 0 || scrollRef.current.position > 0) {
          scrollBy(lines);
          if (e.cancelable) e.preventDefault();
        }
      }
    };
    const onTouchEnd = () => {
      touchY = null;
      touchAcc = 0;
    };
    boxEl.addEventListener('touchstart', onTouchStart, { passive: true });
    boxEl.addEventListener('touchmove', onTouchMove, { passive: false });
    boxEl.addEventListener('touchend', onTouchEnd, { passive: true });
    boxEl.addEventListener('touchcancel', onTouchEnd, { passive: true });
    enqueue({ kind: 'scroll', lines: 0 });
    let histTimer: ReturnType<typeof setTimeout> | undefined;
    const outSub = term.onWriteParsed(() => {
      if (!scrollRef.current.inTmux || scrollRef.current.position > 0) return;
      clearTimeout(histTimer);
      histTimer = setTimeout(() => enqueue({ kind: 'scroll', lines: 0 }), 1500);
    });
    const ro = new ResizeObserver(() => doFit());
    ro.observe(el.current!);
    setTimeout(doFit, 30);
    const sub = term.onData((data) => send(data));
    const offTheme = onThemeChange(() => (term.options.theme = isDark(termPrefs().scheme) ? DARK : LIGHT));
    term.focus();
    return () => {
      sub.dispose();
      outSub.dispose();
      clearTimeout(histTimer);
      clearTimeout(resizeTimer);
      boxEl.removeEventListener('touchstart', onTouchStart);
      boxEl.removeEventListener('touchmove', onTouchMove);
      boxEl.removeEventListener('touchend', onTouchEnd);
      boxEl.removeEventListener('touchcancel', onTouchEnd);
      stopIme();
      hooks.forEach((h) => h.dispose());
      ro.disconnect();
      offTheme();
      if (d.term) d.term = undefined;
      term.dispose();
      termRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId, sessionId]);

  useEffect(() => {
    if (scroll.inTmux && termRef.current) termRef.current.options.scrollback = 0;
  }, [scroll.inTmux]);

  // Settings changed: switch color scheme and font size
  useEffect(() => {
    const t = termRef.current;
    if (!t) return;
    t.options.theme = dark ? DARK : LIGHT;
    if (t.options.fontSize !== prefs.fontSize) {
      t.options.fontSize = prefs.fontSize;
      fitRef.current();
    }
  }, [dark, prefs.fontSize]);

  const copy = () => {
    const t = termRef.current?.getSelection();
    if (t) writeClipboard(t);
  };
  const paste = async () => {
    const t = await readClipboard(false);
    if (t) termRef.current?.paste(t);
    termRef.current?.focus();
  };

  return (
    <div className={`term-wrap ${dark ? 'term-dark' : 'term-light'}`}>
      <div
        className="term"
        ref={el}
        onClick={() => termRef.current?.focus()}
        onContextMenu={(e) =>
          openMenu(e, [
            { label: tr('复制', 'Copy'), icon: 'copy', disabled: !termRef.current?.hasSelection(), run: copy },
            { label: tr('粘贴', 'Paste'), icon: 'paste', run: paste },
            '-',
            { label: tr('全选', 'Select All'), run: () => termRef.current?.selectAll() },
            { label: tr('清屏', 'Clear'), icon: 'trash', run: () => termRef.current?.clear() },
          ])
        }
      />
      {scroll.inTmux && scroll.history > 0 && (
        <TmuxScrollbar
          position={scroll.position}
          history={scroll.history}
          rows={termRef.current?.rows ?? 24}
          onScroll={scrollBy}
          onBottom={() => {
            paging.current = false;
            setScroll((x) => ({ ...x, position: 0 }));
            enqueue({ kind: 'cancel' });
            termRef.current?.focus();
          }}
        />
      )}
      {running && (
        <div className="term-keys">
          {KEYS.map(([label, seq]) => (
            <button
              key={label}
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => {
                send(seq);
                termRef.current?.focus();
              }}
            >
              {label}
            </button>
          ))}
          <button
            onMouseDown={(e) => e.preventDefault()}
            onClick={() => {
              paste();
            }}
          >
            {tr('粘贴', 'Paste')}
          </button>
        </div>
      )}
    </div>
  );
}

/**
 * Full-screen TUIs (Kiro, Claude Code, etc.) hide the real cursor in a corner and draw their own with an inverted cell.
 * xterm positions the hidden IME textarea at the real cursor, so the IME candidate window ends up in the corner too.
 * While the real cursor is hidden, move the textarea and the composition preview onto that lone inverted cell on screen; if none is found, leave them alone.
 */
function imeFollowFakeCursor(term: Terminal): () => void {
  const root = term.element;
  const ta = term.textarea;
  if (!root || !ta) return () => {};
  const comp = root.querySelector<HTMLElement>('.composition-view');
  const screen = root.querySelector<HTMLElement>('.xterm-screen');

  /** Search bottom-up for a single inverted cell whose left and right neighbors aren't inverted */
  const find = () => {
    const buf = term.buffer.active;
    const tmp = buf.getNullCell();
    for (let y = term.rows - 1; y >= 0; y--) {
      const line = buf.getLine(buf.viewportY + y);
      if (!line) continue;
      for (let x = 0; x < term.cols; x++) {
        if (!line.getCell(x, tmp)?.isInverse()) continue;
        const prev = x > 0 && line.getCell(x - 1)?.isInverse();
        const next = line.getCell(x + 1)?.isInverse();
        if (!prev && !next) return { x, y };
      }
    }
    return null;
  };

  const apply = () => {
    if (document.activeElement !== ta || !screen) return;
    // Private field; if unavailable, treat the cursor as hidden (nothing moves if no inverted cell is found anyway)
    const hidden = (term as any)._core?.coreService?.isCursorHidden;
    if (hidden === false) return;
    const p = find();
    if (!p) return;
    const left = `${(p.x * screen.clientWidth) / term.cols}px`;
    const top = `${(p.y * screen.clientHeight) / term.rows}px`;
    for (const el of [ta, comp]) {
      if (el && (el.style.left !== left || el.style.top !== top)) {
        el.style.left = left;
        el.style.top = top;
      }
    }
  };

  // xterm resets the position to the real cursor on every render and composition, so move it back afterwards; skip if the position we set is unchanged
  const mo = new MutationObserver(apply);
  mo.observe(ta, { attributes: true, attributeFilter: ['style'] });
  if (comp) mo.observe(comp, { attributes: true, attributeFilter: ['style'] });
  const r = term.onRender(apply);
  ta.addEventListener('focus', apply);
  ta.addEventListener('compositionstart', apply);
  return () => {
    mo.disconnect();
    r.dispose();
    ta.removeEventListener('focus', apply);
    ta.removeEventListener('compositionstart', apply);
  };
}

/** Scrollbar for tmux history: drag the thumb, click the track to page; shows "back to bottom" at the bottom right while scrolled back */
function TmuxScrollbar({
  position,
  history,
  rows,
  onScroll,
  onBottom,
}: {
  position: number;
  history: number;
  rows: number;
  onScroll: (lines: number) => void;
  onBottom: () => void;
}) {
  const track = useRef<HTMLDivElement>(null);
  const total = history + rows;
  const thumbH = Math.max(24, (rows / total) * 100);
  // Which line the top of the thumb is at: the very top is the first history line
  const topLine = history - position;
  const thumbTop = (topLine / total) * 100;
  const drag = useRef<{ y: number; pos: number } | null>(null);
  const linesPerPx = () => total / (track.current?.clientHeight || 1);

  return (
    <>
      <div
        className="tmux-scroll"
        ref={track}
        onPointerDown={(e) => {
          if (e.target !== track.current) return;
          // Clicking the track: page one screen toward the click
          const r = track.current!.getBoundingClientRect();
          const clickLine = ((e.clientY - r.top) / r.height) * total;
          onScroll((clickLine < topLine ? 1 : -1) * Math.max(1, rows - 2));
        }}
      >
        <div
          className="tmux-thumb"
          style={{ top: `${thumbTop}%`, height: `${Math.min(100, thumbH)}%` }}
          onPointerDown={(e) => {
            e.preventDefault();
            e.stopPropagation();
            drag.current = { y: e.clientY, pos: position };
            let sent = position;
            const move = (ev: PointerEvent) => {
              const d = drag.current;
              if (!d) return;
              const want = Math.max(0, Math.min(history, Math.round(d.pos - (ev.clientY - d.y) * linesPerPx())));
              if (want !== sent) {
                onScroll(want - sent);
                sent = want;
              }
            };
            const up = () => {
              drag.current = null;
              window.removeEventListener('pointermove', move);
              window.removeEventListener('pointerup', up);
              window.removeEventListener('pointercancel', up);
            };
            window.addEventListener('pointermove', move);
            window.addEventListener('pointerup', up);
            window.addEventListener('pointercancel', up);
          }}
        />
      </div>
      {position > 0 && (
        <button className="tmux-bottom" onClick={onBottom} title={tr('回到底部（直接打字也会回到底部）', 'Jump to bottom (typing also returns to bottom)')}>
          {tr(`↓ 回到底部 · 上翻 ${position} 行`, `↓ Bottom · +${position} lines`)}
        </button>
      )}
    </>
  );
}
