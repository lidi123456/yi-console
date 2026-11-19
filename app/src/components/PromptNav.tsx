import { useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useBack } from '../back';
import { writeClipboard } from '../clipboard';
import { parseContextMessage, type ContextRef } from '../contextRefs';
import { getT, useLang } from '../i18n';
import { toast } from '../store';
import type { UserMessageSummary } from '../types';
import { clock } from '../util';
import { ContextRefBar } from './ContextRefBar';
import { ENGINE_ORIGIN } from './EngineNote';
import { Icon } from './Icon';

const t = getT('promptNav');

const MAX_RAIL_TICKS = 40;

export interface PromptEntry {
  /** 1-based display index across the session's user prompts */
  index: number;
  seq: number;
  ts: number;
  rawText: string;
  displayText: string;
  singleLine: string;
  refs: ContextRef[];
  images: number;
  loaded: boolean;
}

/** Build normalized prompt outline entries from stored user-message summaries. */
export function buildPromptEntries(
  userMessages: readonly UserMessageSummary[],
  loadedUserSeqs: ReadonlySet<number>,
  hideCompactCmd: boolean,
  fallbackLabels: { images: (n: number) => string; refs: (n: number) => string; empty: string },
): PromptEntry[] {
  const out: PromptEntry[] = [];
  for (const m of userMessages) {
    if (m.origin === ENGINE_ORIGIN) continue;
    if (hideCompactCmd && m.text.trim() === '/compact') continue;
    const parsed = parseContextMessage(m.text);
    const displayText = parsed.text.trim();
    const collapsed = displayText.replace(/\s+/g, ' ').trim();
    const images = m.images ?? 0;
    const singleLine =
      collapsed ||
      (images > 0
        ? fallbackLabels.images(images)
        : parsed.refs.length > 0
          ? fallbackLabels.refs(parsed.refs.length)
          : fallbackLabels.empty);
    out.push({
      index: out.length + 1,
      seq: m.seq,
      ts: m.ts,
      rawText: m.text,
      displayText,
      singleLine,
      refs: parsed.refs,
      images,
      loaded: loadedUserSeqs.has(m.seq),
    });
  }
  return out;
}

function resolveActiveIndex(prompts: readonly PromptEntry[], activeSeq: number | null): number {
  if (!prompts.length) return -1;
  if (activeSeq != null) {
    const idx = prompts.findIndex((p) => p.seq === activeSeq);
    if (idx >= 0) return idx;
  }
  return prompts.length - 1;
}

function sampleTicks(prompts: readonly PromptEntry[], activeIdx: number): PromptEntry[] {
  if (prompts.length <= MAX_RAIL_TICKS) return [...prompts];
  const indices = new Set<number>([0, prompts.length - 1]);
  if (activeIdx >= 0 && activeIdx < prompts.length) indices.add(activeIdx);
  const steps = MAX_RAIL_TICKS - 1;
  for (let i = 0; i <= steps; i++) {
    indices.add(Math.round((i * (prompts.length - 1)) / steps));
  }
  return [...indices].sort((a, b) => a - b).map((i) => prompts[i]);
}

function useCopyFlash() {
  const [copiedKey, setCopiedKey] = useState<string | number | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current);
    },
    [],
  );
  const copy = async (key: string | number, text: string) => {
    if (!(await writeClipboard(text))) {
      toast(t('copyFailed'), 'error');
      return;
    }
    setCopiedKey(key);
    if (timer.current) clearTimeout(timer.current);
    timer.current = setTimeout(() => setCopiedKey(null), 1500);
  };
  return { copiedKey, copy };
}

