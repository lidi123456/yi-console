import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useBack } from '../back';
import { openFileLink } from '../chatFiles';
import type { ImgCtx } from '../chatImages';
import { getT, useLang } from '../i18n';
import { toast } from '../store';
import { cloneCleanTable, copyTableElement, enhanceTables, fitTables } from '../tableEnhance';
import { Icon } from './Icon';

const t = getT('chatView');

function tableDimensions(table: HTMLTableElement): { cols: number; rows: number } {
  const allRows = Array.from(table.rows);
  const headRows = table.tHead ? table.tHead.rows.length : 0;
  const cols = Math.max(1, ...allRows.map((r) => r.cells.length));
  const rows = Math.max(allRows.length > 0 ? 1 : 0, headRows ? allRows.length - headRows : allRows.length);
  return { cols, rows };
}

/** Fullscreen modal viewer for wide or tall Markdown tables, with adaptive full-width column layout and copy action */
export function TableViewer({
  table,
  ctx = null,
  onLinkClick,
  onClose,
}: {
  table: HTMLTableElement;
  ctx?: ImgCtx | null;
  onLinkClick?: (e: React.MouseEvent) => void;
  onClose: () => void;
}) {
  useLang();
  useBack(true, onClose);
  const hostRef = useRef<HTMLDivElement>(null);
  const closeBtn = useRef<HTMLButtonElement>(null);
  const [copied, setCopied] = useState(false);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const { cols, rows } = tableDimensions(table);

  useLayoutEffect(() => {
    const prev = document.activeElement as HTMLElement | null;
    closeBtn.current?.focus();
    return () => {
      if (timerRef.current) clearTimeout(timerRef.current);
      prev?.focus();
    };
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || e.isComposing) return;
      e.preventDefault();
      e.stopImmediatePropagation();
      onClose();
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [onClose]);

  useLayoutEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const cloned = cloneCleanTable(table);
    host.replaceChildren(cloned);
    enhanceTables(host, undefined, false);
    let prevW = host.clientWidth;
    const ro = new ResizeObserver(() => {
      const nextW = host.clientWidth;
      if (!nextW || nextW === prevW) return;
      prevW = nextW;
      fitTables(host);
    });
    ro.observe(host);
    return () => {
      ro.disconnect();
      host.replaceChildren();
    };
  }, [table]);

  const onCopy = async () => {
    const live = hostRef.current?.querySelector<HTMLTableElement>('table') ?? table;
    if (!(await copyTableElement(live))) {
      toast(t('copyFailed'), 'error');
      return;
    }
    setCopied(true);
    if (timerRef.current) clearTimeout(timerRef.current);
    timerRef.current = setTimeout(() => setCopied(false), 1500);
  };

  return createPortal(
    <div
      className="table-viewer-bg"
      role="dialog"
      aria-modal="true"
      aria-label={t('expandTable')}
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
      onKeyDown={(e) => {
        if (e.key !== 'Tab') return;
        const focusables = e.currentTarget.querySelectorAll<HTMLElement>(
          'button:not(:disabled), a[href], a.md-file, code.md-path',
        );
        if (!focusables.length) return;
        const first = focusables[0];
        const last = focusables[focusables.length - 1];
        if (e.shiftKey && document.activeElement === first) {
          e.preventDefault();
          last.focus();
        } else if (!e.shiftKey && document.activeElement === last) {
          e.preventDefault();
          first.focus();
        }
      }}
    >
      <div className="table-viewer-card">
        <div className="table-viewer-head">
          <span className="table-viewer-title">{t('tablePreviewTitle', { cols, rows })}</span>
          <button
            type="button"
            className={`btn small ${copied ? 'copied' : ''}`}
            onClick={() => void onCopy()}
          >
            <Icon name={copied ? 'check' : 'copy'} size={13} />
            <span>{copied ? t('copied') : t('copyTable')}</span>
          </button>
          <button
            ref={closeBtn}
            type="button"
            className="icon-btn"
            title={t('close')}
            aria-label={t('close')}
            onClick={onClose}
          >
            <Icon name="x" size={16} />
          </button>
        </div>
        <div
          ref={hostRef}
          className="table-viewer-body md"
          onClick={(e) => {
            if (ctx && openFileLink(e, ctx)) {
              onClose();
              return;
            }
            if (onLinkClick) {
              onLinkClick(e);
              if (e.defaultPrevented) onClose();
            }
          }}
          onKeyDown={(e) => {
            if (e.key !== 'Enter' && e.key !== ' ') return;
            if (ctx && openFileLink(e, ctx)) onClose();
          }}
        />
      </div>
    </div>,
    document.body,
  );
}
