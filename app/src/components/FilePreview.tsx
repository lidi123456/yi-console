import { useDeferredValue, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { tr } from '../i18n';
import { marked, type Token, type TokensList } from 'marked';
import DOMPurify from 'dompurify';
import { rpc } from '../transport';
import { Icon } from './Icon';
import { openImage } from './Lightbox';

// 文件面板里的 PDF、CSV、Markdown 预览

/** 按 RFC 4180 解析：引号里可以有分隔符、换行，"" 表示一个引号 */
export function parseCsv(text: string, sep: string, maxRows: number): { rows: string[][]; more: boolean } {
  const rows: string[][] = [];
  let row: string[] = [];
  let cell = '';
  let q = false;
  let i = 0;
  if (text.charCodeAt(0) === 0xfeff) i = 1;
  for (; i < text.length; i++) {
    const c = text[i];
    if (q) {
      if (c === '"') {
        if (text[i + 1] === '"') {
          cell += '"';
          i++;
        } else q = false;
      } else cell += c;
    } else if (c === '"' && cell === '') q = true;
    else if (c === sep) {
      row.push(cell);
      cell = '';
    } else if (c === '\n' || c === '\r') {
      if (c === '\r' && text[i + 1] === '\n') i++;
      row.push(cell);
      rows.push(row);
      row = [];
      cell = '';
      if (rows.length > maxRows) return { rows: rows.slice(0, maxRows), more: true };
    } else cell += c;
  }
  if (cell !== '' || row.length) {
    row.push(cell);
    rows.push(row);
  }
  return { rows, more: false };
}

const MAX_ROWS = 5000;

export function CsvView({ text, tsv }: { text: string; tsv: boolean }) {
  const { rows, more } = useMemo(() => parseCsv(text, tsv ? '\t' : ',', MAX_ROWS), [text, tsv]);
  const [header, setHeader] = useState(true);
  const [q, setQ] = useState('');
  const head = header ? rows[0] ?? [] : [];
  const body = header ? rows.slice(1) : rows;
  const cols = Math.max(0, ...rows.map((r) => r.length));
  const ql = q.trim().toLowerCase();
  const shown = ql ? body.filter((r) => r.some((c) => c.toLowerCase().includes(ql))) : body;
  return (
    <div className="csv">
      <div className="csv-bar">
        <span className="muted small">
          {tr(
            `${body.length}${more ? '+' : ''} 行 · ${cols} 列${ql ? ` · 筛出 ${shown.length} 行` : ''}`,
            `${body.length}${more ? '+' : ''} rows · ${cols} cols${ql ? ` · ${shown.length} shown` : ''}`,
          )}
        </span>
        <label className="csv-opt small">
          <input type="checkbox" checked={header} onChange={(e) => setHeader(e.target.checked)} /> {tr('第一行是表头', 'First row is a header')}
        </label>
        <div className="grow" />
        <input className="csv-filter" value={q} placeholder={tr('筛选行…', 'Filter rows…')} onChange={(e) => setQ(e.target.value)} />
      </div>
      <div className="csv-scroll">
        <table>
          {head.length > 0 && (
            <thead>
              <tr>
                <th className="csv-n" />
                {Array.from({ length: cols }, (_, i) => (
                  <th key={i} title={head[i]}>
                    {head[i] ?? ''}
                  </th>
                ))}
              </tr>
            </thead>
          )}
          <tbody>
            {shown.map((r, i) => (
              <tr key={i}>
                <td className="csv-n">{i + 1}</td>
                {Array.from({ length: cols }, (_, k) => (
                  <td key={k} title={r[k]}>
                    {r[k] ?? ''}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
        {more && <div className="muted small csv-more">{tr(`只显示前 ${MAX_ROWS} 行，完整内容切到「源码」看。`, `Showing the first ${MAX_ROWS} rows. Switch to Source for the rest.`)}</div>}
      </div>
    </div>
  );
}

/** PDF 用浏览器内核自带的阅读器（WebView2 / Chromium 有；不支持的系统显示下载按钮） */
export function PdfView({ base64, onDownload }: { base64: string; onDownload: () => void }) {
  const url = useMemo(() => {
    const bin = atob(base64);
    const buf = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) buf[i] = bin.charCodeAt(i);
    return URL.createObjectURL(new Blob([buf], { type: 'application/pdf' }));
  }, [base64]);
  useEffect(() => () => URL.revokeObjectURL(url), [url]);
  const supported = typeof navigator !== 'undefined' && (navigator as Navigator & { pdfViewerEnabled?: boolean }).pdfViewerEnabled !== false;
  return supported ? (
    <iframe className="wp-pdf" src={url} title={tr('PDF 预览', 'PDF preview')} />
  ) : (
    <div className="wp-empty">
      <div className="wp-empty-title">{tr('这个系统上不能直接预览 PDF', 'PDF preview is not available on this system')}</div>
      <button className="btn small" onClick={onDownload}>
        {tr('下载到本机', 'Download')}
      </button>
    </div>
  );
}

// ---------- Markdown ----------

const IMG_MIME: Record<string, string> = {
  png: 'image/png',
  jpg: 'image/jpeg',
  jpeg: 'image/jpeg',
  gif: 'image/gif',
  webp: 'image/webp',
  svg: 'image/svg+xml',
  bmp: 'image/bmp',
  ico: 'image/x-icon',
};

/** 按顶层块渲染，每块前面插一个带源码行号的空锚点：分栏时据此对齐滚动，按大纲跳到标题 */
export function renderMdLines(src: string): string {
  const tokens = marked.lexer(src);
  let line = 1;
  let html = '';
  for (const t of tokens) {
    if (t.type !== 'space') {
      const sub = [t] as Token[] as TokensList;
      sub.links = tokens.links;
      html += `<a class="md-ln" data-line="${line}"></a>` + (marked.parser(sub) as string);
    }
    line += (t.raw.match(/\n/g) ?? []).length;
  }
  // 相对路径的图片先不让浏览器去加载，由组件经 yi-agentd 读出来再填上
  return DOMPurify.sanitize(html).replace(/<img([^>]*?) src="(?![a-zA-Z][a-zA-Z0-9+.-]*:|\/\/|#)([^"]*)"/g, '<img$1 data-src="$2"');
}

const isAbs = (p: string) => /^([a-zA-Z]:)?[\\/]/.test(p);

/** 相对 dir 解析路径，处理 . 和 ..；沿用 dir 的分隔符 */
export function resolvePath(dir: string, rel: string): string {
  const sep = dir.includes('\\') && !dir.includes('/') ? '\\' : '/';
  const parts = (isAbs(rel) ? rel : `${dir}/${rel}`).split(/[\\/]+/);
  const out: string[] = [];
  for (const x of parts) {
    if (x === '.' || (x === '' && out.length)) continue;
    if (x === '..') {
      if (out.length > 1) out.pop();
    } else out.push(x);
  }
  return out.join(sep) || sep;
}

const slug = (s: string) =>
  s
    .trim()
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s_-]/gu, '')
    .replace(/\s+/g, '-');

type Anchor = { line: number; top: number };

/** 源码行（line + frac）和预览里纵向位置的互相换算，按相邻两个锚点线性插值 */
function lineToTop(as: Anchor[], L: number, end: number): number {
  if (!as.length) return 0;
  let i = 0;
  while (i + 1 < as.length && as[i + 1].line <= L) i++;
  const a = as[i];
  const b = as[i + 1] ?? { line: Number.MAX_SAFE_INTEGER, top: end };
  if (L <= a.line) return a.top * (L / Math.max(1, a.line));
  const span = b.line === Number.MAX_SAFE_INTEGER ? 1 : b.line - a.line;
  return a.top + Math.min(1, (L - a.line) / span) * (b.top - a.top);
}

function topToLine(as: Anchor[], y: number): number {
  if (!as.length) return 1;
  let i = 0;
  while (i + 1 < as.length && as[i + 1].top <= y) i++;
  const a = as[i];
  const b = as[i + 1];
  if (!b || b.top <= a.top) return a.line;
  return a.line + ((y - a.top) / (b.top - a.top)) * (b.line - a.line);
}

export function MdPreview({
  hostId,
  path,
  text,
  onOpen,
  onTopLine,
  syncTo,
  goto,
}: {
  hostId: string;
  /** 这个 Markdown 文件的绝对路径，相对链接和图片按它所在目录解析 */
  path: string;
  text: string;
  /** 点了指向工作区文件的相对链接 */
  onOpen: (abs: string) => void;
  /** 预览滚动时报告对应的源码行（分栏时编辑器跟着滚） */
  onTopLine?: (line: number, frac: number) => void;
  /** 编辑器滚到哪一行，预览跟着对齐；n 变了就再滚 */
  syncTo?: { line: number; frac: number; n: number } | null;
  /** 跳到某一行所在的块（大纲） */
  goto?: { line: number; n: number } | null;
}) {
  const box = useRef<HTMLDivElement>(null);
  const src = useDeferredValue(text);
  const html = useMemo(() => renderMdLines(src), [src]);
  const imgs = useRef(new Map<string, Promise<string>>());
  const anchors = useRef<Anchor[]>([]);
  const muted = useRef(false);
  const dir = path.slice(0, Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\')));

  const measure = () => {
    const el = box.current;
    if (!el) return;
    const base = el.getBoundingClientRect().top - el.scrollTop;
    anchors.current = [...el.querySelectorAll<HTMLElement>('.md-ln')].map((a) => ({
      line: Number(a.dataset.line),
      top: a.getBoundingClientRect().top - base,
    }));
  };

  useLayoutEffect(() => {
    const el = box.current;
    if (!el) return;
    measure();
    for (const img of el.querySelectorAll<HTMLImageElement>('img[data-src]')) {
      const raw = img.dataset.src ?? '';
      let rel = raw.split(/[?#]/)[0];
      try {
        rel = decodeURI(rel);
      } catch {
        /* 保持原样 */
      }
      const abs = resolvePath(dir, rel);
      const mime = IMG_MIME[rel.slice(rel.lastIndexOf('.') + 1).toLowerCase()];
      if (!mime) continue;
      let p = imgs.current.get(abs);
      if (!p) {
        p = rpc<{ base64: string }>(hostId, 'fs.read', { path: abs, image: true }).then((r) => `data:${mime};base64,${r.base64}`);
        imgs.current.set(abs, p);
      }
      p.then(
        (url) => {
          img.src = url;
        },
        () => {
          img.alt = img.alt || raw;
          img.classList.add('md-img-missing');
          img.title = tr(`读不到图片：${raw}`, `Could not load image: ${raw}`);
        },
      );
      img.addEventListener('load', measure, { once: true });
    }
  }, [html]); // eslint-disable-line react-hooks/exhaustive-deps

  // 宽度变了（拖面板、换行）重新量锚点
  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const scrollToLine = (L: number) => {
    const el = box.current;
    if (!el) return;
    muted.current = true;
    el.scrollTop = lineToTop(anchors.current, L, el.scrollHeight);
    requestAnimationFrame(() => (muted.current = false));
  };
  useEffect(() => {
    if (syncTo) scrollToLine(syncTo.line + syncTo.frac);
  }, [syncTo?.n]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    if (goto) scrollToLine(goto.line);
  }, [goto?.n]); // eslint-disable-line react-hooks/exhaustive-deps

  const onScroll = () => {
    const el = box.current;
    if (!el || muted.current || !onTopLine) return;
    const L = topToLine(anchors.current, el.scrollTop);
    onTopLine(Math.floor(L), L - Math.floor(L));
  };

  const onClick = (e: React.MouseEvent) => {
    const a = (e.target as HTMLElement).closest('a');
    const href = a?.getAttribute('href');
    if (!a || !href) return;
    if (href.startsWith('#')) {
      e.preventDefault();
      const want = decodeURIComponent(href.slice(1));
      const h = [...box.current!.querySelectorAll<HTMLElement>('h1,h2,h3,h4,h5,h6')].find((x) => x.id === want || slug(x.textContent ?? '') === slug(want));
      h?.scrollIntoView({ block: 'start' });
      return;
    }
    if (/^[a-zA-Z][a-zA-Z0-9+.-]*:|^\/\//.test(href)) {
      a.target = '_blank';
      a.rel = 'noreferrer';
      return;
    }
    e.preventDefault();
    let rel = href.split(/[?#]/)[0];
    try {
      rel = decodeURI(rel);
    } catch {
      /* 保持原样 */
    }
    if (rel) onOpen(resolvePath(dir, rel));
  };

  return <div ref={box} className="wp-md md" onScroll={onScroll} onClick={onClick} dangerouslySetInnerHTML={{ __html: html }} />;
}

// ---------- 图片 ----------

const clampScale = (v: number) => Math.min(16, Math.max(0.05, v));

/** 图片预览：适应窗口 / 1:1、缩放（按钮、滚轮、双指）、拖动、左右旋转、水平翻转 */
export function ImageView({ src, name, size }: { src: string; name: string; size: string }) {
  const box = useRef<HTMLDivElement>(null);
  const [nat, setNat] = useState<{ w: number; h: number } | null>(null);
  const [boxSize, setBoxSize] = useState({ w: 0, h: 0 });
  // scale 为 null 表示「适应窗口」，跟着面板大小变
  const [scale, setScale] = useState<number | null>(null);
  const [rot, setRot] = useState(0);
  const [flip, setFlip] = useState(false);
  const [pos, setPos] = useState({ x: 0, y: 0 });
  const pointers = useRef(new Map<number, { x: number; y: number }>());
  const gesture = useRef<{ x: number; y: number; px: number; py: number; dist: number; s: number } | null>(null);

  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setBoxSize({ w: el.clientWidth, h: el.clientHeight }));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const side = rot % 180 !== 0;
  const fitScale = nat && boxSize.w ? Math.min(1, (boxSize.w - 48) / (side ? nat.h : nat.w), (boxSize.h - 48) / (side ? nat.w : nat.h)) : 1;
  const s = scale ?? fitScale;

  /** 以 (cx, cy)（相对容器中心）为中心缩放到 next */
  const zoomTo = (next: number, cx = 0, cy = 0) => {
    const n = clampScale(next);
    const k = n / s;
    setPos((p) => ({ x: cx - (cx - p.x) * k, y: cy - (cy - p.y) * k }));
    setScale(n);
  };
  const fit = () => {
    setScale(null);
    setPos({ x: 0, y: 0 });
  };
  const rotate = (d: number) => {
    setRot((r) => (r + d + 360) % 360);
    if (scale === null) setPos({ x: 0, y: 0 });
  };
  const center = (e: { clientX: number; clientY: number }) => {
    const r = box.current!.getBoundingClientRect();
    return [e.clientX - r.left - r.width / 2, e.clientY - r.top - r.height / 2] as const;
  };

  const onWheel = (e: React.WheelEvent) => {
    const [cx, cy] = center(e);
    zoomTo(s * (e.deltaY < 0 ? 1.15 : 1 / 1.15), cx, cy);
  };
  const onDown = (e: React.PointerEvent) => {
    if ((e.target as HTMLElement).closest('.iv-bar')) return;
    box.current!.setPointerCapture(e.pointerId);
    pointers.current.set(e.pointerId, { x: e.clientX, y: e.clientY });
    const ps = [...pointers.current.values()];
    const mid = ps.length >= 2 ? { x: (ps[0].x + ps[1].x) / 2, y: (ps[0].y + ps[1].y) / 2 } : ps[0];
    const dist = ps.length >= 2 ? Math.hypot(ps[0].x - ps[1].x, ps[0].y - ps[1].y) : 0;
    gesture.current = { x: mid.x, y: mid.y, px: pos.x, py: pos.y, dist, s };
  };
  const onMove = (e: React.PointerEvent) => {
    if (!pointers.current.has(e.pointerId) || !gesture.current) return;
    pointers.current.set(e.pointerId, { x: e.clientX, y: e.clientY });
    const g = gesture.current;
    const ps = [...pointers.current.values()];
    if (ps.length >= 2 && g.dist) {
      const dist = Math.hypot(ps[0].x - ps[1].x, ps[0].y - ps[1].y);
      const n = clampScale((g.s * dist) / g.dist);
      const [cx, cy] = center({ clientX: g.x, clientY: g.y });
      const k = n / g.s;
      setScale(n);
      setPos({ x: cx - (cx - g.px) * k, y: cy - (cy - g.py) * k });
    } else {
      setPos({ x: g.px + e.clientX - g.x, y: g.py + e.clientY - g.y });
    }
  };
  const onUp = (e: React.PointerEvent) => {
    pointers.current.delete(e.pointerId);
    const ps = [...pointers.current.values()];
    gesture.current = ps.length ? { x: ps[0].x, y: ps[0].y, px: pos.x, py: pos.y, dist: 0, s } : null;
  };

  // 快捷键：焦点在图片区域时生效
  const onKey = (e: React.KeyboardEvent) => {
    if (e.key === '+' || e.key === '=') zoomTo(s * 1.25);
    else if (e.key === '-') zoomTo(s / 1.25);
    else if (e.key === '0') fit();
    else if (e.key === '1') zoomTo(1), setPos({ x: 0, y: 0 });
    else if (e.key === 'r') rotate(90);
    else if (e.key === 'R') rotate(-90);
    else return;
    e.preventDefault();
  };

  return (
    <div className="wp-image iv" ref={box} tabIndex={0} onWheel={onWheel} onPointerDown={onDown} onPointerMove={onMove} onPointerUp={onUp} onPointerCancel={onUp} onKeyDown={onKey} onDoubleClick={(e) => {
      if ((e.target as HTMLElement).closest('.iv-bar')) return;
      if (scale === null || Math.abs(s - fitScale) < 0.01) {
        const [cx, cy] = center(e);
        zoomTo(Math.max(1, fitScale * 2), cx, cy);
      } else fit();
    }}>
      <img
        src={src}
        alt={tr(`图片预览：${name}`, `Image preview: ${name}`)}
        draggable={false}
        onLoad={(e) => setNat({ w: e.currentTarget.naturalWidth, h: e.currentTarget.naturalHeight })}
        style={{
          width: nat?.w,
          height: nat?.h,
          visibility: nat ? 'visible' : 'hidden',
          transform: `translate(-50%, -50%) translate(${pos.x}px, ${pos.y}px) rotate(${rot}deg) scale(${flip ? -s : s}, ${s})`,
          imageRendering: s >= 3 ? 'pixelated' : undefined,
        }}
      />
      <div className="iv-bar" onDoubleClick={(e) => e.stopPropagation()}>
        <button title={tr('缩小（-）', 'Zoom out (-)')} onClick={() => zoomTo(s / 1.25)}>
          <Icon name="zoom-out" size={15} />
        </button>
        <button className="iv-pct" title={`${nat ? `${nat.w} × ${nat.h} · ` : ''}${size}\n${tr('点一下切换适应窗口 / 原始大小', 'Click to toggle fit / actual size')}`} onClick={() => (scale === null ? (zoomTo(1), setPos({ x: 0, y: 0 })) : fit())}>
          {Math.round(s * 100)}%
        </button>
        <button title={tr('放大（+）', 'Zoom in (+)')} onClick={() => zoomTo(s * 1.25)}>
          <Icon name="zoom" size={15} />
        </button>
        <span className="iv-sep" />
        <button title={tr('适应窗口（0）', 'Fit (0)')} className={scale === null ? 'on' : ''} onClick={fit}>
          <Icon name="fit" size={15} />
        </button>
        <button title={tr('向左旋转（Shift+R）', 'Rotate left (Shift+R)')} onClick={() => rotate(-90)}>
          <Icon name="rotate-l" size={15} />
        </button>
        <button title={tr('向右旋转（R）', 'Rotate right (R)')} onClick={() => rotate(90)}>
          <Icon name="rotate-r" size={15} />
        </button>
        <button title={tr('水平翻转', 'Flip horizontal')} className={flip ? 'on' : ''} onClick={() => setFlip(!flip)}>
          <Icon name="flip" size={15} />
        </button>
        <button title={tr('全屏查看', 'Full screen')} onClick={() => openImage(src)}>
          <Icon name="maximize" size={14} />
        </button>
        <span className="iv-info">
          {nat ? `${nat.w} × ${nat.h} · ` : ''}
          {size}
        </span>
      </div>
    </div>
  );
}
