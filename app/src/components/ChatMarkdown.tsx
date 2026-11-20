import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { hydrateImages, type ImgCtx } from '../chatImages';
import { hydrateFileLinks, hydratePaths, openFileLink } from '../chatFiles';
import { useLang } from '../i18n';
import { enhanceTables, fitTables, handleBlockActionClick, makeCopyBtn } from '../tableEnhance';
import { renderDiagramMarkdown } from '../util';
import { MermaidBlock } from './MermaidBlock';
import { TableViewer } from './TableViewer';

/** Extract fence language label from `<pre><code class="language-xxx">` */
function codeBlockLang(pre: HTMLPreElement): string {
  const cls = pre.querySelector('code')?.className ?? '';
  const m = /\blanguage-([a-z0-9_+#.-]+)\b/i.exec(cls);
  return m ? m[1].toLowerCase() : '';
}

/** Wrap chat code blocks and Markdown tables with floating top-right actions outside `<pre>` / `<table>` so manual selection stays clean */
function hydrateCopyBlocks(root: HTMLElement, copiedUntil: Map<string, number>) {
  const now = Date.now();
  let codeIdx = 0;
  for (const pre of Array.from(root.querySelectorAll<HTMLPreElement>('pre'))) {
    if (pre.closest('.mermaid-block, table') || pre.parentElement?.classList.contains('md-code-wrap')) continue;
    const key = `code:${codeIdx++}`;
    const lang = codeBlockLang(pre);
    const wrap = document.createElement('div');
    wrap.className = lang ? 'md-code-wrap has-lang' : 'md-code-wrap';
    const anchor = document.createElement('div');
    anchor.className = 'copy-float-anchor';
    anchor.append(makeCopyBtn('code', key, (copiedUntil.get(key) ?? 0) > now));
    pre.replaceWith(wrap);
    if (lang) {
      const head = document.createElement('div');
      head.className = 'md-code-head';
      head.setAttribute('aria-hidden', 'true');
      const tag = document.createElement('span');
      tag.className = 'md-code-lang mono';
      tag.textContent = lang;
      head.append(tag);
      wrap.append(anchor, head, pre);
    } else {
      wrap.append(anchor, pre);
    }
  }
  enhanceTables(root, copiedUntil);
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
  const [expandedTable, setExpandedTable] = useState<HTMLTableElement | null>(null);
  const targets = useMemo(() => model.diagrams.map((_, index) => previous.current[index] ?? document.createElement('div')), [model]);

  useLayoutEffect(() => {
    return () => {
      for (const timer of timers.current.values()) clearTimeout(timer);
      timers.current.clear();
    };
  }, []);

  useEffect(() => {
    const root = ref.current;
    if (!root) return;
    let prevW = root.clientWidth;
    const ro = new ResizeObserver(() => {
      const nextW = root.clientWidth;
      if (!nextW || nextW === prevW) return;
      prevW = nextW;
      fitTables(root);
    });
    ro.observe(root);
    return () => ro.disconnect();
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
    hydrateImages(root, ctx);
    hydrateFileLinks(root, ctx);
    hydratePaths(root, ctx);
    hydrateCopyBlocks(root, copiedUntil.current);
  }, [model, targets, ctx, lang]);

  return (
    <>
      <div
        ref={ref}
        className="md"
        onClick={(e) => {
          if (!handleBlockActionClick(e, ref.current, copiedUntil.current, timers.current, setExpandedTable)) {
            openFileLink(e, ctx);
          }
        }}
        onKeyDown={(e) => {
          if (e.key !== 'Enter' && e.key !== ' ') return;
          if ((e.target as Element | null)?.closest?.('.table-actions button, button.copy-float-btn')) return;
          openFileLink(e, ctx);
        }}
      />
      {model.diagrams.map((diagram, index) =>
        createPortal(<MermaidBlock {...diagram} streaming={streaming} />, targets[index], String(index)),
      )}
      {expandedTable && <TableViewer table={expandedTable} ctx={ctx} onClose={() => setExpandedTable(null)} />}
    </>
  );
});
