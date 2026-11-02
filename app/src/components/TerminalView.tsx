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

// 配色取自 Windows Terminal：暗色 One Half Dark，亮色 One Half Light
const DARK: ITheme = {
  background: '#1e1f22',
  foreground: '#dcdfe4',
  cursor: '#dcdfe4',
  cursorAccent: '#1e1f22',
  selectionBackground: '#3e4451',
  black: '#282c34',
  red: '#e06c75',
  green: '#98c379',
  yellow: '#e5c07b',
  blue: '#61afef',
  magenta: '#c678dd',
  cyan: '#56b6c2',
  white: '#dcdfe4',
  brightBlack: '#5a6374',
  brightRed: '#e06c75',
  brightGreen: '#98c379',
  brightYellow: '#e5c07b',
  brightBlue: '#61afef',
  brightMagenta: '#c678dd',
  brightCyan: '#56b6c2',
  brightWhite: '#ffffff',
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

/** auto 跟随界面主题（界面主题又可以跟随系统） */
function isDark(scheme: TermScheme) {
  return scheme === 'dark' || (scheme === 'auto' && isAppDark());
}

// 手机上没有 Ctrl/Esc/方向键，补一排快捷键
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

/** 终端配色、字号按钮，放在终端下面的状态栏右侧 */
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

  // tmux 里的会话：翻的是 tmux 自己的历史。position 是离底部的行数，history 是历史总行数
  const [scroll, setScroll] = useState<{ inTmux: boolean; position: number; history: number }>({ inTmux: false, position: 0, history: 0 });
  const scrollRef = useRef(scroll);
  scrollRef.current = scroll;

  // 每次调用在后端是并发处理的，逐键发送会乱序：同一时刻只发一个请求，期间的操作攒到下一次。
  // 翻页和输入走同一个队列：翻页中一打字，先退出翻页再发输入
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
              // 翻回了底部（tmux 自动退出翻页）
              if (!r.position) paging.current = false;
              setScroll({ inTmux: true, position: r.position ?? 0, history: r.history ?? 0 });
            });
      req.catch(() => {}).finally(flush);
    };
    flush();
  };
  // 挂载时问的那一次常常早于 tmux 会话建好（程序要等第一个 pty.resize 才启动），回答「不在 tmux」后就再也不问，
  // 滚动条一直不出来：确认在 tmux 之前每 3 秒再问，最多问 20 次。确认之后历史增长由输出触发更新
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
  // 发出往上翻的请求就算进入翻页，不等后端返回：否则请求还在路上时打字，就不会先退出翻页
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
      // 和 Windows Terminal 一样优先用 Cascadia Mono，中文回退到雅黑 / 苹方
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
    // 中文、emoji 按两格宽算，光标不会错位
    term.loadAddon(new Unicode11Addon());
    term.unicode.activeVersion = '11';
    term.open(el.current!);
    // GPU 渲染：字更清晰、滚动更顺；不支持时用默认的 DOM 渲染
    try {
      const gl = new WebglAddon();
      gl.onContextLoss(() => gl.dispose());
      term.loadAddon(gl);
    } catch {
      /* 不支持 WebGL */
    }
    termRef.current = term;
    const stopIme = imeFollowFakeCursor(term);
    const key = skey(hostId, sessionId);
    const d = getSession(key);
    // 回放历史输出时，里面程序当年发的终端查询（光标位置、终端型号等）不能再回答一次：
    // 回答会被当成键盘输入打进 shell（比如 tmux 的 ESC[>c 变成提示符后面的 0;276;0c）
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
    // 之前打开过：上次的终端内容随着组件一起销毁了，重新订阅拿一份屏幕快照
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
      // 尺寸变了 xterm 会把视口停在最上面：原来在底部就回到底部，否则回到原来那行
      requestAnimationFrame(() => (atBottom ? term.scrollToBottom() : term.scrollToLine(top)));
      const size = `${term.cols}x${term.rows}`;
      if (size === lastSize) return;
      const first = !lastSize;
      lastSize = size;
      // 第一次马上发（yi-agentd 等这个尺寸才启动程序）；之后等尺寸停下来再发，拖窗口时全屏程序不用重画几十次
      clearTimeout(resizeTimer);
      const push = () => rpc(hostId, 'pty.resize', { sessionId, cols: term.cols, rows: term.rows }).catch(() => {});
      if (first) push();
      else resizeTimer = setTimeout(push, 200);
    };
    fitRef.current = doFit;
    // 和 Windows Terminal 一样：有选中时 Ctrl+C 复制，Ctrl+Shift+C / V 复制粘贴，Ctrl+V 粘贴
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
    // 滚轮：tmux 里的会话翻 tmux 的历史；按像素累计，满一行发一次
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
    // 手机触摸上下滑：tmux 里同样翻 tmux 历史（手指往下划看更早的行，往上划回到底部）
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

  // 设置改了：换配色、字号
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
 * 全屏 TUI（Kiro、Claude Code 等）会把真实光标藏起来停在角落，自己用反色字符画一个光标。
 * xterm 按真实光标摆放接收输入法的隐藏输入框，输入法候选框就跟着跑到角落去了。
 * 真实光标隐藏时，把输入框和组字预览挪到屏幕上那个单独的反色格子上；找不到就不动。
 */
function imeFollowFakeCursor(term: Terminal): () => void {
  const root = term.element;
  const ta = term.textarea;
  if (!root || !ta) return () => {};
  const comp = root.querySelector<HTMLElement>('.composition-view');
  const screen = root.querySelector<HTMLElement>('.xterm-screen');

  /** 从下往上找左右都不是反色的单个反色格子 */
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
    // 私有字段，取不到时当作隐藏处理（找不到反色格子也不会动）
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

  // xterm 每次渲染、组字时都会按真实光标重设位置，改完再挪回来；自己改的位置相同就不再触发
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

/** tmux 历史的滚动条：拖动滑块、点轨道翻页；翻页中右下角显示"回到底部" */
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
  // 滑块顶部在第几行：最上面是历史第一行
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
          // 点轨道：往点的方向翻一页
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
