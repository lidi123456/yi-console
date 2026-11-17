import { memo, useMemo, useRef, useLayoutEffect } from 'react';
import { createPortal } from 'react-dom';
import { hydrateImages, type ImgCtx } from '../chatImages';
import { hydrateFileLinks, hydratePaths, openFileLink } from '../chatFiles';
import { writeClipboard, writeRichClipboard } from '../clipboard';
import { getT, useLang } from '../i18n';
import { toast } from '../store';
import { renderDiagramMarkdown } from '../util';
import { MermaidBlock } from './MermaidBlock';

const t = getT('chatView');

const SVG_NS = 'http://www.w3.org/2000/svg';
const COPY_PATH = 'M8 8h12v12H8zM4 16V4h12';
const CHECK_PATH = 'M5 12l5 5L20 7';
const COPIED_MS = 1500;

function makeSvgIcon(d: string): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, 'svg');
  for (const [k, v] of Object.entries({
    width: '14',
    height: '14',
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

function setBtnCopied(btn: HTMLButtonElement, copied: boolean) {
  const idleLabel = btn.dataset.copyKind === 'table' ? t('copyTable') : t('copyCode');
  const label = copied ? t('copied') : idleLabel;
  btn.classList.toggle('copied', copied);
  btn.title = label;
  btn.setAttribute('aria-label', label);
  btn.replaceChildren(makeSvgIcon(copied ? CHECK_PATH : COPY_PATH));
}

function makeCopyBtn(kind: 'code' | 'table', key: string, copied: boolean): HTMLButtonElement {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = kind === 'table' ? 'copy-float-btn table-copy-btn' : 'copy-float-btn';
  btn.dataset.copyKind = kind;
  btn.dataset.copyKey = key;
  setBtnCopied(btn, copied);
  return btn;
}

/** Wrap chat code blocks and Markdown tables with floating top-right copy buttons outside `<pre>` / `<table>` so manual selection stays clean */
function hydrateCopyBlocks(root: HTMLElement, copiedUntil: Map<string, number>) {
  const now = Date.now();
  let codeIdx = 0;
  for (const pre of Array.from(root.querySelectorAll<HTMLPreElement>('pre'))) {
    if (pre.closest('.mermaid-block, table') || pre.parentElement?.classList.contains('md-code-wrap')) continue;
    const key = `code:${codeIdx++}`;
    const wrap = document.createElement('div');
    wrap.className = 'md-code-wrap';
    const anchor = document.createElement('div');
    anchor.className = 'copy-float-anchor';
    anchor.append(makeCopyBtn('code', key, (copiedUntil.get(key) ?? 0) > now));
    pre.replaceWith(wrap);
    wrap.append(anchor, pre);
  }
  let tableIdx = 0;
  for (const table of Array.from(root.querySelectorAll<HTMLTableElement>('table'))) {
    if (table.closest('.mermaid-block') || table.parentElement?.classList.contains('md-table-wrap')) continue;
    const key = `table:${tableIdx++}`;
    const wrap = document.createElement('div');
    wrap.className = 'md-table-wrap';
    const btn = makeCopyBtn('table', key, (copiedUntil.get(key) ?? 0) > now);
    table.replaceWith(wrap);
    wrap.append(table, btn);
  }
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

/** Reconstruct GFM Markdown and clean HTML from a rendered Markdown `<table>` */
function serializeTable(table: HTMLTableElement): { text: string; html: string } {
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
      return `<${cellTag}${align}>${escHtml((cell?.textContent ?? '').trim())}</${cellTag}>`;
    }).join('')}</tr>`;
  const html = `<table><thead>${headRows.map((r) => htmlRow(r, 'th')).join('')}</thead>${bodyRows.length ? `<tbody>${bodyRows.map((r) => htmlRow(r, 'td')).join('')}</tbody>` : ''}</table>`;
  return { text: mdLines.join('\n'), html };
}

export const ChatMarkdown = memo(function ChatMarkdown({
  text,
  streaming = false,
  ctx = null,
}: {
  text: string;
  streaming?: boolean;
  ctx?: ImgCtx | null;
}) {
  const [, lang] = useLang();
  const model = useMemo(() => renderDiagramMarkdown(text), [text]);
  const ref = useRef<HTMLDivElement>(null);
  const previous = useRef<HTMLElement[]>([]);
  const copiedUntil = useRef<Map<string, number>>(new Map());
  const timers = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map());
  const targets = useMemo(() => model.diagrams.map((_, index) => previous.current[index] ?? document.createElement('div')), [model]);
  useLayoutEffect(() => {
    return () => {
      for (const timer of timers.current.values()) clearTimeout(timer);
      timers.current.clear();
    };
  }, []);
  useLayoutEffect(() => {
    const root = ref.current;
    if (!root) return;
    const template = document.createElement('template');
    template.innerHTML = model.html;
    for (const placeholder of template.content.querySelectorAll<HTMLElement>(`[data-diagram="${model.marker}"]`)) {
      const target = targets[Number(placeholder.dataset.index)];
      if (target) placeholder.replaceWith(target);
    }
    // Move existing portal targets into the new tree: completed diagrams keep their React state while text streams.
    root.replaceChildren(template.content);
    previous.current = targets;
    hydrateCopyBlocks(root, copiedUntil.current);
    hydrateImages(root, ctx);
    hydrateFileLinks(root, ctx);
    hydratePaths(root, ctx);
  }, [model, targets, ctx, lang]);

  const onCopyClick = async (btn: HTMLButtonElement) => {
    const kind = btn.dataset.copyKind;
    const key = btn.dataset.copyKey ?? '';
    let ok = false;
    if (kind === 'table') {
      const table = btn.closest('.md-table-wrap')?.querySelector('table');
      if (!table) return;
      const { text: mdText, html } = serializeTable(table);
      if (!mdText) return;
      ok = await writeRichClipboard(mdText, html);
    } else {
      const pre = btn.closest('.md-code-wrap')?.querySelector('pre');
      if (!pre) return;
      const code = pre.querySelector('code') ?? pre;
      const raw = (code.textContent ?? '').replace(/\r?\n$/, '');
      ok = await writeClipboard(raw);
    }
    if (!ok) {
      toast(t('copyFailed'), 'error');
      return;
    }
    copiedUntil.current.set(key, Date.now() + COPIED_MS);
    setBtnCopied(btn, true);
    const old = timers.current.get(key);
    if (old) clearTimeout(old);
    timers.current.set(
      key,
      setTimeout(() => {
        timers.current.delete(key);
        copiedUntil.current.delete(key);
        const live = ref.current?.querySelector<HTMLButtonElement>(`button.copy-float-btn[data-copy-key="${key}"]`);
        if (live) setBtnCopied(live, false);
      }, COPIED_MS),
    );
  };

  return (
    <>
      <div
        ref={ref}
        className="md"
        onClick={(e) => {
          const copyBtn = (e.target as Element | null)?.closest?.<HTMLButtonElement>('button.copy-float-btn');
          if (copyBtn) {
            e.preventDefault();
            e.stopPropagation();
            void onCopyClick(copyBtn);
            return;
          }
          openFileLink(e, ctx);
        }}
        onKeyDown={(e) => {
          if (e.key !== 'Enter' && e.key !== ' ') return;
          if ((e.target as Element | null)?.closest?.('button.copy-float-btn')) return;
          openFileLink(e, ctx);
        }}
      />
      {model.diagrams.map((diagram, index) =>
        createPortal(<MermaidBlock {...diagram} streaming={streaming} />, targets[index], String(index)),
      )}
    </>
  );
});