/** Floating bar pinned at the top of the chat viewport when the current prompt bubble scrolls out of view; expands downward in place. */
export function PromptStickyBar({
  prompts,
  activeSeq,
  visible,
  mobile,
  outlineOpen,
  hasRail,
  hostId,
  sessionId,
  cwd,
  onJump,
  onStep,
  onToggleOutline,
  onRefill,
}: {
  prompts: readonly PromptEntry[];
  activeSeq: number | null;
  visible: boolean;
  mobile: boolean;
  outlineOpen: boolean;
  hasRail: boolean;
  hostId: string;
  sessionId: string;
  cwd?: string;
  onJump: (seq: number) => void;
  onStep: (delta: -1 | 1) => void;
  onToggleOutline: () => void;
  onRefill: (rawText: string) => void;
}) {
  useLang();
  const [expanded, setExpanded] = useState(false);
  const cardRef = useRef<HTMLDivElement>(null);
  const { copiedKey, copy } = useCopyFlash();

  const activeIdx = useMemo(() => resolveActiveIndex(prompts, activeSeq), [prompts, activeSeq]);
  const activePrompt = activeIdx >= 0 ? prompts[activeIdx] : undefined;

  useEffect(() => {
    setExpanded(false);
  }, [sessionId, activePrompt?.seq, visible]);

  useEffect(() => {
    if (outlineOpen) setExpanded(false);
  }, [outlineOpen]);

  useBack(expanded, () => setExpanded(false));

  useEffect(() => {
    if (!expanded) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.isComposing || e.keyCode === 229) return;
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        setExpanded(false);
      }
    };
    const onDown = (e: PointerEvent) => {
      if (cardRef.current && !cardRef.current.contains(e.target as Node)) {
        setExpanded(false);
      }
    };
    window.addEventListener('keydown', onKey, true);
    document.addEventListener('pointerdown', onDown);
    return () => {
      window.removeEventListener('keydown', onKey, true);
      document.removeEventListener('pointerdown', onDown);
    };
  }, [expanded]);

  if (!visible || !activePrompt) return null;

  const canPrev = activeIdx > 0;
  const canNext = activeIdx < prompts.length - 1;
  const copyPayload = activePrompt.displayText || activePrompt.rawText;
  const isCopied = copiedKey === activePrompt.seq;

  return (
    <div className={`prompt-sticky-wrap ${hasRail ? '' : 'no-rail'} ${mobile ? 'mobile' : ''}`}>
      <div ref={cardRef} className={`prompt-sticky ${expanded ? 'expanded' : ''} ${outlineOpen ? 'open' : ''}`}>
        <div className="ps-head">
          <button
            type="button"
            className="ps-body"
            aria-expanded={expanded}
            title={expanded ? t('collapsePrompt') : t('expandPrompt')}
            onClick={() => setExpanded((v) => !v)}
          >
            <span className="ps-text">{activePrompt.singleLine}</span>
            {(activePrompt.refs.length > 0 || activePrompt.images > 0) && (
              <span className="ps-meta">
                {activePrompt.refs.length > 0 && (
                  <span className="ps-chip">{t('refsBadge', { count: activePrompt.refs.length })}</span>
                )}
                {activePrompt.images > 0 && (
                  <span className="ps-chip">
                    <Icon name="image" size={11} />
                    <span>{activePrompt.images}</span>
                  </span>
                )}
              </span>
            )}
            <span className="ps-expand-chev">
              <Icon name={expanded ? 'up' : 'down'} size={13} />
            </span>
          </button>
          <div className="ps-actions">
            <span className="ps-idx">
              {mobile
                ? t('indexOfTotalCompact', { index: activePrompt.index, total: prompts.length })
                : t('indexOfTotal', { index: activePrompt.index, total: prompts.length })}
            </span>
            {!mobile && (
              <button
                type="button"
                className="ps-btn ps-btn-secondary"
                title={t('locatePrompt')}
                aria-label={t('locatePrompt')}
                onClick={() => {
                  setExpanded(false);
                  onJump(activePrompt.seq);
                }}
              >
                <Icon name="locate" size={14} />
              </button>
            )}
            <button
              type="button"
              className="ps-btn"
              disabled={!canPrev}
              title={t('prevPrompt')}
              aria-label={t('prevPrompt')}
              onClick={() => onStep(-1)}
            >
              <Icon name="up" size={14} />
            </button>
            <button
              type="button"
              className="ps-btn"
              disabled={!canNext}
              title={t('nextPrompt')}
              aria-label={t('nextPrompt')}
              onClick={() => onStep(1)}
            >
              <Icon name="down" size={14} />
            </button>
            {mobile && (
              <button
                type="button"
                data-outline-trigger="1"
                className={`ps-btn ps-outline-btn ${outlineOpen ? 'active' : ''}`}
                title={t('toggleOutline')}
                aria-label={t('toggleOutline')}
                aria-expanded={outlineOpen}
                onClick={() => {
                  setExpanded(false);
                  onToggleOutline();
                }}
              >
                <Icon name="list" size={14} />
              </button>
            )}
          </div>
        </div>
        {expanded && (
          <div className="ps-expand">
            <div className="ps-expand-text">{activePrompt.displayText || activePrompt.singleLine}</div>
            {activePrompt.refs.length > 0 && (
              <ContextRefBar refs={activePrompt.refs} hostId={hostId} sessionId={sessionId} cwd={cwd} className="ps-expand-refs" />
            )}
            {activePrompt.images > 0 && (
              <div className="ps-expand-imgs">
                <span className="ps-chip">
                  <Icon name="image" size={11} />
                  <span>{t('imagesSummary', { count: activePrompt.images })}</span>
                </span>
              </div>
            )}
            <div className="ps-expand-foot">
              <span className="muted small">{clock(activePrompt.ts)}</span>
              <div className="ps-expand-acts">
                <button
                  type="button"
                  className="ol-nav-btn"
                  title={t('locatePrompt')}
                  onClick={() => {
                    setExpanded(false);
                    onJump(activePrompt.seq);
                  }}
                >
                  <Icon name="locate" size={12} />
                  <span>{t('locateShort')}</span>
                </button>
                <button
                  type="button"
                  className="ol-nav-btn"
                  title={t('copyPrompt')}
                  onClick={() => copy(activePrompt.seq, copyPayload)}
                >
                  <Icon name={isCopied ? 'check' : 'copy'} size={12} />
                  <span>{isCopied ? t('copied') : t('copyShort')}</span>
                </button>
                <button
                  type="button"
                  className="ol-nav-btn"
                  title={t('refillPrompt')}
                  onClick={() => {
                    setExpanded(false);
                    onRefill(activePrompt.rawText);
                  }}
                >
                  <Icon name="reply" size={12} />
                  <span>{t('refillShort')}</span>
                </button>
                <button
                  type="button"
                  className="ol-nav-btn"
                  title={t('collapsePrompt')}
                  onClick={() => setExpanded(false)}
                >
                  <Icon name="up" size={12} />
                  <span>{t('collapseShort')}</span>
                </button>
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

/** Right-edge minimal tick rail showing session prompt length and current position; hover or click opens the outline popover. */
export function PromptTickRail({
  prompts,
  activeSeq,
  lit,
  onHoverEnter,
  onHoverLeave,
  onJump,
}: {
  prompts: readonly PromptEntry[];
  activeSeq: number | null;
  lit: boolean;
  onHoverEnter: () => void;
  onHoverLeave: () => void;
  onJump: (seq: number) => void;
}) {
  useLang();
  const activeIdx = useMemo(() => resolveActiveIndex(prompts, activeSeq), [prompts, activeSeq]);
  const ticks = useMemo(() => sampleTicks(prompts, activeIdx), [prompts, activeIdx]);
  const activeTickSeq = activeIdx >= 0 ? prompts[activeIdx]?.seq : null;

  if (prompts.length < 2) return null;

  return (
    <div
      className={`prompt-rail ${lit ? 'hover-lit' : ''}`}
      onPointerEnter={(e) => {
        if (e.pointerType !== 'touch') onHoverEnter();
      }}
      onMouseLeave={onHoverLeave}
      role="navigation"
      aria-label={t('title')}
    >
      {ticks.map((p) => {
        const active = p.seq === activeTickSeq;
        return (
          <button
            key={p.seq}
            type="button"
            className={`rail-tick ${active ? 'active' : ''} ${!p.loaded ? 'earlier' : ''}`}
            title={t('tickLabel', { index: p.index, text: p.singleLine })}
            aria-label={t('tickLabel', { index: p.index, text: p.singleLine })}
            onClick={() => onJump(p.seq)}
          >
            <span className="rail-bar" />
          </button>
        );
      })}
    </div>
  );
}

function PromptOutlineRow({
  entry,
  active,
  selected,
  jumping,
  copied,
  onSelect,
  onCopy,
  onRefill,
}: {
  entry: PromptEntry;
  active: boolean;
  selected: boolean;
  jumping: boolean;
  copied: boolean;
  onSelect: () => void;
  onCopy: () => void;
  onRefill: () => void;
}) {
  return (
    <div
      role="button"
      tabIndex={0}
      data-seq={entry.seq}
      className={`ol-item ${active ? 'active' : ''} ${selected ? 'hovered' : ''} ${!entry.loaded ? 'earlier' : ''}`}
      onClick={onSelect}
      onKeyDown={(e) => {
        if (e.target !== e.currentTarget) return;
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          onSelect();
        }
      }}
    >
      <span className="ol-num">{t('indexTag', { index: entry.index })}</span>
      <div className="ol-main">
        <div className="ol-text">{entry.singleLine}</div>
        <div className="ol-meta">
          {active && <span className="ol-cur-pill">{t('current')}</span>}
          {jumping && <span className="spinner ol-spin" />}
          <span>{clock(entry.ts)}</span>
          {entry.refs.length > 0 && <span className="ps-chip">{t('refsBadge', { count: entry.refs.length })}</span>}
          {entry.images > 0 && (
            <span className="ps-chip">
              <Icon name="image" size={10} />
              <span>{entry.images}</span>
            </span>
          )}
        </div>
      </div>
      <div className="ol-row-acts">
        <button
          type="button"
          className={`ol-act-btn ${copied ? 'copied' : ''}`}
          title={copied ? t('copied') : t('copyPrompt')}
          aria-label={copied ? t('copied') : t('copyPrompt')}
          onClick={(e) => {
            e.stopPropagation();
            onCopy();
          }}
        >
          <Icon name={copied ? 'check' : 'copy'} size={12} />
        </button>
        <button
          type="button"
          className="ol-act-btn"
          title={t('refillPrompt')}
          aria-label={t('refillPrompt')}
          onClick={(e) => {
            e.stopPropagation();
            onRefill();
          }}
        >
          <Icon name="reply" size={12} />
        </button>
      </div>
    </div>
  );
}

/** Shared prompt outline card body used in desktop floating popover, docked right sidebar, and mobile Bottom Sheet. */
export function PromptOutlinePanel({
  mode,
  prompts,
  activeSeq,
  jumpingSeq,
  fullIndexed,
  hasMore,
  loadingIndex,
  loadingOlder,
  pinned,
  onJump,
  onStep,
  onRefill,
  onTogglePin,
  onClose,
  onLoadOlder,
  onSearchFocusChange,
}: {
  mode: 'popover' | 'dock' | 'sheet';
  prompts: readonly PromptEntry[];
  activeSeq: number | null;
  jumpingSeq: number | null;
  fullIndexed: boolean;
  hasMore: boolean;
  loadingIndex: boolean;
  loadingOlder: boolean;
  pinned: boolean;
  onJump: (seq: number) => void;
  onStep: (delta: -1 | 1) => void;
  onRefill: (rawText: string) => void;
  onTogglePin?: () => void;
  onClose: () => void;
  onLoadOlder?: () => void;
  onSearchFocusChange?: (focused: boolean) => void;
}) {
  useLang();
  const [query, setQuery] = useState('');
  const [selIdx, setSelIdx] = useState(-1);
  const listRef = useRef<HTMLDivElement>(null);
  const didAutoScroll = useRef(false);
  const { copiedKey, copy } = useCopyFlash();

  const activeIdx = useMemo(() => resolveActiveIndex(prompts, activeSeq), [prompts, activeSeq]);
  const activePromptSeq = activeIdx >= 0 ? prompts[activeIdx]?.seq ?? null : null;

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return prompts;
    return prompts.filter((p) => {
      if (p.displayText.toLowerCase().includes(q) || p.singleLine.toLowerCase().includes(q)) return true;
      if (`#${p.index}`.includes(q) || String(p.index) === q) return true;
      return p.refs.some((r) => r.path.toLowerCase().includes(q));
    });
  }, [prompts, query]);

  useEffect(() => {
    setSelIdx(-1);
  }, [query]);

  // Center the active item in the list when the outline first opens.
  useEffect(() => {
    if (didAutoScroll.current || !listRef.current || activePromptSeq == null) return;
    didAutoScroll.current = true;
    const el = listRef.current.querySelector(`[data-seq="${activePromptSeq}"]`);
    el?.scrollIntoView({ block: 'nearest' });
  }, [activePromptSeq]);

  useEffect(() => {
    if (selIdx < 0 || !listRef.current) return;
    const target = filtered[selIdx];
    if (!target) return;
    listRef.current.querySelector(`[data-seq="${target.seq}"]`)?.scrollIntoView({ block: 'nearest' });
  }, [selIdx, filtered]);

  const trimmed = query.trim();
  const firstLoadedIdx = !trimmed ? filtered.findIndex((p) => p.loaded) : -1;
  const hasSplit = !trimmed && firstLoadedIdx > 0;
  const earlierItems = hasSplit ? filtered.slice(0, firstLoadedIdx) : [];
  const recentItems = hasSplit ? filtered.slice(firstLoadedIdx) : filtered;

  const pick = (p: PromptEntry) => {
    onJump(p.seq);
    if (mode === 'sheet') onClose();
  };

  const renderRow = (p: PromptEntry, idxInFiltered: number) => (
    <PromptOutlineRow
      key={p.seq}
      entry={p}
      active={p.seq === activePromptSeq}
      selected={idxInFiltered === selIdx}
      jumping={jumpingSeq === p.seq}
      copied={copiedKey === p.seq}
      onSelect={() => pick(p)}
      onCopy={() => copy(p.seq, p.displayText || p.rawText)}
      onRefill={() => {
        onRefill(p.rawText);
        if (mode === 'sheet') onClose();
      }}
    />
  );

  return (
    <>
      <div className="ol-head">
        <span className="ol-title">{t('title')}</span>
        <span className="ol-count">{prompts.length}</span>
        {fullIndexed && prompts.length > 0 && <span className="ol-full-tag">{t('fullSession')}</span>}
        <div className="ol-head-acts">
          {onTogglePin && mode !== 'sheet' && (
            <button
              type="button"
              className={`ps-btn ${pinned ? 'active' : ''}`}
              title={pinned ? t('unpinDock') : t('pinDock')}
              aria-label={pinned ? t('unpinDock') : t('pinDock')}
              onClick={onTogglePin}
            >
              <Icon name="pin" size={14} />
            </button>
          )}
          <button type="button" className="ps-btn" title={t('close')} aria-label={t('close')} onClick={onClose}>
            <Icon name="x" size={14} />
          </button>
        </div>
      </div>

      <div className="ol-search-wrap">
        <div className="ol-search">
          <Icon name="search" size={13} />
          <input
            type="text"
            value={query}
            placeholder={t('searchPlaceholder')}
            aria-label={t('searchPlaceholder')}
            onFocus={() => onSearchFocusChange?.(true)}
            onBlur={() => onSearchFocusChange?.(false)}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.nativeEvent.isComposing || e.nativeEvent.keyCode === 229) return;
              if (e.key === 'ArrowDown') {
                if (!filtered.length) return;
                e.preventDefault();
                setSelIdx((i) => (i + 1) % filtered.length);
              } else if (e.key === 'ArrowUp') {
                if (!filtered.length) return;
                e.preventDefault();
                setSelIdx((i) => (i <= 0 ? filtered.length - 1 : i - 1));
              } else if (e.key === 'Enter') {
                const target = selIdx >= 0 ? filtered[selIdx] : filtered[0];
                if (target) {
                  e.preventDefault();
                  pick(target);
                }
              } else if (e.key === 'Escape') {
                e.preventDefault();
                e.stopPropagation();
                if (query) setQuery('');
                else onClose();
              }
            }}
          />
          {query && (
            <button
              type="button"
              className="ol-search-clear"
              title={t('clearSearch')}
              aria-label={t('clearSearch')}
              onClick={() => setQuery('')}
            >
              <Icon name="x" size={12} />
            </button>
          )}
        </div>
      </div>

      {hasMore && !fullIndexed && (
        <div className="ol-notice">
          <span>{loadingIndex ? t('loadingIndex') : t('loadedOnly')}</span>
          {!loadingIndex && onLoadOlder && (
            <button type="button" className="link-btn small" disabled={loadingOlder} onClick={onLoadOlder}>
              {t('loadEarlier')}
            </button>
          )}
        </div>
      )}

      <div className="ol-list" ref={listRef}>
        {filtered.length === 0 ? (
          <div className="ol-empty">{prompts.length === 0 ? t('emptySession') : t('emptySearch')}</div>
        ) : (
          <>
            {hasSplit && (
              <div className="ol-section-label">
                <span>{t('earlierHistoryHeader')}</span>
                <span>{t('earlierRange', { from: earlierItems[0].index, to: earlierItems[earlierItems.length - 1].index })}</span>
              </div>
            )}
            {earlierItems.map((p, i) => renderRow(p, i))}
            {hasSplit && recentItems.length > 0 && <div className="ol-divider" />}
            {recentItems.map((p, i) => renderRow(p, earlierItems.length + i))}
          </>
        )}
      </div>

      {mode !== 'sheet' && prompts.length > 1 && (
        <div className="ol-foot">
          <button
            type="button"
            className="ol-nav-btn"
            disabled={activeIdx <= 0}
            onClick={() => onStep(-1)}
          >
            <Icon name="up" size={12} />
            <span>{t('prevShort')}</span>
          </button>
          {mode === 'popover' && <span className="ol-kbd">{t('shortcutHint')}</span>}
          <button
            type="button"
            className="ol-nav-btn"
            disabled={activeIdx >= prompts.length - 1}
            onClick={() => onStep(1)}
          >
            <span>{t('nextShort')}</span>
            <Icon name="down" size={12} />
          </button>
        </div>
      )}
    </>
  );
}

/** Mobile Bottom Sheet portal hosting the prompt outline with backdrop, grab handle, Escape, and Android back handling. */
export function PromptOutlineSheet({
  open,
  onClose,
  ...panelProps
}: {
  open: boolean;
  onClose: () => void;
} & Omit<Parameters<typeof PromptOutlinePanel>[0], 'mode' | 'onClose'>) {
  useLang();
  useBack(open, onClose);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.isComposing || e.keyCode === 229) return;
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [open, onClose]);

  if (!open) return null;

  return createPortal(
    <>
      <div className="prompt-mob-scrim" onClick={onClose} />
      <div className="prompt-mob-sheet" role="dialog" aria-modal="true" aria-label={t('title')}>
        <div className="prompt-mob-handle" />
        <PromptOutlinePanel mode="sheet" onClose={onClose} {...panelProps} />
      </div>
    </>,
    document.body,
  );
}
