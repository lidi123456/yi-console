import { useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from 'react';
import { createPortal } from 'react-dom';
import { useBack } from '../back';
import { getT, useLang } from '../i18n';
import { errorToast, toast } from '../store';
import { rpc } from '../transport';
import { isMobile } from '../util';
import { Icon } from './Icon';

const tx = getT('filePanel');

export interface WorktreeEntry {
  path: string;
  head: string;
  branch?: string | null;
  detached: boolean;
  isMain: boolean;
  isCurrent: boolean;
  bare: boolean;
  prunable: boolean;
}

export interface WorktreesResult {
  repo: string | null;
  current: string | null;
  worktrees: WorktreeEntry[];
}

export interface BranchEntry {
  name: string;
  current: boolean;
  upstream?: string | null;
  ahead?: number | null;
  behind?: number | null;
}

export interface BranchesResult {
  repo: string | null;
  current: string | null;
  defaultBranch: string | null;
  branches: BranchEntry[];
  remoteBranches?: string[];
}

export interface CommitFileItem {
  rel: string;
  status: string;
  added?: number | null;
  deleted?: number | null;
}

export function CommitDialog({
  hostId,
  cwd,
  branch,
  defaultBranch,
  files,
  onCommitted,
  onClose,
}: {
  hostId: string;
  cwd: string;
  branch: string;
  defaultBranch?: string | null;
  files: CommitFileItem[];
  onCommitted: () => void;
  onClose: () => void;
}) {
  useLang();
  useBack(true, onClose);
  const [selected, setSelected] = useState<Set<string>>(() => new Set(files.map((f) => f.rel)));
  const [message, setMessage] = useState('');
  const [createBranch, setCreateBranch] = useState(false);
  const [newBranch, setNewBranch] = useState('');
  const [busy, setBusy] = useState(false);
  const msgRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    requestAnimationFrame(() => msgRef.current?.focus());
  }, []);

  const isDefaultBranch = useMemo(() => {
    const b = branch.trim();
    if (!b || b === 'HEAD') return false;
    if (defaultBranch && b === defaultBranch) return true;
    return b === 'main' || b === 'master';
  }, [branch, defaultBranch]);

  const toggle = (rel: string) =>
    setSelected((cur) => {
      const next = new Set(cur);
      if (next.has(rel)) next.delete(rel);
      else next.add(rel);
      return next;
    });

  const canSubmit = !busy && selected.size > 0 && message.trim().length > 0 && (!createBranch || newBranch.trim().length > 0);

  const submit = async () => {
    if (!canSubmit) return;
    setBusy(true);
    try {
      if (createBranch && newBranch.trim()) {
        await rpc(hostId, 'git.checkout', { cwd, branch: newBranch.trim(), create: true });
      }
      const res = await rpc<{ commit?: string }>(hostId, 'git.commit', {
        cwd,
        paths: [...selected],
        message: message.trim(),
      });
      toast(tx('committedChanges', { hash: res?.commit ?? '' }));
      onCommitted();
      onClose();
    } catch (e) {
      errorToast(e);
      setBusy(false);
    }
  };

  return (
    <div className="modal-bg" onClick={onClose}>
      <div className="modal wp-commit-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <div className="row">
            <Icon name="git-commit" size={16} />
            <h3>{tx('commitChanges')}</h3>
            {branch && (
              <span className="wp-commit-branch mono">
                <Icon name="git-branch" size={12} />
                {branch}
              </span>
            )}
          </div>
          <button type="button" className="icon-btn" onClick={onClose} title={tx('close')}>
            <Icon name="x" size={15} />
          </button>
        </div>

        {isDefaultBranch && (
          <div className="wp-commit-guard">
            <div className="wp-commit-guard-row">
              <Icon name="alert" size={14} />
              <span>{tx('defaultBranchCommitWarn', { branch })}</span>
            </div>
            <label className="wp-commit-newbr-toggle">
              <input type="checkbox" checked={createBranch} onChange={(e) => setCreateBranch(e.target.checked)} />
              <span>{tx('createNewBranchBeforeCommit')}</span>
            </label>
            {createBranch && (
              <input
                type="text"
                className="mono"
                value={newBranch}
                placeholder={tx('newBranchPlaceholder')}
                onChange={(e) => setNewBranch(e.target.value)}
              />
            )}
          </div>
        )}

        <div className="wp-commit-files-head">
          <span className="muted small">{tx('commitFilesSelected', { n: selected.size, total: files.length })}</span>
          <div className="row">
            <button type="button" className="wp-review-link" onClick={() => setSelected(new Set(files.map((f) => f.rel)))}>
              {tx('selectAll')}
            </button>
            <button type="button" className="wp-review-link" onClick={() => setSelected(new Set())}>
              {tx('clearSelection')}
            </button>
          </div>
        </div>
        <div className="wp-commit-files">
          {files.map((f) => (
            <label key={f.rel} className="wp-commit-pick">
              <input type="checkbox" checked={selected.has(f.rel)} onChange={() => toggle(f.rel)} />
              <span className={`wp-commit-st st-${f.status.toLowerCase()}`}>{f.status === 'new' ? 'U' : f.status[0]}</span>
              <span className="mono truncate grow" title={f.rel}>
                {f.rel}
              </span>
              {(f.added != null || f.deleted != null) && (
                <span className="wp-stat">
                  {!!f.added && <span className="add">+{f.added}</span>}
                  {!!f.deleted && <span className="del">−{f.deleted}</span>}
                </span>
              )}
            </label>
          ))}
        </div>

        <label className="wp-review-field">
          <span>{tx('commitMessageLabel')}</span>
          <textarea
            ref={msgRef}
            rows={3}
            value={message}
            placeholder={tx('commitMessagePlaceholder')}
            onChange={(e) => setMessage(e.target.value)}
            onKeyDown={(e) => {
              if ((e.metaKey || e.ctrlKey) && e.key === 'Enter' && !e.nativeEvent.isComposing) {
                e.preventDefault();
                void submit();
              }
            }}
          />
        </label>

        <div className="row end">
          <button type="button" className="btn" onClick={onClose} disabled={busy}>
            {tx('cancel')}
          </button>
          <button type="button" className="btn primary" disabled={!canSubmit} onClick={() => void submit()}>
            {busy && <span className="spinner mini" />}
            {tx('commitCount', { n: selected.size })}
          </button>
        </div>
      </div>
    </div>
  );
}

