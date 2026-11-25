import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useBack } from '../back';
import { getT } from '../i18n';
import { KIND_LABEL } from '../outline';
import type { NavLocation, NavReferenceHit, NavReferencesResult } from '../types';
import type { NavCoords } from './EditorCodeNav';
import { FileIcon } from './FilePanel';
import { Icon } from './Icon';

const tx = getT('filePanel');

const baseName = (p: string) => p.slice(p.lastIndexOf('/') + 1);
const dirName = (p: string) => (p.includes('/') ? p.slice(0, p.lastIndexOf('/')) : '');

function kindBadgeClass(kind: string): string {
  if (kind === 'type' || kind === 'interface' || kind === 'struct' || kind === 'enum' || kind === 'class') return 'type';
  if (kind === 'method') return 'method';
  return 'func';
}

function kindLabel(kind: string): string {
  if (kind === 'const') return 'v';
  return KIND_LABEL[kind] ?? (kind[0] || 's');
}

export function DefinitionPicker({
  symbol,
  locations,
  coords,
  onPick,
  onClose,
}: {
  symbol: string;
  locations: NavLocation[];
  coords: NavCoords;
  onPick: (loc: NavLocation) => void;
  onClose: () => void;
}) {
  const boxRef = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const [sel, setSel] = useState(0);
  const [pos, setPos] = useState<{ left: number; top: number }>({ left: coords.left, top: coords.bottom + 6 });

  useBack(true, onClose);

  useEffect(() => {
    setSel(0);
  }, [symbol, locations.length]);

  useLayoutEffect(() => {
    const el = boxRef.current;
    if (!el) return;
    const w = el.offsetWidth || 540;
    const h = el.offsetHeight || 240;
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const left = Math.max(8, Math.min(coords.left, vw - w - 8));
    const fitsBelow = coords.bottom + 6 + h <= vh - 8;
    const top = fitsBelow ? coords.bottom + 6 : Math.max(8, coords.top - h - 6);
    setPos({ left, top });
  }, [coords.left, coords.top, coords.bottom, locations.length]);

  useEffect(() => {
    listRef.current?.querySelector('.cn-pop-row.sel')?.scrollIntoView({ block: 'nearest' });
  }, [sel]);

  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      if (boxRef.current && !boxRef.current.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.isComposing) return;
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        onClose();
      } else if (e.key === 'ArrowDown') {
        e.preventDefault();
        e.stopPropagation();
        setSel((i) => (locations.length ? (i + 1) % locations.length : 0));
      } else if (e.key === 'ArrowUp') {
        e.preventDefault();
        e.stopPropagation();
        setSel((i) => (locations.length ? (i - 1 + locations.length) % locations.length : 0));
      } else if (e.key === 'Enter') {
        const target = locations[sel];
        if (target) {
          e.preventDefault();
          e.stopPropagation();
          onPick(target);
        }
      }
    };
    window.addEventListener('mousedown', onDown, true);
    window.addEventListener('keydown', onKey, true);
    return () => {
      window.removeEventListener('mousedown', onDown, true);
      window.removeEventListener('keydown', onKey, true);
    };
  }, [locations, sel, onPick, onClose]);

  return (
    <div ref={boxRef} className="cn-pop" style={{ left: pos.left, top: pos.top }} role="dialog" aria-label={tx('navDefinitions')}>
      <div className="cn-pop-head">
        <span className="cn-pop-title">{tx('navDefinitions')}</span>
        <span className="cn-sym-chip">{symbol}</span>
        <span className="cn-cnt-pill">{locations.length}</span>
        <span className="cn-pop-hints">{tx('navPickerHint')}</span>
      </div>
      <div className="cn-pop-list" ref={listRef}>
        {locations.map((loc, idx) => {
          const title = loc.container ? `${loc.container}::${loc.symbol}` : loc.symbol;
          return (
            <button
              type="button"
              key={`${loc.absPath}:${loc.line}:${idx}`}
              className={`cn-pop-row ${idx === sel ? 'sel' : ''}`}
              onMouseMove={() => setSel(idx)}
              onClick={() => onPick(loc)}
            >
              <div className="cn-pop-r1">
                {loc.kind === 'file' ? (
                  <FileIcon name={baseName(loc.path)} size={14} />
                ) : (
                  <span className={`cn-kind-badge ${kindBadgeClass(loc.kind)}`}>{kindLabel(loc.kind)}</span>
                )}
                <span className="cn-pop-sym truncate">{title}</span>
                <span className="cn-pop-loc truncate">{`${loc.path}:${loc.line}`}</span>
              </div>
              {loc.preview && <div className="cn-pop-r2">{loc.preview}</div>}
            </button>
          );
        })}
      </div>
    </div>
  );
}

