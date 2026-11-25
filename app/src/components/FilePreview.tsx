import { useDeferredValue, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { getT, useLang } from '../i18n';
import { marked, type Token, type TokensList } from 'marked';
import DOMPurify from 'dompurify';
import { rpc } from '../transport';
import { enhanceTables, fitTables, handleBlockActionClick } from '../tableEnhance';
import { Icon } from './Icon';
import { openImage } from './Lightbox';
import { TableViewer } from './TableViewer';

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
const CSV_CELL_MIN_W = 48;
const CSV_CELL_MAX_W = 280;
const CSV_CELL_CHROME_W = 24;

let csvMeasureCtx: CanvasRenderingContext2D | null | undefined;
function getCsvMeasureCtx(): CanvasRenderingContext2D | null {
  if (csvMeasureCtx !== undefined) return csvMeasureCtx;
  if (typeof document === 'undefined') {
    csvMeasureCtx = null;
    return null;
  }
  try {
    csvMeasureCtx = document.createElement('canvas').getContext('2d');
  } catch {
    csvMeasureCtx = null;
  }
  return csvMeasureCtx;
}

function approxDisplayUnits(s: string): number {
  let units = 0;
  for (let i = 0; i < s.length; i++) {
    units += s.charCodeAt(i) > 0x7f ? 1.85 : 1;
    if (units >= 44) return units;
  }
  return units;
}

function measureCsvWidths(
  head: string[],
  rows: string[][],
  cols: number,
  fontFamily: string,
): { rowNumW: number; colWidths: number[]; tableWidth: number } {
  const rowNumW = Math.max(40, String(Math.max(rows.length, 1)).length * 8 + 22);
  if (cols <= 0) return { rowNumW, colWidths: [], tableWidth: rowNumW };

  const maxUnits = new Float64Array(cols);
  const widestBody: string[] = new Array(cols).fill('');

  for (let r = 0; r < rows.length; r++) {
    const row = rows[r];
    const n = Math.min(row.length, cols);
    for (let c = 0; c < n; c++) {
      if (maxUnits[c] >= 44) continue;
      const val = row[c];
      if (!val) continue;
      const u = approxDisplayUnits(val);
      if (u > maxUnits[c]) {
        maxUnits[c] = u;
        widestBody[c] = val;
      }
    }
  }

  const ctx = getCsvMeasureCtx();
  const bodyFont = `400 12px ${fontFamily}`;
  const headFont = `600 12px ${fontFamily}`;
  const measureStr = (str: string, bold: boolean): number => {
    if (!str) return 0;
    if (ctx) {
      ctx.font = bold ? headFont : bodyFont;
      return ctx.measureText(str).width;
    }
    return approxDisplayUnits(str) * (bold ? 7.6 : 7.2);
  };

  const colWidths = new Array<number>(cols);
  let tableWidth = rowNumW;
  for (let c = 0; c < cols; c++) {
    const hw = measureStr(head[c] ?? '', true);
    const bw = measureStr(widestBody[c], false);
    const w = Math.min(CSV_CELL_MAX_W, Math.max(CSV_CELL_MIN_W, Math.ceil(Math.max(hw, bw) + CSV_CELL_CHROME_W)));
    colWidths[c] = w;
    tableWidth += w;
  }
  return { rowNumW, colWidths, tableWidth };
}

export function CsvView({ text, tsv }: { text: string; tsv: boolean }) {
  const { rows, more } = useMemo(() => parseCsv(text, tsv ? '\t' : ',', MAX_ROWS), [text, tsv]);
  const [header, setHeader] = useState(true);
  const [q, setQ] = useState('');
  const dq = useDeferredValue(q);
  const head = useMemo(() => (header ? rows[0] ?? [] : []), [header, rows]);
  const body = useMemo(() => (header ? rows.slice(1) : rows), [header, rows]);
  const cols = useMemo(() => Math.max(0, ...rows.map((r) => r.length)), [rows]);
  const ql = dq.trim().toLowerCase();
  const shown = useMemo(
    () => (ql ? body.filter((r) => r.some((c) => c.toLowerCase().includes(ql))) : body),
    [body, ql],
  );
  const fontFamily = useMemo(() => {
    if (typeof window === 'undefined' || typeof document === 'undefined') return 'sans-serif';
    return getComputedStyle(document.body).fontFamily || 'sans-serif';
  }, []);
  const { rowNumW, colWidths, tableWidth } = useMemo(
    () => measureCsvWidths(head, shown, cols, fontFamily),
    [head, shown, cols, fontFamily],
  );
  const scrollRef = useRef<HTMLDivElement>(null);
  const tbodyRef = useRef<HTMLTableSectionElement>(null);
  const [rowH, setRowH] = useState(24);
  const rowHRef = useRef(rowH);
  rowHRef.current = rowH;
  const virtual = shown.length > 120;
  const OVERSCAN = 20;
  const [win, setWin] = useState<[number, number]>([0, 60]);

  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!virtual || !el) return;
    let raf = 0;
    const calc = () => {
      const sample = tbodyRef.current?.querySelector<HTMLElement>('tr:not(.csv-spacer)');
      const h = sample?.offsetHeight || rowHRef.current;
      if (h !== rowHRef.current) setRowH(h);
      const topOffset = tbodyRef.current?.offsetTop ?? 0;
      const viewTop = Math.max(0, el.scrollTop - topOffset);
      const viewBot = Math.max(0, el.scrollTop + el.clientHeight - topOffset);
      const from = Math.max(0, Math.floor(viewTop / h) - OVERSCAN);
      const to = Math.min(shown.length, Math.ceil(viewBot / h) + OVERSCAN);
      setWin((p) => (p[0] === from && p[1] === to ? p : [from, to]));
    };
    const onScroll = () => {
      if (raf) return;
      raf = requestAnimationFrame(() => {
        raf = 0;
        calc();
      });
    };
    calc();
    el.addEventListener('scroll', onScroll, { passive: true });
    const ro = new ResizeObserver(onScroll);
    ro.observe(el);
    return () => {
      el.removeEventListener('scroll', onScroll);
      ro.disconnect();
      if (raf) cancelAnimationFrame(raf);
    };
  }, [virtual, shown.length, header]);

  const from = virtual ? Math.min(win[0], shown.length) : 0;
  const to = virtual ? Math.min(Math.max(win[1], from), shown.length) : shown.length;
  const topPad = from * rowH;
  const botPad = (shown.length - to) * rowH;

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
      <div className="csv-scroll" ref={scrollRef}>
        <table style={cols > 0 ? { tableLayout: 'fixed', width: tableWidth } : undefined}>
          {cols > 0 && (
            <colgroup>
              <col style={{ width: rowNumW }} />
              {colWidths.map((w, i) => (
                <col key={i} style={{ width: w }} />
              ))}
            </colgroup>
          )}
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
          <tbody ref={tbodyRef}>
            {virtual && topPad > 0 && (
              <tr className="csv-spacer" aria-hidden="true">
                <td colSpan={cols + 1} style={{ height: topPad }} />
              </tr>
            )}
            {shown.slice(from, to).map((r, idx) => {
              const i = from + idx;
              return (
                <tr key={i}>
                  <td className="csv-n">{i + 1}</td>
                  {Array.from({ length: cols }, (_, k) => (
                    <td key={k} title={r[k]}>
                      {r[k] ?? ''}
                    </td>
                  ))}
                </tr>
              );
            })}
            {virtual && botPad > 0 && (
              <tr className="csv-spacer" aria-hidden="true">
                <td colSpan={cols + 1} style={{ height: botPad }} />
              </tr>
            )}
          </tbody>
        </table>
        {more && <div className="muted small csv-more">{t('showingTheFirstRowsSwitch', { MAX_ROWS })}</div>}
      </div>
    </div>
  );
}