const normPath = (p: string) => p.replace(/\\/g, '/').replace(/\/+$/, '').toLowerCase();
const baseName = (p: string) => p.replace(/[\\/]+$/, '').split(/[\\/]/).pop() || p;

type SelectableItem =
  | { kind: 'wt'; key: string; entry: WorktreeEntry; active: boolean }
  | { kind: 'br'; key: string; entry: BranchEntry; active: boolean };

/** Anchored searchable popover for switching git worktrees or checking out / creating local branches */
export function BranchWorktreePopover({
  anchor,
  cwd,
  baseCwd,
  isOtherWorktree,
  standalone,
  worktrees,
  branchesInfo,
  loadingBranches,
  onSwitchWorktree,
  onCheckoutBranch,
  onCreateBranch,
  onNewSession,
  onClose,
}: {
  anchor: HTMLElement;
  cwd: string;
  baseCwd?: string;
  isOtherWorktree: boolean;
  standalone?: boolean;
  worktrees: WorktreeEntry[];
  branchesInfo: BranchesResult | null;
  loadingBranches?: boolean;
  onSwitchWorktree?: (path: string) => void;
  onCheckoutBranch: (branch: string) => Promise<void> | void;
  onCreateBranch: () => void;
  onNewSession?: () => void;
  onClose: () => void;
}) {
  const [, lang] = useLang();
  useBack(true, onClose);
  const popRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const navMode = useRef<'key' | 'mouse'>('key');
  const [q, setQ] = useState('');
  const [sel, setSel] = useState(-1);
  const [pos, setPos] = useState<CSSProperties | null>(null);

  useEffect(() => {
    if (!isMobile()) {
      requestAnimationFrame(() => inputRef.current?.focus());
    }
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || e.isComposing || e.keyCode === 229) return;
      e.preventDefault();
      e.stopImmediatePropagation();
      onClose();
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [onClose]);

  useLayoutEffect(() => {
    const place = () => {
      if (window.innerWidth <= 760) {
        setPos((old) => (old && Object.keys(old).length === 0 ? old : {}));
        return;
      }
      const ar = anchor.getBoundingClientRect();
      const width = Math.min(340, window.innerWidth - 16);
      const left = Math.max(8, Math.min(ar.left, window.innerWidth - width - 8));
      const top = ar.bottom + 6;
      const availBelow = window.innerHeight - top - 12;
      const next: CSSProperties =
        availBelow >= 220
          ? { left, top, width, maxHeight: Math.min(420, availBelow) }
          : {
              left,
              bottom: Math.max(8, window.innerHeight - ar.top + 6),
              width,
              maxHeight: Math.max(180, Math.min(420, ar.top - 18)),
            };
      setPos((old) =>
        old &&
        old.left === next.left &&
        old.top === next.top &&
        old.bottom === next.bottom &&
        old.width === next.width &&
        old.maxHeight === next.maxHeight
          ? old
          : next,
      );
    };
    place();
    const ro = typeof ResizeObserver !== 'undefined' ? new ResizeObserver(place) : null;
    ro?.observe(anchor);
    window.addEventListener('resize', place);
    return () => {
      ro?.disconnect();
      window.removeEventListener('resize', place);
    };
  }, [anchor]);

  const curNorm = normPath(cwd);
  const hasMultiWt = !!onSwitchWorktree && worktrees.length > 1;
  const term = q.trim().toLowerCase();

  const { filteredWorktrees, filteredBranches, selectable } = useMemo(() => {
    const wtBranches = new Set<string>();
    for (const w of worktrees) {
      if (w.branch) wtBranches.add(w.branch);
    }
    const wtList = hasMultiWt
      ? worktrees.filter((w) => {
          if (!term) return true;
          const hay = [w.branch ?? '', baseName(w.path), w.path, w.isMain ? tx('mainWorktreeTag') : '']
            .join('\n')
            .toLowerCase();
          return hay.includes(term);
        })
      : [];
    const extraBranches = (branchesInfo?.branches ?? []).filter((b) => !hasMultiWt || !wtBranches.has(b.name));
    const brList = extraBranches.filter((b) => !term || b.name.toLowerCase().includes(term));
    const items: SelectableItem[] = [
      ...wtList.map((entry) => ({
        kind: 'wt' as const,
        key: `wt:${entry.path}`,
        entry,
        active: normPath(entry.path) === curNorm,
      })),
      ...brList.map((entry) => ({
        kind: 'br' as const,
        key: `br:${entry.name}`,
        entry,
        active: !hasMultiWt && entry.current,
      })),
    ];
    return { filteredWorktrees: wtList, filteredBranches: brList, selectable: items };
  }, [worktrees, branchesInfo, hasMultiWt, term, curNorm, lang]);

  useEffect(() => {
    navMode.current = 'key';
    setSel(term && selectable.length > 0 ? 0 : -1);
  }, [term, selectable.length]);

  useEffect(() => {
    if (sel < 0 || navMode.current !== 'key') return;
    listRef.current?.querySelector(`[data-bw-idx="${sel}"]`)?.scrollIntoView({ block: 'nearest' });
  }, [sel]);

  const triggerItem = (item: SelectableItem) => {
    onClose();
    if (item.kind === 'wt') {
      if (!item.active) onSwitchWorktree?.(item.entry.path);
    } else if (!item.active) {
      void onCheckoutBranch(item.entry.name);
    }
  };

  const wtCount = filteredWorktrees.length;
  const brCount = filteredBranches.length;
  const showLoadingBranches = !!loadingBranches && !branchesInfo;

  return createPortal(
    <div className="wp-bw-scrim" onMouseDown={onClose}>
      <div
        ref={popRef}
        className="wp-bw-pop"
        role="dialog"
        aria-label={tx('switchBranchOrWorktree')}
        style={pos ? pos : { visibility: 'hidden' }}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="wp-bw-search">
          <Icon name="search" size={13} />
          <input
            ref={inputRef}
            value={q}
            placeholder={tx('searchBranchOrWorktree')}
            aria-label={tx('searchBranchOrWorktree')}
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => {
              if (e.nativeEvent.isComposing || e.keyCode === 229) return;
              if (e.key === 'Escape') {
                e.preventDefault();
                onClose();
                return;
              }
              if (!selectable.length) return;
              if (e.key === 'ArrowDown') {
                e.preventDefault();
                navMode.current = 'key';
                setSel((cur) => (cur + 1) % selectable.length);
              } else if (e.key === 'ArrowUp') {
                e.preventDefault();
                navMode.current = 'key';
                setSel((cur) => (cur <= 0 ? selectable.length - 1 : cur - 1));
              } else if (e.key === 'Enter' && sel >= 0 && sel < selectable.length) {
                e.preventDefault();
                triggerItem(selectable[sel]);
              }
            }}
          />
          {q && (
            <button
              type="button"
              className="icon-btn"
              title={tx('clear')}
              aria-label={tx('clear')}
              onClick={() => {
                setQ('');
                inputRef.current?.focus();
              }}
            >
              <Icon name="x" size={12} />
            </button>
          )}
        </div>

        <div className="wp-bw-list" ref={listRef}>
          {wtCount > 0 && (
            <>
              <div className="wp-bw-sec">{tx('worktreesSection', { count: wtCount })}</div>
              {filteredWorktrees.map((w, i) => {
                const idx = i;
                const item = selectable[idx];
                const active = item?.active ?? false;
                const label = w.branch ? w.branch : baseName(w.path);
                return (
                  <button
                    key={w.path}
                    type="button"
                    data-bw-idx={idx}
                    className={`wp-bw-item ${active ? 'active' : ''} ${sel === idx ? 'sel' : ''}`}
                    title={w.path}
                    onMouseMove={() => {
                      if (sel !== idx) {
                        navMode.current = 'mouse';
                        setSel(idx);
                      }
                    }}
                    onClick={() => item && triggerItem(item)}
                  >
                    <Icon name="git-branch" size={13} />
                    <span className="wp-bw-name mono truncate">{label}</span>
                    {w.isMain && <span className="wp-bw-tag">{tx('mainWorktreeTag')}</span>}
                    <span className="wp-bw-meta truncate">{baseName(w.path)}</span>
                    {active && (
                      <span className="wp-bw-check">
                        <Icon name="check" size={13} />
                      </span>
                    )}
                  </button>
                );
              })}
            </>
          )}

          {brCount > 0 && (
            <>
              <div className="wp-bw-sec">{tx('localBranchesSection', { count: brCount })}</div>
              {filteredBranches.map((b, i) => {
                const idx = wtCount + i;
                const item = selectable[idx];
                const active = item?.active ?? false;
                const syncHint = [b.ahead ? `↑${b.ahead}` : '', b.behind ? `↓${b.behind}` : ''].filter(Boolean).join(' ');
                return (
                  <button
                    key={b.name}
                    type="button"
                    data-bw-idx={idx}
                    className={`wp-bw-item ${active ? 'active' : ''} ${sel === idx ? 'sel' : ''}`}
                    title={b.name}
                    onMouseMove={() => {
                      if (sel !== idx) {
                        navMode.current = 'mouse';
                        setSel(idx);
                      }
                    }}
                    onClick={() => item && triggerItem(item)}
                  >
                    <Icon name="git-branch" size={13} />
                    <span className="wp-bw-name mono truncate">{b.name}</span>
                    {b.name === branchesInfo?.defaultBranch && <span className="wp-bw-tag">{tx('defaultBranchTag')}</span>}
                    {syncHint && <span className="wp-bw-meta mono">{syncHint}</span>}
                    {active && (
                      <span className="wp-bw-check">
                        <Icon name="check" size={13} />
                      </span>
                    )}
                  </button>
                );
              })}
            </>
          )}

          {showLoadingBranches && (
            <div className="wp-bw-loading">
              <span className="spinner mini" />
              <span>{tx('loading')}</span>
            </div>
          )}

          {selectable.length === 0 && !showLoadingBranches && (
            <div className="wp-bw-empty">{tx('noMatchingBranches')}</div>
          )}
        </div>

        <div className="wp-bw-foot">
          {isOtherWorktree && baseCwd && onSwitchWorktree && (
            <button
              type="button"
              className="wp-bw-foot-btn"
              onClick={() => {
                onClose();
                onSwitchWorktree(baseCwd);
              }}
            >
              <Icon name="back" size={12} />
              <span>{tx('backToSessionDir')}</span>
            </button>
          )}
          <button
            type="button"
            className="wp-bw-foot-btn"
            onClick={() => {
              onClose();
              onCreateBranch();
            }}
          >
            <Icon name="plus" size={12} />
            <span>{tx('newBranch')}</span>
          </button>
          {!standalone && onNewSession && (
            <button
              type="button"
              className="wp-bw-foot-btn"
              onClick={() => {
                onClose();
                onNewSession();
              }}
            >
              <Icon name="plus" size={12} />
              <span>{tx('newSessionInCurrentWorktree')}</span>
            </button>
          )}
        </div>
      </div>
    </div>,
    document.body,
  );
}
