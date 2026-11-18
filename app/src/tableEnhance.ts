import { writeClipboard, writeRichClipboard } from './clipboard';
import { getT } from './i18n';
import { toast } from './store';

const t = getT('chatView');
const COPIED_MS = 1500;

const SVG_NS = 'http://www.w3.org/2000/svg';
export const COPY_PATH = 'M8 8h12v12H8zM4 16V4h12';
export const CHECK_PATH = 'M5 12l5 5L20 7';
export const EXPAND_PATH = 'M14 4h6v6M10 20H4v-6M20 4l-6 6M4 20l6-6';

export function makeSvgIcon(d: string, size = 14): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, 'svg');
  for (const [k, v] of Object.entries({
    width: String(size),
    height: String(size),
    viewBox: '0 0 24 24',
    fill: 'none',
    stroke: 'currentColor',
    'stroke-width': '1.8',
    'stroke-linecap': 'round',
    'stroke-linejoin': 'round',
    'aria-hidden': 'true',
    class: 'icon',
  }))
    svg.setAttribute(k, v);
  const p = document.createElementNS(SVG_NS, 'path');
  p.setAttribute('d', d);
  svg.append(p);
  return svg;
}

export function setCopyBtnState(btn: HTMLButtonElement, copied: boolean) {
  const idleLabel = btn.dataset.copyKind === 'table' ? t('copyTable') : t('copyCode');
  const label = copied ? t('copied') : idleLabel;
  btn.classList.toggle('copied', copied);
  btn.title = label;
  btn.setAttribute('aria-label', label);
  btn.replaceChildren(makeSvgIcon(copied ? CHECK_PATH : COPY_PATH));
}

export function makeCopyBtn(kind: 'code' | 'table', key: string, copied: boolean): HTMLButtonElement {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = kind === 'table' ? 'copy-float-btn table-copy-btn' : 'copy-float-btn';
  btn.dataset.copyKind = kind;
  btn.dataset.copyKey = key;
  setCopyBtnState(btn, copied);
  return btn;
}

function makeExpandBtn(): HTMLButtonElement {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'copy-float-btn table-expand-btn';
  btn.dataset.tableAction = 'expand';
  const label = t('expandTable');
  btn.title = label;
  btn.setAttribute('aria-label', label);
  btn.replaceChildren(makeSvgIcon(EXPAND_PATH));
  return btn;
}

function cellToMarkdown(node: Node): string {
  if (node.nodeType === Node.TEXT_NODE) return node.nodeValue ?? '';
  if (node.nodeType !== Node.ELEMENT_NODE) return '';
  const el = node as HTMLElement;
  const tag = el.tagName.toLowerCase();
  if (tag === 'br') return '<br>';
  if (tag === 'code') return `\`${(el.textContent ?? '').replace(/`/g, '\\`')}\``;
  const inner = Array.from(el.childNodes).map(cellToMarkdown).join('');
  if (tag === 'strong' || tag === 'b') return `**${inner}**`;
  if (tag === 'em' || tag === 'i') return `*${inner}*`;
  if (tag === 'del' || tag === 's') return `~~${inner}~~`;
  if (tag === 'a') {
    const href = el.getAttribute('href') ?? el.dataset.path;
    return href ? `[${inner}](${href})` : inner;
  }
  return inner;
}

