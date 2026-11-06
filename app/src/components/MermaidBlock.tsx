import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useBack } from '../back';
import { writeClipboard } from '../clipboard';
import { getT, useLang } from '../i18n';
import { diagramPng, renderDiagram, type Diagram } from '../mermaid';
import { toast } from '../store';
import { useTheme } from '../theme';
import { saveImage } from './ImageMenu';
import { Icon } from './Icon';
import { openMenu } from './Overlay';

const t = getT('mermaidBlock');

export function MermaidBlock({ source, closed, streaming }: { source: string; closed: boolean; streaming: boolean }) {
  const [, dark] = useTheme();
  useLang();
  const box = useRef<HTMLDivElement>(null);
  const [font, setFont] = useState<{ family: string; size: number } | null>(null);
  const [result, setResult] = useState<{ source: string; diagram?: Diagram; error?: string }>();
  const [showSource, setShowSource] = useState(false);
  const [full, setFull] = useState(false);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    const read = () => {
      if (!box.current) return;
      const style = getComputedStyle(box.current);
      setFont({ family: style.fontFamily, size: parseFloat(style.fontSize) });
    };
    read();
    window.addEventListener('ah:font', read);
    return () => window.removeEventListener('ah:font', read);
  }, []);
  useEffect(() => {
    const node = box.current;
    if (!node) return;
    const observer = new ResizeObserver(() => node.dispatchEvent(new Event('yi-diagram-resize', { bubbles: true })));
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  useEffect(() => {
    if (!closed || !font) return;
    let active = true;
    setResult(undefined);
    renderDiagram(source, !!dark, font.family, font.size).then(
      (diagram) => active && setResult({ source, diagram }),
      (e) => active && setResult({ source, error: e instanceof Error ? e.message : '' }),
    );
    return () => {
      active = false;
    };
  }, [source, closed, dark, font?.family, font?.size]);
  const diagram = result?.source === source ? result.diagram : undefined;
  const error = result?.source === source ? result.error : undefined;
  const [url, setUrl] = useState('');
  useEffect(() => {
    if (!diagram) {
      setUrl('');
      return;
    }
    const next = URL.createObjectURL(new Blob([diagram.svg], { type: 'image/svg+xml' }));
    setUrl(next);
    return () => URL.revokeObjectURL(next);
  }, [diagram]);
  const exportImage = async (png: boolean) => {
    if (!diagram || busy) return;
    setBusy(true);
    let downloadUrl = '';
    try {
      const blob = png ? await diagramPng(diagram) : new Blob([diagram.svg], { type: 'image/svg+xml' });
      downloadUrl = URL.createObjectURL(blob);
      await saveImage(downloadUrl);
    } catch {
      toast(t('exportFailed'), 'error');
    } finally {
      if (downloadUrl) URL.revokeObjectURL(downloadUrl);
      setBusy(false);
    }
  };
  return (
    <div className="mermaid-block" ref={box} onClick={(e) => e.stopPropagation()}>
      <div className="mermaid-toolbar mermaid-toolbar-inline">
        <div className="mermaid-tabs" role="group" aria-label={t('diagram')}>
          <button
            type="button"
            aria-label={t('showDiagram')}
            title={t('showDiagram')}
            aria-pressed={!showSource}
            onClick={() => setShowSource(false)}
          >
            {t('diagram')}
          </button>
          <button
            type="button"
            aria-label={t('showSource')}
            title={t('showSource')}
            aria-pressed={showSource}
            onClick={() => setShowSource(true)}
          >
            {t('source')}
          </button>
        </div>
        <div className="mermaid-actions">
          <button
            type="button"
            aria-label={t('copySource')}
            title={t('copySource')}
            onClick={async () => toast((await writeClipboard(source)) ? t('copied') : t('copyFailed'))}
          >
            <Icon name="copy" size={14} />
          </button>
          <button
            type="button"
            aria-label={t('fullscreen')}
            title={t('fullscreen')}
            disabled={!diagram || !url}
            onClick={() => setFull(true)}
          >
            <Icon name="maximize" size={14} />
          </button>
          <button
            type="button"
            aria-label={t('exportDiagram')}
            title={t('exportDiagram')}
            aria-haspopup="true"
            disabled={!diagram || busy}
            onClick={(e) => {
              const rect = e.currentTarget.getBoundingClientRect();
              openMenu({ clientX: rect.left, clientY: rect.bottom + 4 }, [
                { label: t('saveSvg'), icon: 'download', run: () => void exportImage(false) },
                { label: t('savePng'), icon: 'download', run: () => void exportImage(true) },
              ]);
            }}
          >
            <Icon name="download" size={14} />
          </button>
        </div>
      </div>
      {!closed && !streaming && <div className="mermaid-status">{t('incomplete')}</div>}
      {/* While the code is still streaming, or rendering, show a spinner instead of the half-written source (the Source tab still has it) */}
      {((!closed && streaming) || (closed && !result)) && !showSource && (
        <div className="mermaid-status busy" role="status">
          <span className="spinner" />
          {closed ? t('loading') : t('waiting')}
        </div>
      )}
      {error !== undefined && (
        <div className="mermaid-status" role="status">
          {error === 'limit' ? t('restricted') : t('failed')}
        </div>
      )}
      {showSource || (!closed && !streaming) || error !== undefined ? (
        <pre>
          <code>{source}</code>
        </pre>
      ) : url && diagram ? (
        <div className="mermaid-scroll">
          <img
            src={url}
            alt={t('diagram')}
            onClick={() => setFull(true)}
            style={{ width: diagram.width, maxWidth: 'none', height: 'auto' }}
          />
        </div>
      ) : null}
      {full && diagram && url && createPortal(<DiagramViewer url={url} diagram={diagram} close={() => setFull(false)} />, document.body)}
    </div>
  );
}

function DiagramViewer({ url, diagram, close }: { url: string; diagram: Diagram; close: () => void }) {
  const view = useRef<HTMLDivElement>(null);
  const closeButton = useRef<HTMLButtonElement>(null);
  const [scale, setScale] = useState(1);
  const [pos, setPos] = useState({ x: 0, y: 0 });
  const pointers = useRef(new Map<number, { x: number; y: number }>());
  const gesture = useRef<{ x: number; y: number; distance: number } | undefined>(undefined);
  const zoom = (factor: number) => setScale((s) => Math.max(0.1, Math.min(8, s * factor)));
  const fit = () => {
    const rect = view.current?.getBoundingClientRect();
    if (rect) setScale(Math.min((rect.width - 40) / diagram.width, (rect.height - 40) / diagram.height, 2));
    setPos({ x: 0, y: 0 });
  };
  useBack(true, close);
  useLayoutEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    fit();
    closeButton.current?.focus();
    const observer = new ResizeObserver(fit);
    if (view.current) observer.observe(view.current);
    return () => {
      observer.disconnect();
      previous?.focus();
    };
  }, []);
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopImmediatePropagation();
        close();
      } else if (e.key === '+' || e.key === '=') {
        e.preventDefault();
        zoom(1.25);
      } else if (e.key === '-') {
        e.preventDefault();
        zoom(0.8);
      } else if (e.key === '0') {
        e.preventDefault();
        fit();
      }
    };
    window.addEventListener('keydown', key, true);
    return () => window.removeEventListener('keydown', key, true);
  });
  const measure = () => {
    const points = [...pointers.current.values()];
    if (!points.length) return undefined;
    return {
      x: points.reduce((v, p) => v + p.x, 0) / points.length,
      y: points.reduce((v, p) => v + p.y, 0) / points.length,
      distance: points.length > 1 ? Math.hypot(points[0].x - points[1].x, points[0].y - points[1].y) : 0,
    };
  };
  return (
    <div
      className="diagram-viewer"
      role="dialog"
      aria-modal="true"
      aria-label={t('fullscreen')}
      onKeyDown={(e) => {
        if (e.key !== 'Tab') return;
        const buttons = e.currentTarget.querySelectorAll<HTMLButtonElement>('button');
        const first = buttons[0],
          last = buttons[buttons.length - 1];
        if (e.shiftKey && document.activeElement === first) {
          e.preventDefault();
          last.focus();
        } else if (!e.shiftKey && document.activeElement === last) {
          e.preventDefault();
          first.focus();
        }
      }}
    >
      <div className="mermaid-toolbar">
        <button type="button" onClick={() => zoom(0.8)}>
          {t('zoomOut')}
        </button>
        <span>{Math.round(scale * 100)}%</span>
        <button type="button" onClick={() => zoom(1.25)}>
          {t('zoomIn')}
        </button>
        <button type="button" onClick={fit}>
          {t('fit')}
        </button>
        <button type="button" ref={closeButton} onClick={close}>
          {t('close')}
        </button>
      </div>
      <div
        className="diagram-viewport"
        ref={view}
        onWheel={(e) => {
          e.preventDefault();
          zoom(e.deltaY < 0 ? 1.1 : 1 / 1.1);
        }}
        onDoubleClick={fit}
        onDragStart={(e) => e.preventDefault()}
        onPointerDown={(e) => {
          // Without this a mouse drag can select the diagram and turn into a
          // native drag of the selection, which cancels the pointer stream.
          if (e.pointerType === 'mouse') {
            if (e.button !== 0) return;
            e.preventDefault();
          }
          e.currentTarget.setPointerCapture(e.pointerId);
          pointers.current.set(e.pointerId, { x: e.clientX, y: e.clientY });
          gesture.current = measure();
        }}
        onPointerMove={(e) => {
          if (!pointers.current.has(e.pointerId)) return;
          pointers.current.set(e.pointerId, { x: e.clientX, y: e.clientY });
          const next = measure(),
            old = gesture.current;
          if (next && old) {
            setPos((p) => ({ x: p.x + next.x - old.x, y: p.y + next.y - old.y }));
            if (next.distance && old.distance) zoom(next.distance / old.distance);
          }
          gesture.current = next;
        }}
        onPointerUp={(e) => {
          pointers.current.delete(e.pointerId);
          gesture.current = measure();
        }}
        onPointerCancel={(e) => {
          pointers.current.delete(e.pointerId);
          gesture.current = measure();
        }}
      >
        <img
          src={url}
          alt={t('diagram')}
          draggable={false}
          style={{ width: diagram.width, height: diagram.height, transform: `translate(${pos.x}px, ${pos.y}px) scale(${scale})` }}
        />
      </div>
    </div>
  );
}