/** PDFs use the browser engine's built-in viewer (WebView2 / Chromium have one; unsupported systems show a download button) */
export function PdfView({ src, base64, onDownload }: { src?: string; base64?: string; onDownload: () => void }) {
  const supported = typeof navigator !== 'undefined' && (navigator as Navigator & { pdfViewerEnabled?: boolean }).pdfViewerEnabled !== false;
  const [blobUrl, setBlobUrl] = useState<string | null>(null);
  const [err, setErr] = useState(false);

  useEffect(() => {
    setBlobUrl(null);
    setErr(false);
    if (!supported || src || !base64) return;
    let cancelled = false;
    let created: string | null = null;
    fetch(`data:application/pdf;base64,${base64}`)
      .then((r) => r.blob())
      .catch(
        () =>
          new Promise<Blob>((resolve, reject) => {
            window.setTimeout(() => {
              try {
                const bin = atob(base64);
                const buf = new Uint8Array(bin.length);
                for (let i = 0; i < bin.length; i++) buf[i] = bin.charCodeAt(i);
                resolve(new Blob([buf], { type: 'application/pdf' }));
              } catch (e) {
                reject(e);
              }
            }, 0);
          }),
      )
      .then((blob) => {
        if (cancelled) return;
        const u = URL.createObjectURL(blob);
        created = u;
        setBlobUrl(u);
      })
      .catch(() => !cancelled && setErr(true));
    return () => {
      cancelled = true;
      if (created) URL.revokeObjectURL(created);
    };
  }, [src, base64, supported]);

  if (!supported || err) {
    return (
      <div className="wp-empty">
        <div className="wp-empty-title">{t('pdfPreviewIsNotAvailable')}</div>
        <button className="btn small" onClick={onDownload}>
          {t('download')}
        </button>
      </div>
    );
  }
  const url = src || blobUrl;
  if (!url) {
    return (
      <div className="wp-empty">
        <div className="muted small wp-tip-loading" role="status">
          <span className="spinner mini" />
          <span>{t('loadingPdf')}</span>
        </div>
      </div>
    );
  }
  return <iframe className="wp-pdf" src={url} title={t('pdfPreview')} />;
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
  const [, lang] = useLang();
  const box = useRef<HTMLDivElement>(null);
  const src = useDeferredValue(text);
  const html = useMemo(() => renderMdLines(src), [src]);
  const imgs = useRef(new Map<string, Promise<string>>());
  const anchors = useRef<Anchor[]>([]);
  const muted = useRef(false);
  const copiedUntil = useRef<Map<string, number>>(new Map());
  const timers = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map());
  const [expandedTable, setExpandedTable] = useState<HTMLTableElement | null>(null);
  const dir = path.slice(0, Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\')));

  useLayoutEffect(() => {
    return () => {
      for (const timer of timers.current.values()) clearTimeout(timer);
      timers.current.clear();
    };
  }, []);

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
    enhanceTables(el, copiedUntil.current);
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
      img.addEventListener(
        'load',
        () => {
          if (el.querySelector('.md-table-wrap')) fitTables(el);
          measure();
        },
        { once: true },
      );
    }
  }, [html, lang]); // eslint-disable-line react-hooks/exhaustive-deps

  // Width changed (panel dragged, rewrapped): refit tables and re-measure anchors (coalesced during drag)
  useEffect(() => {
    const el = box.current;
    if (!el) return;
    let prevW = el.clientWidth;
    let timer = 0;
    const ro = new ResizeObserver(() => {
      if (timer) window.clearTimeout(timer);
      const delay = document.body.classList.contains('resizing') ? 90 : 32;
      timer = window.setTimeout(() => {
        timer = 0;
        const nextW = el.clientWidth;
        if (nextW && nextW !== prevW) {
          prevW = nextW;
          if (el.querySelector('.md-table-wrap')) fitTables(el);
        }
        measure();
      }, delay);
    });
    ro.observe(el);
    return () => {
      ro.disconnect();
      if (timer) window.clearTimeout(timer);
    };
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

  const onLinkClick = (e: React.MouseEvent) => {
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

  const onClick = (e: React.MouseEvent) => {
    if (handleBlockActionClick(e, box.current, copiedUntil.current, timers.current, setExpandedTable)) return;
    onLinkClick(e);
  };

  return (
    <>
      <div ref={box} className="wp-md md" onScroll={onScroll} onClick={onClick} dangerouslySetInnerHTML={{ __html: html }} />
      {expandedTable && <TableViewer table={expandedTable} onLinkClick={onLinkClick} onClose={() => setExpandedTable(null)} />}
    </>
  );
}

// ---------- Images ----------

const clampScale = (v: number) => Math.min(16, Math.max(0.05, v));

/** Image preview: fit / 1:1, zoom (buttons, wheel, pinch), pan, rotate left/right, flip horizontally */
export function ImageView({ src, name, size }: { src: string; name: string; size: string }) {
  const box = useRef<HTMLDivElement>(null);
  const [nat, setNat] = useState<{ w: number; h: number } | null>(null);
  const [err, setErr] = useState(false);
  const [boxSize, setBoxSize] = useState({ w: 0, h: 0 });
  // A null scale means "fit to window" and follows the panel size
  const [scale, setScale] = useState<number | null>(null);
  const [rot, setRot] = useState(0);
  const [flip, setFlip] = useState(false);
  const [pos, setPos] = useState({ x: 0, y: 0 });
  const pointers = useRef(new Map<number, { x: number; y: number }>());
  const gesture = useRef<{ x: number; y: number; px: number; py: number; dist: number; s: number } | null>(null);

  useEffect(() => {
    setNat(null);
    setErr(false);
  }, [src]);

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
      {!nat && !err && (
        <div className="img-loading" role="status">
          <span className="spinner" />
        </div>
      )}
      {err ? (
        <div className="img-loading" role="alert">
          <span className="muted small">{t('imageLoadFailed')}</span>
        </div>
      ) : (
        <img
          src={src}
          alt={t('imagePreview', { name })}
          draggable={false}
          onLoad={(e) => setNat({ w: e.currentTarget.naturalWidth, h: e.currentTarget.naturalHeight })}
          onError={() => setErr(true)}
          style={{
            width: nat?.w,
            height: nat?.h,
            visibility: nat ? 'visible' : 'hidden',
            transform: `translate(-50%, -50%) translate(${pos.x}px, ${pos.y}px) rotate(${rot}deg) scale(${flip ? -s : s}, ${s})`,
            imageRendering: s >= 3 ? 'pixelated' : undefined,
          }}
        />
      )}
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
