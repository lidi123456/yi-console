import { useDeferredValue, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { getT } from '../i18n';
import { marked, type Token, type TokensList } from 'marked';
import DOMPurify from 'dompurify';
import { rpc } from '../transport';
import { Icon } from './Icon';
import { openImage } from './Lightbox';

const t = getT('filePreview');

// PDF, CSV and Markdown previews in the file panel

/** Parse per RFC 4180: quoted fields may contain separators and newlines, "" means one quote */
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
          {t('csvSize', { rows: `${body.length}${more ? '+' : ''}`, cols })}
          {ql ? t('csvShown', { n: shown.length }) : ''}
        </span>
        <label className="csv-opt small">
          <input type="checkbox" checked={header} onChange={(e) => setHeader(e.target.checked)} /> {t('firstRowIsAHeader')}
        </label>
        <div className="grow" />
        <input className="csv-filter" value={q} placeholder={t('filterRows')} onChange={(e) => setQ(e.target.value)} />
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
        {more && <div className="muted small csv-more">{t('showingTheFirstRowsSwitch', { MAX_ROWS })}</div>}
      </div>
    </div>
  );
}

/** PDFs use the browser engine's built-in viewer (WebView2 / Chromium have one; unsupported systems show a download button) */
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
    <iframe className="wp-pdf" src={url} title={t('pdfPreview')} />
  ) : (
    <div className="wp-empty">
      <div className="wp-empty-title">{t('pdfPreviewIsNotAvailable')}</div>
      <button className="btn small" onClick={onDownload}>
        {t('download')}
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

/** Render top-level blocks, inserting an empty anchor with the source line number before each: used to sync scrolling in split view and to jump to headings from the outline */
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
  // Don't let the browser load relative-path images; the component reads them via yi-agentd and fills them in
  return DOMPurify.sanitize(html).replace(/<img([^>]*?) src="(?![a-zA-Z][a-zA-Z0-9+.-]*:|\/\/|#)([^"]*)"/g, '<img$1 data-src="$2"');
}

const isAbs = (p: string) => /^([a-zA-Z]:)?[\\/]/.test(p);

/** Resolve a path relative to dir, handling . and ..; keeps dir's separator */
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

/** Convert between a source line (line + frac) and a vertical position in the preview by linear interpolation between neighboring anchors */
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
  /** Absolute path of this Markdown file; relative links and images resolve against its directory */
  path: string;
  text: string;
  /** A relative link to a workspace file was clicked */
  onOpen: (abs: string) => void;
  /** Report the matching source line as the preview scrolls (the editor follows in split view) */
  onTopLine?: (line: number, frac: number) => void;
  /** Line the editor scrolled to; the preview aligns to it, and scrolls again whenever n changes */
  syncTo?: { line: number; frac: number; n: number } | null;
  /** Jump to the block containing a line (outline) */
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
        /* keep as is */
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
          img.title = t('couldNotLoadImage', { raw });
        },
      );
      img.addEventListener('load', measure, { once: true });
    }
  }, [html]); // eslint-disable-line react-hooks/exhaustive-deps

  // Width changed (panel dragged, rewrapped): re-measure the anchors
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
      /* keep as is */
    }
    if (rel) onOpen(resolvePath(dir, rel));
  };

  return <div ref={box} className="wp-md md" onScroll={onScroll} onClick={onClick} dangerouslySetInnerHTML={{ __html: html }} />;
}

// ---------- Images ----------

const clampScale = (v: number) => Math.min(16, Math.max(0.05, v));

/** Image preview: fit / 1:1, zoom (buttons, wheel, pinch), pan, rotate left/right, flip horizontally */
export function ImageView({ src, name, size }: { src: string; name: string; size: string }) {
  const box = useRef<HTMLDivElement>(null);
  const [nat, setNat] = useState<{ w: number; h: number } | null>(null);
  const [boxSize, setBoxSize] = useState({ w: 0, h: 0 });
  // A null scale means "fit to window" and follows the panel size
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

  /** Zoom to next, centered on (cx, cy) (relative to the container center) */
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

  // Shortcuts: active while focus is in the image area
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
        alt={t('imagePreview', { name })}
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
        <button title={t('zoomOut')} onClick={() => zoomTo(s / 1.25)}>
          <Icon name="zoom-out" size={15} />
        </button>
        <button className="iv-pct" title={`${nat ? `${nat.w} × ${nat.h} · ` : ''}${size}\n${t('clickToToggleFitActual')}`} onClick={() => (scale === null ? (zoomTo(1), setPos({ x: 0, y: 0 })) : fit())}>
          {Math.round(s * 100)}%
        </button>
        <button title={t('zoomIn')} onClick={() => zoomTo(s * 1.25)}>
          <Icon name="zoom" size={15} />
        </button>
        <span className="iv-sep" />
        <button title={t('fit0')} className={scale === null ? 'on' : ''} onClick={fit}>
          <Icon name="fit" size={15} />
        </button>
        <button title={t('rotateLeftShiftR')} onClick={() => rotate(-90)}>
          <Icon name="rotate-l" size={15} />
        </button>
        <button title={t('rotateRightR')} onClick={() => rotate(90)}>
          <Icon name="rotate-r" size={15} />
        </button>
        <button title={t('flipHorizontal')} className={flip ? 'on' : ''} onClick={() => setFlip(!flip)}>
          <Icon name="flip" size={15} />
        </button>
        <button title={t('fullScreen')} onClick={() => openImage(src)}>
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