function renderHighlightedLine(line: string, col: number, len: number, markClass: string) {
  if (col < 0 || len <= 0 || col >= line.length) return line || ' ';
  const pre = line.slice(0, col);
  const mid = line.slice(col, col + len);
  const post = line.slice(col + len);
  return (
    <>
      {pre}
      <mark className={markClass}>{mid}</mark>
      {post}
    </>
  );
}

function renderSnippet(hit: NavReferenceHit) {
  const trimmed = hit.text.replace(/^\s+/, '');
  const leading = hit.text.length - trimmed.length;
  const c = hit.col - leading;
  if (c < 0 || c >= trimmed.length) return trimmed;
  const head = trimmed.slice(0, c);
  const headChars = [...head];
  const pre = headChars.length > 18 ? '…' + headChars.slice(-16).join('') : head;
  const mid = trimmed.slice(c, c + hit.len);
  const post = trimmed.slice(c + hit.len);
  return (
    <>
      {pre}
      <mark>{mid}</mark>
      {post}
    </>
  );
}

export function PeekReferences({
  symbol,
  loading,
  result,
  coords,
  containerWidth,
  onJump,
  onOpenInSearch,
  onClose,
}: {
  symbol: string;
  loading: boolean;
  result: NavReferencesResult | null;
  coords: NavCoords;
  containerWidth: number;
  onJump: (hit: NavReferenceHit) => void;
  onOpenInSearch: (symbol: string) => void;
  onClose: () => void;
}) {
  const boxRef = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const [folded, setFolded] = useState<Set<string>>(new Set());
  const [selKey, setSelKey] = useState<string | null>(null);

  useBack(true, onClose);

  const narrow = containerWidth < 520 || (typeof window !== 'undefined' && window.innerWidth < 600);

  const groups = useMemo(() => {
    const map = new Map<string, NavReferenceHit[]>();
    for (const h of result?.hits ?? []) {
      const arr = map.get(h.path);
      if (arr) arr.push(h);
      else map.set(h.path, [h]);
    }
    return [...map.entries()];
  }, [result]);

  const visibleHits = useMemo(
    () => groups.flatMap(([p, items]) => (folded.has(p) ? [] : items)),
    [groups, folded],
  );

  const hitKey = (h: NavReferenceHit) => `${h.path}\n${h.line}\n${h.col}`;

  useEffect(() => {
    setFolded(new Set());
    const first = result?.hits[0];
    setSelKey(first ? `${first.path}\n${first.line}\n${first.col}` : null);
  }, [result]);

  const activeHit = useMemo(() => {
    if (!result?.hits.length) return null;
    if (selKey) {
      const found = result.hits.find((h) => hitKey(h) === selKey);
      if (found) return found;
    }
    return result.hits[0];
  }, [result, selKey]);

  const viewportWidth = typeof window !== 'undefined' ? window.innerWidth : 1000;
  const width = narrow
    ? Math.max(260, Math.min(viewportWidth < 600 ? viewportWidth - 16 : 480, viewportWidth - 16))
    : Math.max(460, Math.min(840, Math.max(460, containerWidth - 24), viewportWidth - 16));
  const maxH = typeof window !== 'undefined' ? Math.min(340, Math.max(200, window.innerHeight - 24)) : 324;

  const [pos, setPos] = useState<{ left: number; top: number }>({ left: 16, top: coords.bottom + 6 });

  useLayoutEffect(() => {
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const h = boxRef.current?.offsetHeight || maxH;
    const left = Math.max(8, Math.min(coords.left - 32, vw - width - 8));
    const fitsBelow = coords.bottom + 6 + h <= vh - 8;
    const top = fitsBelow ? coords.bottom + 6 : Math.max(8, coords.top - h - 6);
    setPos({ left, top });
  }, [coords.left, coords.top, coords.bottom, width, maxH, result?.hits.length, loading]);

  useEffect(() => {
    if (!selKey) return;
    listRef.current?.querySelector(`[data-rk="${CSS.escape(selKey)}"]`)?.scrollIntoView({ block: 'nearest' });
  }, [selKey]);

  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      if (boxRef.current && !boxRef.current.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.isComposing) return;
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        onClose();
      } else if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        if (!visibleHits.length) return;
        e.preventDefault();
        e.stopPropagation();
        const idx = visibleHits.findIndex((h) => hitKey(h) === selKey);
        const delta = e.key === 'ArrowDown' ? 1 : -1;
        const nextIdx = idx < 0 ? 0 : (idx + delta + visibleHits.length) % visibleHits.length;
        setSelKey(hitKey(visibleHits[nextIdx]));
      } else if (e.key === 'Enter') {
        if (!activeHit) return;
        e.preventDefault();
        e.stopPropagation();
        onJump(activeHit);
      }
    };
    window.addEventListener('mousedown', onDown, true);
    window.addEventListener('keydown', onKey, true);
    return () => {
      window.removeEventListener('mousedown', onDown, true);
      window.removeEventListener('keydown', onKey, true);
    };
  }, [visibleHits, selKey, activeHit, onJump, onClose]);

  const summary = result
    ? result.truncated
      ? tx('navRefSummaryTruncated', { hits: result.totalHits, files: result.totalFiles })
      : tx('navRefSummary', { hits: result.totalHits, files: result.totalFiles })
    : tx('navLoadingRefs');

  return (
    <div
      ref={boxRef}
      className={`cn-peek ${narrow ? 'narrow' : ''}`}
      style={{ left: pos.left, top: pos.top, width, maxHeight: maxH }}
      role="dialog"
      aria-label={tx('navReferences')}
    >
      <div className="cn-peek-head">
        <Icon name="search" size={13} />
        <span className="cn-pop-title">{tx('navReferences')}</span>
        <span className="cn-sym-chip">{symbol}</span>
        <span className="cn-peek-sum truncate">{summary}</span>
        <button
          type="button"
          className="cn-peek-btn"
          onClick={() => onOpenInSearch(symbol)}
          title={tx('navOpenInSearch')}
        >
          <Icon name="external" size={11} />
          {!narrow && <span>{tx('navOpenInSearch')}</span>}
        </button>
        <button type="button" className="icon-btn cn-peek-close" title={tx('close')} onClick={onClose}>
          <Icon name="x" size={13} />
        </button>
      </div>
      {loading ? (
        <div className="cn-peek-loading">
          <span className="spinner" /> {tx('navLoadingRefs')}
        </div>
      ) : (
        <div className="cn-peek-body">
          {!narrow && activeHit && (
            <div className="cn-peek-preview" onDoubleClick={() => onJump(activeHit)}>
              <div className="cn-peek-preview-path">
                <FileIcon name={baseName(activeHit.path)} size={13} />
                <span className="truncate">{activeHit.path}</span>
                <span className="cn-peek-preview-ln">{`:${activeHit.line}`}</span>
              </div>
              <div className="cn-peek-preview-code">
                {activeHit.context.map((lineText, i) => {
                  const ln = activeHit.contextStartLine + i;
                  const isHit = ln === activeHit.line;
                  return (
                    <div key={ln} className={`cn-peek-code-row ${isHit ? 'hit' : ''}`}>
                      <span className="cn-peek-code-ln">{ln}</span>
                      <span className="cn-peek-code-txt">
                        {isHit ? renderHighlightedLine(lineText, activeHit.col, activeHit.len, 'cn-peek-mark') : lineText || ' '}
                      </span>
                    </div>
                  );
                })}
              </div>
            </div>
          )}
          <div className="cn-peek-list" ref={listRef}>
            {groups.map(([path, items]) => {
              const closed = folded.has(path);
              return (
                <div key={path} className="cn-peek-group">
                  <button
                    type="button"
                    className="wp-row grep-head cn-peek-file"
                    onClick={() =>
                      setFolded((prev) => {
                        const next = new Set(prev);
                        if (next.has(path)) next.delete(path);
                        else next.add(path);
                        return next;
                      })
                    }
                  >
                    <span className="wp-twist">
                      <span className={`chev ${closed ? '' : 'open'}`}>
                        <Icon name="chevron" size={12} />
                      </span>
                    </span>
                    <FileIcon name={baseName(path)} size={13} />
                    <span className="truncate grep-name">{baseName(path)}</span>
                    <span className="truncate grep-dir">{dirName(path)}</span>
                    <span className="grep-n">{items.length}</span>
                  </button>
                  {!closed &&
                    items.map((it) => {
                      const k = hitKey(it);
                      const isSel = activeHit ? hitKey(activeHit) === k : false;
                      return (
                        <button
                          type="button"
                          key={k}
                          data-rk={k}
                          className={`wp-row grep-hit cn-peek-hit ${isSel ? 'sel' : ''}`}
                          onMouseEnter={() => !narrow && setSelKey(k)}
                          onClick={() => {
                            if (narrow) onJump(it);
                            else setSelKey(k);
                          }}
                          onDoubleClick={() => onJump(it)}
                        >
                          <span className="grep-line">{`:${it.line}`}</span>
                          <span className="truncate grep-text">{renderSnippet(it)}</span>
                        </button>
                      );
                    })}
                </div>
              );
            })}
          </div>
        </div>
      )}
      <div className="cn-peek-foot">{tx('navPeekHint')}</div>
    </div>
  );
}

export function CodeNavNotice({ message }: { message: string | null }) {
  if (!message) return null;
  return (
    <div className="cn-notice" role="status">
      {message}
    </div>
  );
}