function escHtml(s: string): string {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

function cellToHtml(node: Node): string {
  if (node.nodeType === Node.TEXT_NODE) return escHtml(node.nodeValue ?? '');
  if (node.nodeType !== Node.ELEMENT_NODE) return '';
  const el = node as HTMLElement;
  const tag = el.tagName.toLowerCase();
  if (tag === 'svg' || el.classList.contains('md-file-icon')) return '';
  if (tag === 'br') return '<br>';
  if (tag === 'code') return `<code>${escHtml(el.textContent ?? '')}</code>`;
  const inner = Array.from(el.childNodes).map(cellToHtml).join('');
  if (tag === 'strong' || tag === 'b') return `<strong>${inner}</strong>`;
  if (tag === 'em' || tag === 'i') return `<em>${inner}</em>`;
  if (tag === 'del' || tag === 's') return `<del>${inner}</del>`;
  if (tag === 'a') {
    const href = el.getAttribute('href');
    return href ? `<a href="${escHtml(href)}">${inner}</a>` : inner;
  }
  return inner;
}

/** Reconstruct GFM Markdown and clean HTML from a rendered Markdown `<table>` */
export function serializeTable(table: HTMLTableElement): { text: string; html: string } {
  const allRows = Array.from(table.rows);
  if (!allRows.length) return { text: '', html: '' };
  const headRows = table.tHead ? Array.from(table.tHead.rows) : [allRows[0]];
  const headSet = new Set(headRows);
  const bodyRows = allRows.filter((r) => !headSet.has(r));
  const cols = Math.max(1, ...allRows.map((r) => r.cells.length));

  const firstHead = headRows[0];
  const aligns = Array.from({ length: cols }, (_, i) => {
    const c = firstHead?.cells[i] as HTMLElement | undefined;
    const a = (c?.style.textAlign || c?.getAttribute('align') || '').toLowerCase();
    return a === 'center' || a === 'right' || a === 'left' ? a : '';
  });

  const mdRow = (r: HTMLTableRowElement | undefined) =>
    `| ${Array.from({ length: cols }, (_, i) => {
      const cell = r?.cells[i];
      if (!cell) return '';
      return Array.from(cell.childNodes)
        .map(cellToMarkdown)
        .join('')
        .replace(/\r?\n+/g, '<br>')
        .replace(/\|/g, '\\|')
        .trim();
    }).join(' | ')} |`;

  const sepRow = `| ${aligns.map((a) => (a === 'center' ? ':---:' : a === 'right' ? '---:' : a === 'left' ? ':---' : '---')).join(' | ')} |`;
  const mdLines = [mdRow(firstHead), sepRow, ...headRows.slice(1).map(mdRow), ...bodyRows.map(mdRow)];

  const htmlRow = (r: HTMLTableRowElement, cellTag: 'th' | 'td') =>
    `<tr>${Array.from({ length: cols }, (_, i) => {
      const cell = r.cells[i];
      const align = aligns[i] ? ` style="text-align:${aligns[i]}"` : '';
      const inner = cell ? Array.from(cell.childNodes).map(cellToHtml).join('').trim() : '';
      return `<${cellTag}${align}>${inner}</${cellTag}>`;
    }).join('')}</tr>`;
  const html = `<table><thead>${headRows.map((r) => htmlRow(r, 'th')).join('')}</thead>${bodyRows.length ? `<tbody>${bodyRows.map((r) => htmlRow(r, 'td')).join('')}</tbody>` : ''}</table>`;
  return { text: mdLines.join('\n'), html };
}

export async function copyTableElement(table: HTMLTableElement): Promise<boolean> {
  const { text, html } = serializeTable(table);
  if (!text) return false;
  return writeRichClipboard(text, html);
}

/** Handle a click on `.table-expand-btn` or `.copy-float-btn` synchronously so caller link handlers run in the same event turn */
export function handleBlockActionClick(
  e: { target: EventTarget | null; preventDefault(): void; stopPropagation(): void },
  root: HTMLElement | null,
  copiedUntil: Map<string, number>,
  timers: Map<string, ReturnType<typeof setTimeout>>,
  onExpand: (table: HTMLTableElement) => void,
): boolean {
  const target = e.target as Element | null;
  const expandBtn = target?.closest?.<HTMLButtonElement>('button.table-expand-btn');
  if (expandBtn) {
    e.preventDefault();
    e.stopPropagation();
    const table = expandBtn.closest('.md-table-wrap')?.querySelector<HTMLTableElement>('table');
    if (table) onExpand(cloneCleanTable(table));
    return true;
  }
  const copyBtn = target?.closest?.<HTMLButtonElement>('button.copy-float-btn');
  if (!copyBtn) return false;
  e.preventDefault();
  e.stopPropagation();
  const kind = copyBtn.dataset.copyKind;
  const key = copyBtn.dataset.copyKey ?? '';
  let write: Promise<boolean> | null = null;
  if (kind === 'table') {
    const table = copyBtn.closest('.md-table-wrap')?.querySelector<HTMLTableElement>('table');
    if (table) write = copyTableElement(table);
  } else {
    const pre = copyBtn.closest('.md-code-wrap')?.querySelector('pre');
    if (pre) {
      const code = pre.querySelector('code') ?? pre;
      write = writeClipboard((code.textContent ?? '').replace(/\r?\n$/, ''));
    }
  }
  if (!write) return true;
  void write.then((ok) => {
    if (!ok) {
      toast(t('copyFailed'), 'error');
      return;
    }
    copiedUntil.set(key, Date.now() + COPIED_MS);
    setCopyBtnState(copyBtn, true);
    const old = timers.get(key);
    if (old) clearTimeout(old);
    timers.set(
      key,
      setTimeout(() => {
        timers.delete(key);
        copiedUntil.delete(key);
        const live = root?.querySelector<HTMLButtonElement>(`button.copy-float-btn[data-copy-key="${key}"]`);
        if (live) setCopyBtnState(live, false);
      }, COPIED_MS),
    );
  });
  return true;
}

/** Clone a table without transient adaptive sizing so modals or probes start clean */
export function cloneCleanTable(table: HTMLTableElement): HTMLTableElement {
  const clone = table.cloneNode(true) as HTMLTableElement;
  clone.querySelector('colgroup[data-yi-fit]')?.remove();
  clone.style.tableLayout = '';
  clone.style.width = '';
  return clone;
}

/** Sync left/right scroll fade hints on a `.md-table-wrap` */
export function syncTableScrollState(wrap: HTMLElement) {
  const scroll = wrap.querySelector<HTMLElement>('.md-table-scroll');
  if (!scroll) return;
  const maxScroll = scroll.scrollWidth - scroll.clientWidth;
  wrap.classList.toggle('has-scroll-left', maxScroll > 2 && scroll.scrollLeft > 2);
  wrap.classList.toggle('has-scroll-right', maxScroll > 2 && scroll.scrollLeft < maxScroll - 2);
}

function innerContentWidth(el: HTMLElement): number {
  const cs = getComputedStyle(el);
  return el.clientWidth - parseFloat(cs.paddingLeft || '0') - parseFloat(cs.paddingRight || '0');
}

function tagNowrapCode(table: HTMLTableElement) {
  for (const c of table.querySelectorAll('th code, td code')) {
    const txt = (c.textContent ?? '').trim();
    const spaces = txt.match(/\s/g)?.length ?? 0;
    const keepTogether = txt.length > 0 && txt.length <= 22 && !txt.includes(' / ') && spaces <= 1;
    c.classList.toggle('md-code-nowrap', keepTogether);
  }
}

const MAX_WIDTH_CACHE = 64;
const widthCache = new Map<string, number[]>();

function tableCacheKey(table: HTMLTableElement, availW: number): string {
  const cs = getComputedStyle(table);
  let sig = `${availW}|${cs.fontSize}|${cs.fontFamily}|${table.rows.length}`;
  for (const r of Array.from(table.rows)) {
    for (const c of Array.from(r.cells)) {
      const icons = c.querySelectorAll('.md-file-icon, img, .md-rimg').length;
      sig += `|${(c.textContent ?? '').trim()}${icons ? `#${icons}` : ''}`;
    }
  }
  return sig;
}

function applyColWidths(table: HTMLTableElement, widths: number[]) {
  if (!widths.length) return;
  const colgroup = document.createElement('colgroup');
  colgroup.dataset.yiFit = '1';
  for (const w of widths) {
    const col = document.createElement('col');
    col.style.width = `${w}px`;
    colgroup.append(col);
  }
  table.prepend(colgroup);
  table.style.tableLayout = 'fixed';
  table.style.width = `${widths.reduce((a, b) => a + b, 0)}px`;
}

/** Measure per-column natural unwrapped width (`maxLineW`), header width (`headW`), unbreakable min width (`minTokenW`), and max single-cell char length (`maxCellChars`) */
function measureTableColumns(
  table: HTMLTableElement,
  hostRoot: HTMLElement,
): { headW: number[]; maxLineW: number[]; minTokenW: number[]; maxCellChars: number[] } {
  const rows = Array.from(table.rows);
  const cols = Math.max(1, ...rows.map((r) => r.cells.length));
  const headW = new Array<number>(cols).fill(48);
  const maxLineW = new Array<number>(cols).fill(48);
  const minTokenW = new Array<number>(cols).fill(48);
  const maxCellChars = new Array<number>(cols).fill(0);

  for (const r of rows) {
    for (let j = 0; j < cols; j++) {
      maxCellChars[j] = Math.max(maxCellChars[j], (r.cells[j]?.textContent ?? '').trim().length);
    }
  }

  const probeBox = document.createElement('div');
  probeBox.className = 'md-table-probe';
  probeBox.setAttribute('aria-hidden', 'true');
  const probe = cloneCleanTable(table);
  probe.classList.add('yi-probe-nowrap');
  probeBox.append(probe);
  hostRoot.append(probeBox);

  try {
    const pRows = Array.from(probe.rows);
    const hasHead = !!probe.tHead && probe.tHead.rows.length > 0;
    for (let i = 0; i < pRows.length; i++) {
      const cells = pRows[i].cells;
      for (let j = 0; j < cells.length; j++) {
        const w = Math.ceil(cells[j].getBoundingClientRect().width);
        if (i === 0 && hasHead) headW[j] = Math.max(headW[j], w);
        else maxLineW[j] = Math.max(maxLineW[j], w);
      }
    }
    if (!hasHead && pRows[0]) {
      for (let j = 0; j < pRows[0].cells.length; j++) {
        headW[j] = maxLineW[j];
      }
    }

    probe.classList.remove('yi-probe-nowrap');
    probe.classList.add('yi-probe-min');
    // In table-layout: auto without colspan, every cell in column j shares the exact column width, so reading the widest row suffices
    const fullRow = pRows.find((r) => r.cells.length === cols);
    if (fullRow) {
      for (let j = 0; j < cols; j++) {
        minTokenW[j] = Math.max(minTokenW[j], Math.ceil(fullRow.cells[j].getBoundingClientRect().width));
      }
    } else {
      for (const r of pRows) {
        for (let j = 0; j < r.cells.length; j++) {
          minTokenW[j] = Math.max(minTokenW[j], Math.ceil(r.cells[j].getBoundingClientRect().width));
        }
      }
    }
  } finally {
    probeBox.remove();
  }

  for (let j = 0; j < cols; j++) {
    maxLineW[j] = Math.max(maxLineW[j], headW[j]);
  }
  return { headW, maxLineW, minTokenW, maxCellChars };
}

/** Allocate adaptive column widths so words/short-code never break mid-token and dense tables scroll smoothly instead of crushing columns */
export function fitTable(table: HTMLTableElement, availW: number, hostRoot: HTMLElement) {
  if (!table.rows.length || availW <= 40) return;
  tagNowrapCode(table);
  table.querySelector('colgroup[data-yi-fit]')?.remove();
  table.style.tableLayout = '';
  table.style.width = '';

  // Skip fixed colgroup distribution when cells span columns/rows; CSS word-break + scroll wrapper still protects readability
  if (table.querySelector('[colspan], [rowspan]')) return;

  const cacheKey = tableCacheKey(table, availW);
  const cached = widthCache.get(cacheKey);
  if (cached !== undefined) {
    applyColWidths(table, cached);
    return;
  }

  const { headW, maxLineW, minTokenW, maxCellChars } = measureTableColumns(table, hostRoot);
  const cols = maxLineW.length;
  const sumMaxLine = maxLineW.reduce((a, b) => a + b, 0);
  if (sumMaxLine <= availW) {
    if (widthCache.size >= MAX_WIDTH_CACHE) {
      const oldest = widthCache.keys().next().value;
      if (oldest !== undefined) widthCache.delete(oldest);
    }
    widthCache.set(cacheKey, []);
    return;
  }

  const minFloor = maxLineW.map((maxW, j) => {
    const raw = Math.max(Math.min(headW[j], 135), minTokenW[j], Math.min(maxW, 84));
    return Math.min(raw, Math.max(maxW, minTokenW[j]));
  });

  const proseFloor = Math.min(215, Math.max(165, Math.floor(availW * 0.45)));
  const minRead = maxLineW.map((maxW, j) => {
    if (maxCellChars[j] >= 55 && maxW > proseFloor) return Math.max(minFloor[j], proseFloor);
    if (maxW > 140) return Math.max(minFloor[j], Math.min(maxW, 150));
    return minFloor[j];
  });
  const sumRead = minRead.reduce((a, b) => a + b, 0);

  let widths: number[];
  if (sumRead >= availW) {
    widths = minRead;
  } else {
    const ideal = maxLineW.map((maxW, j) => Math.max(minRead[j], Math.min(maxW, maxCellChars[j] >= 65 ? 340 : 230)));
    const sumIdeal = ideal.reduce((a, b) => a + b, 0);
    if (sumIdeal <= availW) {
      const extra = availW - sumIdeal;
      const room = maxLineW.map((maxW, j) => Math.max(0, maxW - ideal[j]));
      const sumRoom = room.reduce((a, b) => a + b, 0);
      widths = ideal.map((w, j) => (sumRoom > 0 ? Math.round(w + extra * (room[j] / sumRoom)) : w));
    } else {
      const tRatio = (availW - sumRead) / Math.max(1, sumIdeal - sumRead);
      widths = minRead.map((w, j) => Math.round(w + tRatio * (ideal[j] - w)));
    }
    // Keep rounding sum within availW so a 1-2px overflow doesn't spawn an unintended horizontal scrollbar
    let drift = widths.reduce((a, b) => a + b, 0) - availW;
    if (drift > 0) {
      let widest = 0;
      for (let j = 1; j < cols; j++) {
        if (widths[j] > widths[widest]) widest = j;
      }
      widths[widest] = Math.max(minFloor[widest], widths[widest] - drift);
    }
  }

  if (widthCache.size >= MAX_WIDTH_CACHE) {
    const oldest = widthCache.keys().next().value;
    if (oldest !== undefined) widthCache.delete(oldest);
  }
  widthCache.set(cacheKey, widths);
  applyColWidths(table, widths);
}

/** Wrap tables in `.md-table-wrap > .md-table-scroll` with floating Expand + Copy actions and fit column widths */
export function enhanceTables(root: HTMLElement, copiedUntil?: Map<string, number>, withActions = true) {
  const now = Date.now();
  let tableIdx = 0;
  for (const table of Array.from(root.querySelectorAll<HTMLTableElement>('table'))) {
    if (table.closest('.mermaid-block') || table.parentElement?.closest('table')) continue;
    let wrap = table.closest<HTMLElement>('.md-table-wrap');
    const key = `table:${tableIdx++}`;
    if (!wrap) {
      wrap = document.createElement('div');
      wrap.className = 'md-table-wrap';
      const scroll = document.createElement('div');
      scroll.className = 'md-table-scroll';
      scroll.addEventListener('scroll', () => syncTableScrollState(wrap!), { passive: true });
      table.replaceWith(wrap);
      scroll.append(table);
      wrap.append(scroll);
      if (withActions) {
        const anchor = document.createElement('div');
        anchor.className = 'copy-float-anchor';
        const actions = document.createElement('div');
        actions.className = 'table-actions';
        actions.append(makeExpandBtn(), makeCopyBtn('table', key, (copiedUntil?.get(key) ?? 0) > now));
        anchor.append(actions);
        wrap.prepend(anchor);
      }
    } else if (withActions) {
      const expandBtn = wrap.querySelector<HTMLButtonElement>('.table-expand-btn');
      if (expandBtn) {
        const label = t('expandTable');
        expandBtn.title = label;
        expandBtn.setAttribute('aria-label', label);
      }
      const copyBtn = wrap.querySelector<HTMLButtonElement>('.table-copy-btn');
      if (copyBtn) {
        copyBtn.dataset.copyKey = key;
        setCopyBtnState(copyBtn, (copiedUntil?.get(key) ?? 0) > now);
      }
    }
  }
  fitTables(root);
}

/** Recompute adaptive widths and scroll hints for every enhanced table in `root` */
export function fitTables(root: HTMLElement) {
  for (const wrap of Array.from(root.querySelectorAll<HTMLElement>('.md-table-wrap'))) {
    const table = wrap.querySelector<HTMLTableElement>('.md-table-scroll > table');
    if (!table) continue;
    const container = wrap.parentElement ?? root;
    const availW = Math.floor(innerContentWidth(container) - 2);
    fitTable(table, availW, root);
    syncTableScrollState(wrap);
  }
}
