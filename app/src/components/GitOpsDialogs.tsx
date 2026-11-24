import { useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from 'react';
import { createPortal } from 'react-dom';
import { useBack } from '../back';
import { getT, useLang } from '../i18n';
import { errorToast, toast } from '../store';
import { rpc } from '../transport';
import { isHostSession, normDirPath, type SessionInfo } from '../types';
import { isMobile } from '../util';
import { Icon } from './Icon';
import { openMenu, type MenuItem } from './Overlay';

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
  dirtyCount?: number;
  upstream?: string | null;
  ahead?: number | null;
  behind?: number | null;
  aheadBase?: number | null;
  merged?: boolean;
}

export interface WorktreesResult {
  repo: string | null;
  current: string | null;
  defaultBranch?: string | null;
  worktrees: WorktreeEntry[];
}

export interface BranchEntry {
  name: string;
  current: boolean;
  upstream?: string | null;
  ahead?: number | null;
  behind?: number | null;
  merged?: boolean;
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
  defaultPush = false,
  files,
  onCommitted,
  onClose,
}: {
  hostId: string;
  cwd: string;
  branch: string;
  defaultBranch?: string | null;
  defaultPush?: boolean;
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
  const [pushAfter, setPushAfter] = useState(!!defaultPush);
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
      const hash = res?.commit ?? '';
      if (pushAfter) {
        try {
          const p = await rpc<{ branch: string; remote: string }>(hostId, 'git.push', { cwd, setUpstream: true });
          toast(tx('committedAndPushed', { hash, remote: p?.remote ?? 'origin' }));
        } catch (pushErr) {
          const errMsg = pushErr instanceof Error ? pushErr.message : String(pushErr);
          toast(tx('committedButPushFailed', { hash, err: errMsg }), 'error');
        }
      } else {
        toast(tx('committedChanges', { hash }));
      }
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

        <label className="wp-commit-push-toggle">
          <input type="checkbox" checked={pushAfter} onChange={(e) => setPushAfter(e.target.checked)} />
          <span>{tx('pushAfterCommit')}</span>
        </label>

        <div className="row end">
          <button type="button" className="btn" onClick={onClose} disabled={busy}>
            {tx('cancel')}
          </button>
          <button type="button" className="btn primary" disabled={!canSubmit} onClick={() => void submit()}>
            {busy && <span className="spinner mini" />}
            {pushAfter ? tx('commitAndPushCount', { n: selected.size }) : tx('commitCount', { n: selected.size })}
          </button>
        </div>
      </div>
    </div>
  );
}

const normPath = (p: string) => p.replace(/\\/g, '/').replace(/\/+$/, '').toLowerCase();
const baseName = (p: string) => p.replace(/[\\/]+$/, '').split(/[\\/]/).pop() || p;

/** Convert an arbitrary branch name into a filesystem-safe worktree folder slug matching `gitops::safe_name`. */
export function worktreeSlug(branch: string, existingPaths: string[] = []): string {
  const cleaned = branch
    .trim()
    .replace(/[^a-zA-Z0-9._-]+/g, '-')
    .replace(/^[-.]+|[-.]+$/g, '')
    .slice(0, 48);
  const base = cleaned || `wt-${Date.now().toString(36).slice(-5)}`;
  const used = new Set(existingPaths.map((p) => baseName(p).toLowerCase()));
  if (!used.has(base.toLowerCase())) return base;
  for (let i = 2; i < 100; i++) {
    const cand = `${base.slice(0, 56)}-${i}`;
    if (!used.has(cand.toLowerCase())) return cand;
  }
  return `${base.slice(0, 50)}-${Date.now().toString(36).slice(-5)}`;
}

/** Compact dialog to create a new isolated Git worktree on a new branch from a chosen base branch. */
export function NewWorktreeDialog({
  hostId,
  cwd,
  defaultBase,
  branchesInfo,
  worktrees,
  canOpenSession,
  onCreated,
  onClose,
}: {
  hostId: string;
  cwd: string;
  defaultBase?: string | null;
  branchesInfo: BranchesResult | null;
  worktrees: WorktreeEntry[];
  canOpenSession?: boolean;
  onCreated: (path: string, branch: string, openSession: boolean) => void;
  onClose: () => void;
}) {
  useLang();
  useBack(true, onClose);
  const baseChoices = useMemo(() => {
    const arr = [
      ...(defaultBase ? [defaultBase] : []),
      ...(branchesInfo?.defaultBranch ? [branchesInfo.defaultBranch] : []),
      ...(branchesInfo?.current ? [branchesInfo.current] : []),
      ...(branchesInfo?.branches ?? []).map((b) => b.name),
    ].filter((x) => x && x !== 'HEAD');
    return [...new Set(arr)];
  }, [defaultBase, branchesInfo]);

  const [branch, setBranch] = useState('');
  const [base, setBase] = useState(() => baseChoices[0] ?? 'HEAD');
  const [openSession, setOpenSession] = useState(false);
  const [busy, setBusy] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    requestAnimationFrame(() => inputRef.current?.focus());
  }, []);

  const canSubmit = !busy && branch.trim().length > 0;

  const submit = async () => {
    if (!canSubmit) return;
    const br = branch.trim();
    const name = worktreeSlug(br, worktrees.map((w) => w.path));
    setBusy(true);
    try {
      const res = await rpc<{ path: string; branch: string }>(hostId, 'git.worktreeAdd', {
        cwd,
        name,
        branch: br,
        base: base || 'HEAD',
        prepareEnv: true,
      });
      toast(tx('createdWorktreeToast', { branch: res.branch || br }));
      onCreated(res.path, res.branch || br, openSession);
      onClose();
    } catch (e) {
      errorToast(e);
      setBusy(false);
    }
  };

  return (
    <div className="modal-bg" onClick={onClose}>
      <div className="modal wp-new-wt-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <div className="row">
            <Icon name="git-worktree" size={16} />
            <h3>{tx('newWorktreeTitle')}</h3>
          </div>
          <button type="button" className="icon-btn" onClick={onClose} title={tx('close')}>
            <Icon name="x" size={15} />
          </button>
        </div>

        <label className="wp-review-field">
          <span>{tx('newWorktreeBranchLabel')}</span>
          <input
            ref={inputRef}
            type="text"
            className="mono"
            value={branch}
            placeholder={tx('newBranchPlaceholder')}
            onChange={(e) => setBranch(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.nativeEvent.isComposing && canSubmit) {
                e.preventDefault();
                void submit();
              }
            }}
          />
        </label>

        {baseChoices.length > 0 && (
          <label className="wp-review-field">
            <span>{tx('newWorktreeBaseLabel')}</span>
            <select className="mono" value={base} onChange={(e) => setBase(e.target.value)}>
              {baseChoices.map((b) => (
                <option key={b} value={b}>
                  {b}
                </option>
              ))}
            </select>
          </label>
        )}

        {canOpenSession && (
          <label className="wp-commit-push-toggle">
            <input type="checkbox" checked={openSession} onChange={(e) => setOpenSession(e.target.checked)} />
            <span>{tx('openSessionInNewWorktree')}</span>
          </label>
        )}

        <div className="row end">
          <button type="button" className="btn" onClick={onClose} disabled={busy}>
            {tx('cancel')}
          </button>
          <button type="button" className="btn primary" disabled={!canSubmit} onClick={() => void submit()}>
            {busy && <span className="spinner mini" />}
            {tx('createWorktreeBtn')}
          </button>
        </div>
      </div>
    </div>
  );
}

type PopoverTab = 'worktrees' | 'local' | 'remote';

interface BoundSession {
  id: string;
  title: string;
  running: boolean;
}

type SelectableItem =
  | { kind: 'wt'; key: string; entry: WorktreeEntry; active: boolean; session?: BoundSession }
  | { kind: 'br'; key: string; entry: BranchEntry; active: boolean; worktree?: WorktreeEntry }
  | { kind: 'rem'; key: string; name: string };

const isSessionActive = (s: SessionInfo) =>
  s.status === 'running' || s.status === 'starting' || s.status === 'waiting-permission';

/** Minimalist 4-zone searchable popover for switching & managing worktrees, local branches, and remote branches. */
export function BranchWorktreePopover({
  anchor,
  cwd,
  standalone,
  worktrees,
  defaultBranch,
  branchesInfo,
  loadingBranches,
  fetching,
  hostSessions,
  currentSessionId,
  onSwitchWorktree,
  onJumpToSession,
  onNewSessionAt,
  onCheckoutBranch,
  onCreateBranch,
  onCreateWorktree,
  onOpenBranchInWorktree,
  onMergeWorktreeToDefault,
  onMergeBranchIntoCurrent,
  onRebaseOnto,
  onCompareBranch,
  onRenameBranch,
  onDeleteBranch,
  onDeleteWorktree,
  onCleanupMergedWorktrees,
  onFetch,
  onCopyPath,
  onClose,
}: {
  anchor: HTMLElement;
  cwd: string;
  standalone?: boolean;
  worktrees: WorktreeEntry[];
  defaultBranch?: string | null;
  branchesInfo: BranchesResult | null;
  loadingBranches?: boolean;
  fetching?: boolean;
  hostSessions: SessionInfo[];
  currentSessionId?: string;
  onSwitchWorktree?: (path: string) => void;
  onJumpToSession?: (sessionId: string) => void;
  onNewSessionAt?: (path: string) => void;
  onCheckoutBranch: (branch: string) => Promise<void> | void;
  onCreateBranch: (base?: string) => void;
  onCreateWorktree: (base?: string) => void;
  onOpenBranchInWorktree: (branch: string) => void;
  onMergeWorktreeToDefault: (wt: WorktreeEntry, squash: boolean) => void;
  onMergeBranchIntoCurrent: (branch: string) => void;
  onRebaseOnto: (branch: string) => void;
  onCompareBranch: (branch: string) => void;
  onRenameBranch: (branch: string) => void;
  onDeleteBranch: (branch: string) => void;
  onDeleteWorktree: (wt: WorktreeEntry) => void;
  onCleanupMergedWorktrees: (candidates: WorktreeEntry[]) => void;
  onFetch: () => void;
  onCopyPath: (path: string) => void;
  onClose: () => void;
}) {
  const [, lang] = useLang();
  useBack(true, onClose);
  const popRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const navMode = useRef<'key' | 'mouse'>('key');

  const [tab, setTab] = useState<PopoverTab>(() => (worktrees.length > 1 || !!onSwitchWorktree ? 'worktrees' : 'local'));
  const [mergedOpen, setMergedOpen] = useState(false);
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
      // Let topmost ContextMenu or askConfirm/askText handle Escape before closing the popover
      if (document.querySelector('.ctx-scrim, .dlg-bg')) return;
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
      const width = Math.min(352, window.innerWidth - 16);
      const left = Math.max(8, Math.min(ar.left, window.innerWidth - width - 8));
      const top = ar.bottom + 6;
      const availBelow = window.innerHeight - top - 12;
      const next: CSSProperties =
        availBelow >= 260
          ? { left, top, width, maxHeight: Math.min(460, availBelow) }
          : {
              left,
              bottom: Math.max(8, window.innerHeight - ar.top + 6),
              width,
              maxHeight: Math.max(200, Math.min(460, ar.top - 18)),
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
  const effDefaultBranch = defaultBranch || branchesInfo?.defaultBranch || 'main';
  const term = q.trim().toLowerCase();

  // Map normalized directory path -> primary bound top-level session (running takes priority over most recent idle)
  const sessionByDir = useMemo(() => {
    const map = new Map<string, BoundSession>();
    const sorted = [...hostSessions]
      .filter((s) => isHostSession(s) && !s.sideChat && !s.review && !s.parentId)
      .sort((a, b) => {
        const ar = isSessionActive(a) ? 1 : 0;
        const br = isSessionActive(b) ? 1 : 0;
        if (ar !== br) return br - ar;
        if (a.id === currentSessionId) return -1;
        if (b.id === currentSessionId) return 1;
        return (b.updatedAt ?? b.createdAt) - (a.updatedAt ?? a.createdAt);
      });
    for (const s of sorted) {
      const k = normPath(normDirPath(s.cwd));
      if (!map.has(k)) {
        map.set(k, { id: s.id, title: s.title, running: isSessionActive(s) });
      }
    }
    return map;
  }, [hostSessions, currentSessionId]);

  const worktreeByBranch = useMemo(() => {
    const m = new Map<string, WorktreeEntry>();
    for (const w of worktrees) {
      if (w.branch) m.set(w.branch, w);
    }
    return m;
  }, [worktrees]);

  const {
    currentWt,
    activeWts,
    mergedWts,
    localList,
    remoteList,
    wtTotalCount,
    brTotalCount,
    remTotalCount,
    selectable,
  } = useMemo(() => {
    const matchWt = (w: WorktreeEntry) => {
      if (!term) return true;
      const sess = sessionByDir.get(normPath(w.path));
      const hay = [w.branch ?? '', baseName(w.path), w.path, w.isMain ? tx('mainWorktreeTag') : '', sess?.title ?? '']
        .join('\n')
        .toLowerCase();
      return hay.includes(term);
    };

    const filteredWts = worktrees.filter(matchWt);
    const curWtEntry = filteredWts.find((w) => normPath(w.path) === curNorm) ?? null;
    const otherWts = filteredWts.filter((w) => normPath(w.path) !== curNorm);

    const mergedGroup = otherWts.filter((w) => !w.isMain && (!!w.merged || w.prunable));
    const activeGroup = otherWts
      .filter((w) => w.isMain || (!w.merged && !w.prunable))
      .sort((a, b) => {
        if (a.isMain !== b.isMain) return a.isMain ? -1 : 1;
        const sa = sessionByDir.get(normPath(a.path));
        const sb = sessionByDir.get(normPath(b.path));
        const ra = sa?.running ? 1 : 0;
        const rb = sb?.running ? 1 : 0;
        if (ra !== rb) return rb - ra;
        const da = (a.dirtyCount ?? 0) > 0 || (a.ahead ?? 0) > 0 ? 1 : 0;
        const db = (b.dirtyCount ?? 0) > 0 || (b.ahead ?? 0) > 0 ? 1 : 0;
        if (da !== db) return db - da;
        return (a.branch ?? a.path).localeCompare(b.branch ?? b.path);
      });

    const allBranches = branchesInfo?.branches ?? [];
    const filteredLocal = allBranches
      .filter((b) => !term || b.name.toLowerCase().includes(term))
      .sort((a, b) => {
        if (a.current !== b.current) return a.current ? -1 : 1;
        return 0;
      });

    const allRemotes = branchesInfo?.remoteBranches ?? [];
    const filteredRemote = allRemotes.filter((r) => !term || r.toLowerCase().includes(term));

    const items: SelectableItem[] = [];
    if (tab === 'worktrees') {
      if (curWtEntry) {
        items.push({
          kind: 'wt',
          key: `wt:${curWtEntry.path}`,
          entry: curWtEntry,
          active: true,
          session: sessionByDir.get(normPath(curWtEntry.path)),
        });
      }
      for (const w of activeGroup) {
        items.push({
          kind: 'wt',
          key: `wt:${w.path}`,
          entry: w,
          active: false,
          session: sessionByDir.get(normPath(w.path)),
        });
      }
      if (mergedOpen || !!term) {
        for (const w of mergedGroup) {
          items.push({
            kind: 'wt',
            key: `wt:${w.path}`,
            entry: w,
            active: false,
            session: sessionByDir.get(normPath(w.path)),
          });
        }
      }
    } else if (tab === 'local') {
      for (const b of filteredLocal) {
        items.push({
          kind: 'br',
          key: `br:${b.name}`,
          entry: b,
          active: b.current,
          worktree: worktreeByBranch.get(b.name),
        });
      }
    } else {
      for (const name of filteredRemote) {
        items.push({ kind: 'rem', key: `rem:${name}`, name });
      }
    }

    return {
      currentWt: curWtEntry,
      activeWts: activeGroup,
      mergedWts: mergedGroup,
      localList: filteredLocal,
      remoteList: filteredRemote,
      wtTotalCount: term ? filteredWts.length : worktrees.length,
      brTotalCount: term ? filteredLocal.length : allBranches.length,
      remTotalCount: term ? filteredRemote.length : allRemotes.length,
      selectable: items,
    };
  }, [worktrees, branchesInfo, term, curNorm, sessionByDir, worktreeByBranch, tab, mergedOpen, lang]);

  useEffect(() => {
    navMode.current = 'key';
    setSel(term && selectable.length > 0 ? 0 : -1);
  }, [term, tab, selectable.length]);

  useEffect(() => {
    if (sel < 0 || navMode.current !== 'key') return;
    listRef.current?.querySelector(`[data-bw-idx="${sel}"]`)?.scrollIntoView({ block: 'nearest' });
  }, [sel]);

  const triggerItem = (item: SelectableItem) => {
    if (item.kind === 'wt') {
      onClose();
      if (!item.active) onSwitchWorktree?.(item.entry.path);
    } else if (item.kind === 'br') {
      onClose();
      if (item.worktree && normPath(item.worktree.path) !== curNorm && onSwitchWorktree) {
        onSwitchWorktree(item.worktree.path);
      } else if (!item.active) {
        void onCheckoutBranch(item.entry.name);
      }
    } else {
      onClose();
      void onCheckoutBranch(item.name);
    }
  };

  const openWorktreeMenu = (e: React.MouseEvent<HTMLElement>, w: WorktreeEntry, active: boolean, sess?: BoundSession) => {
    e.stopPropagation();
    const r = e.currentTarget.getBoundingClientRect();
    const canMergeMain = !w.isMain && !!w.branch && w.branch !== effDefaultBranch && !w.prunable;
    const items: (MenuItem | '-')[] = [
      ...(!active && onSwitchWorktree
        ? [
            {
              label: tx('switchPanelToWorktree'),
              icon: 'folder',
              run: () => {
                onClose();
                onSwitchWorktree(w.path);
              },
            },
          ]
        : []),
      ...(sess && onJumpToSession && sess.id !== currentSessionId
        ? [
            {
              label: tx('jumpToBoundSession'),
              icon: 'chat',
              hint: sess.running ? tx('statusRunningShort') : undefined,
              run: () => {
                onClose();
                onJumpToSession(sess.id);
              },
            },
          ]
        : []),
      ...(!standalone && onNewSessionAt
        ? [
            {
              label: tx('newSessionInThisDir'),
              icon: 'plus',
              run: () => {
                onClose();
                onNewSessionAt(w.path);
              },
            },
          ]
        : []),
      ...(canMergeMain
        ? [
            '-' as const,
            {
              label: tx('mergeBackInto', { target: effDefaultBranch }),
              icon: 'git-merge',
              run: () => onMergeWorktreeToDefault(w, false),
            },
            {
              label: tx('squashBackInto', { target: effDefaultBranch }),
              icon: 'git-commit',
              run: () => onMergeWorktreeToDefault(w, true),
            },
          ]
        : []),
      ...((!active && onSwitchWorktree) ||
      (sess && onJumpToSession && sess.id !== currentSessionId) ||
      (!standalone && onNewSessionAt) ||
      canMergeMain
        ? ['-' as const]
        : []),
      ...(w.branch && !w.detached
        ? [
            {
              label: tx('renameBranchAction'),
              icon: 'edit',
              run: () => onRenameBranch(w.branch!),
            },
          ]
        : []),
      {
        label: tx('copyDirectoryPath'),
        icon: 'copy',
        run: () => onCopyPath(w.path),
      },
      ...(!w.isMain
        ? [
            {
              label: tx('deleteWorktreeAction'),
              icon: 'trash',
              danger: true,
              run: () => onDeleteWorktree(w),
            },
          ]
        : []),
    ];
    openMenu({ clientX: r.right - 200, clientY: r.bottom + 4 }, items);
  };

  const openLocalBranchMenu = (e: React.MouseEvent<HTMLElement>, b: BranchEntry, wt?: WorktreeEntry) => {
    e.stopPropagation();
    const r = e.currentTarget.getBoundingClientRect();
    const occupiedElsewhere = !!wt && normPath(wt.path) !== curNorm;
    const isDefault = b.name === branchesInfo?.defaultBranch || b.name === effDefaultBranch;
    const items: (MenuItem | '-')[] = [
      occupiedElsewhere && onSwitchWorktree
        ? {
            label: tx('switchPanelToWorktree'),
            icon: 'git-worktree',
            run: () => {
              onClose();
              onSwitchWorktree(wt.path);
            },
          }
        : {
            label: tx('checkoutInCurrentDir'),
            icon: 'check',
            disabled: b.current,
            run: () => {
              onClose();
              void onCheckoutBranch(b.name);
            },
          },
      {
        label: tx('openInNewWorktree'),
        icon: 'git-worktree',
        disabled: !!wt,
        hint: wt ? tx('alreadyInWorktreeHint') : undefined,
        run: () => {
          onClose();
          onOpenBranchInWorktree(b.name);
        },
      },
      {
        label: tx('newBranchFromThis'),
        icon: 'git-branch',
        run: () => {
          onClose();
          onCreateBranch(b.name);
        },
      },
      ...(!b.current
        ? [
            '-' as const,
            {
              label: tx('mergeIntoCurrentBranch'),
              icon: 'git-merge',
              run: () => onMergeBranchIntoCurrent(b.name),
            },
            {
              label: tx('rebaseOntoBranch'),
              icon: 'git-rebase',
              run: () => onRebaseOnto(b.name),
            },
            {
              label: tx('compareWithCurrent'),
              icon: 'compare',
              run: () => {
                onClose();
                onCompareBranch(b.name);
              },
            },
          ]
        : []),
      '-',
      {
        label: tx('renameBranchAction'),
        icon: 'edit',
        run: () => onRenameBranch(b.name),
      },
      {
        label: tx('deleteBranchAction'),
        icon: 'trash',
        danger: true,
        disabled: b.current || isDefault || !!wt,
        hint: wt ? tx('occupiedByWorktreeHint') : undefined,
        run: () => onDeleteBranch(b.name),
      },
    ];
    openMenu({ clientX: r.right - 210, clientY: r.bottom + 4 }, items);
  };

  const openRemoteBranchMenu = (e: React.MouseEvent<HTMLElement>, name: string) => {
    e.stopPropagation();
    const r = e.currentTarget.getBoundingClientRect();
    openMenu(
      { clientX: r.right - 210, clientY: r.bottom + 4 },
      [
        {
          label: tx('checkoutRemoteTracking'),
          icon: 'git-branch',
          run: () => {
            onClose();
            void onCheckoutBranch(name);
          },
        },
        {
          label: tx('openInNewWorktree'),
          icon: 'git-worktree',
          run: () => {
            onClose();
            onOpenBranchInWorktree(name);
          },
        },
        '-',
        {
          label: tx('mergeIntoCurrentBranch'),
          icon: 'git-merge',
          run: () => onMergeBranchIntoCurrent(name),
        },
        {
          label: tx('compareWithCurrent'),
          icon: 'compare',
          run: () => {
            onClose();
            onCompareBranch(name);
          },
        },
      ],
    );
  };

  const showLoadingBranches = !!loadingBranches && !branchesInfo && tab !== 'worktrees';
  const showMerged = mergedOpen || !!term;

  const formatWtStats = (w: WorktreeEntry) => {
    const parts: string[] = [];
    const dirty = w.dirtyCount ?? 0;
    if (dirty > 0) parts.push(tx('dirtyChangesCount', { n: dirty }));
    if ((w.ahead ?? 0) > 0) parts.push(`↑${w.ahead}`);
    else if (!w.upstream && (w.aheadBase ?? 0) > 0) parts.push(`↑${w.aheadBase}`);
    if ((w.behind ?? 0) > 0) parts.push(`↓${w.behind}`);
    if (parts.length > 0) return parts.join(' ');
    if (w.isMain && w.dirtyCount === 0 && !w.ahead && !w.behind) return tx('wtSyncedShort');
    return '';
  };

  const indexOfKey = (key: string) => selectable.findIndex((x) => x.key === key);

  const renderWorktreeRow = (w: WorktreeEntry, isPinned: boolean) => {
    const key = `wt:${w.path}`;
    const idx = indexOfKey(key);
    const active = normPath(w.path) === curNorm;
    const sess = sessionByDir.get(normPath(w.path));
    const label = w.branch ? w.branch : baseName(w.path);
    const statText = formatWtStats(w);
    const showHoverBar = !w.prunable;

    if (isPinned) {
      const subParts = [
        sess ? (sess.running ? tx('statusRunningShort') : sess.title || tx('statusIdleSession')) : null,
        w.isMain ? baseName(w.path) : effDefaultBranch ? tx('basedOnBranch', { base: effDefaultBranch }) : baseName(w.path),
      ].filter(Boolean);
      return (
        <div
          key={w.path}
          data-bw-idx={idx}
          role="button"
          tabIndex={0}
          className={`wp-bw-item current-pin ${sel === idx ? 'sel' : ''}`}
          title={w.path}
          onMouseMove={() => {
            if (idx >= 0 && sel !== idx) {
              navMode.current = 'mouse';
              setSel(idx);
            }
          }}
          onClick={() => onClose()}
          onKeyDown={(e) => e.key === 'Enter' && onClose()}
        >
          <Icon name={w.isMain ? 'home' : 'git-worktree'} size={14} />
          <div className="wp-bw-main">
            <div className="wp-bw-line1">
              <span className="wp-bw-name mono truncate">{label}</span>
              <span className="wp-bw-quiet-tag">{tx('currentTag')}</span>
              {w.isMain && <span className="wp-bw-quiet-tag">{tx('mainWorktreeTag')}</span>}
            </div>
            <div className="wp-bw-sub truncate">
              {sess && <span className={`wp-bw-dot ${sess.running ? 'running' : 'idle'}`} />}
              <span className="truncate">{subParts.join(' · ')}</span>
            </div>
          </div>
          <div className="wp-bw-right">
            <span className="wp-bw-stat mono">
              {statText}
              {statText ? ' ' : ''}
              <span className="wp-bw-check">
                <Icon name="check" size={13} />
              </span>
            </span>
            <div className="wp-bw-acts" onClick={(e) => e.stopPropagation()}>
              <button
                type="button"
                className="wp-bw-act-btn wp-bw-more-btn"
                title={tx('moreActions')}
                onClick={(e) => openWorktreeMenu(e, w, active, sess)}
              >
                <Icon name="more" size={13} />
              </button>
            </div>
          </div>
        </div>
      );
    }

    return (
      <div
        key={w.path}
        data-bw-idx={idx}
        role="button"
        tabIndex={0}
        className={`wp-bw-item ${w.merged || w.prunable ? 'muted-row' : ''} ${sel === idx ? 'sel' : ''}`}
        title={w.path}
        onMouseMove={() => {
          if (idx >= 0 && sel !== idx) {
            navMode.current = 'mouse';
            setSel(idx);
          }
        }}
        onClick={() => {
          const item = idx >= 0 ? selectable[idx] : undefined;
          if (item) triggerItem(item);
        }}
        onKeyDown={(e) => {
          if (e.key === 'Enter') {
            const item = idx >= 0 ? selectable[idx] : undefined;
            if (item) triggerItem(item);
          }
        }}
      >
        <Icon name={w.isMain ? 'home' : 'git-worktree'} size={13} />
        <div className="wp-bw-line1 grow">
          <span className="wp-bw-name mono truncate">{label}</span>
          {sess && (
            <span
              className={`wp-bw-dot ${sess.running ? 'running' : 'idle'}`}
              title={sess.running ? tx('statusRunningShort') : sess.title}
            />
          )}
          {w.isMain && <span className="wp-bw-quiet-tag">{tx('mainWorktreeTag')}</span>}
          {w.prunable ? (
            <span className="wp-bw-quiet-tag">{tx('wtPrunableTag')}</span>
          ) : (
            w.merged && <span className="wp-bw-quiet-tag ok">{tx('mergedIntoBaseShort', { base: effDefaultBranch })}</span>
          )}
        </div>
        <div className="wp-bw-right">
          {statText && <span className="wp-bw-stat mono">{statText}</span>}
          <div className="wp-bw-acts" onClick={(e) => e.stopPropagation()}>
            {showHoverBar && sess && onJumpToSession && sess.id !== currentSessionId ? (
              <button
                type="button"
                className="wp-bw-act-btn"
                title={tx('jumpToBoundSession')}
                onClick={() => {
                  onClose();
                  onJumpToSession(sess.id);
                }}
              >
                <Icon name="chat" size={12} />
              </button>
            ) : (
              showHoverBar &&
              !standalone &&
              onNewSessionAt && (
                <button
                  type="button"
                  className="wp-bw-act-btn"
                  title={tx('newSessionInThisDir')}
                  onClick={() => {
                    onClose();
                    onNewSessionAt(w.path);
                  }}
                >
                  <Icon name="plus" size={12} />
                </button>
              )
            )}
            {showHoverBar && !w.isMain && !!w.branch && w.branch !== effDefaultBranch && (
              <button
                type="button"
                className="wp-bw-act-btn"
                title={tx('mergeBackInto', { target: effDefaultBranch })}
                onClick={() => onMergeWorktreeToDefault(w, false)}
              >
                <Icon name="git-merge" size={12} />
              </button>
            )}
            {!w.isMain && (w.merged || w.prunable) && (
              <button
                type="button"
                className="wp-bw-act-btn danger"
                title={tx('deleteWorktreeAction')}
                onClick={() => onDeleteWorktree(w)}
              >
                <Icon name="trash" size={12} />
              </button>
            )}
            <button
              type="button"
              className="wp-bw-act-btn wp-bw-more-btn"
              title={tx('moreActions')}
              onClick={(e) => openWorktreeMenu(e, w, active, sess)}
            >
              <Icon name="more" size={13} />
            </button>
          </div>
        </div>
      </div>
    );
  };

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
        {/* Zone 1: Single-line search + Fetch */}
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
          <button
            type="button"
            className={`icon-btn wp-bw-fetch ${fetching ? 'spinning' : ''}`}
            title={tx('fetchRemotesAction')}
            aria-label={tx('fetchRemotesAction')}
            disabled={fetching}
            onClick={onFetch}
          >
            <Icon name="refresh" size={13} />
          </button>
        </div>

        {/* Zone 2: Minimal underline tabs */}
        <div className="wp-bw-tabs" role="tablist">
          <button
            type="button"
            role="tab"
            aria-selected={tab === 'worktrees'}
            className={`wp-bw-tab ${tab === 'worktrees' ? 'active' : ''}`}
            onClick={() => setTab('worktrees')}
          >
            <span>{tx('tabWorktrees')}</span>
            <span className="wp-bw-tab-cnt">{wtTotalCount}</span>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === 'local'}
            className={`wp-bw-tab ${tab === 'local' ? 'active' : ''}`}
            onClick={() => setTab('local')}
          >
            <span>{tx('tabLocalBranches')}</span>
            <span className="wp-bw-tab-cnt">{brTotalCount}</span>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === 'remote'}
            className={`wp-bw-tab ${tab === 'remote' ? 'active' : ''}`}
            onClick={() => setTab('remote')}
          >
            <span>{tx('tabRemoteBranches')}</span>
            <span className="wp-bw-tab-cnt">{remTotalCount}</span>
          </button>
        </div>

        {/* Zone 3: Breathable list */}
        <div className="wp-bw-list" ref={listRef}>
          {tab === 'worktrees' && (
            <>
              {currentWt && renderWorktreeRow(currentWt, true)}
              {currentWt && (activeWts.length > 0 || mergedWts.length > 0) && <div className="wp-bw-sep" />}
              {activeWts.map((w) => renderWorktreeRow(w, false))}
              {mergedWts.length > 0 && (
                <>
                  {(currentWt || activeWts.length > 0) && <div className="wp-bw-sep" />}
                  <div className="wp-bw-merged-head">
                    <button
                      type="button"
                      className="wp-bw-merged-toggle"
                      aria-expanded={showMerged}
                      onClick={() => setMergedOpen((v) => !v)}
                    >
                      <span className={`chev ${showMerged ? 'open' : ''}`}>
                        <Icon name="chevron" size={12} />
                      </span>
                      <span>{tx('mergedWorktreesGroup', { count: mergedWts.length })}</span>
                    </button>
                    <button
                      type="button"
                      className="wp-bw-cleanup-btn"
                      onClick={() => onCleanupMergedWorktrees(mergedWts)}
                    >
                      {showMerged ? tx('cleanupAllMerged', { count: mergedWts.length }) : tx('oneClickCleanup')}
                    </button>
                  </div>
                  {showMerged && (
                    <>
                      {mergedWts.map((w) => renderWorktreeRow(w, false))}
                      <div className="wp-bw-merged-tip">{tx('cleanupSafetyTip')}</div>
                    </>
                  )}
                </>
              )}
              {!currentWt && activeWts.length === 0 && mergedWts.length === 0 && (
                <div className="wp-bw-empty">{tx('noMatchingBranches')}</div>
              )}
            </>
          )}

          {tab === 'local' && (
            <>
              {showLoadingBranches ? (
                <div className="wp-bw-loading">
                  <span className="spinner mini" />
                  <span>{tx('loading')}</span>
                </div>
              ) : localList.length === 0 ? (
                <div className="wp-bw-empty">{tx('noMatchingBranches')}</div>
              ) : (
                <>
                  <div className="wp-bw-hint-row">{tx('localBranchListHint')}</div>
                  {localList.map((b) => {
                    const key = `br:${b.name}`;
                    const idx = indexOfKey(key);
                    const wt = worktreeByBranch.get(b.name);
                    const syncHint = [b.ahead ? `↑${b.ahead}` : '', b.behind ? `↓${b.behind}` : ''].filter(Boolean).join(' ');
                    return (
                      <div
                        key={b.name}
                        data-bw-idx={idx}
                        role="button"
                        tabIndex={0}
                        className={`wp-bw-item ${b.current ? 'current-pin' : ''} ${sel === idx ? 'sel' : ''}`}
                        title={b.name}
                        onMouseMove={() => {
                          if (idx >= 0 && sel !== idx) {
                            navMode.current = 'mouse';
                            setSel(idx);
                          }
                        }}
                        onClick={() => {
                          const item = idx >= 0 ? selectable[idx] : undefined;
                          if (item) triggerItem(item);
                        }}
                        onKeyDown={(e) => {
                          if (e.key === 'Enter') {
                            const item = idx >= 0 ? selectable[idx] : undefined;
                            if (item) triggerItem(item);
                          }
                        }}
                      >
                        <Icon name="git-branch" size={13} />
                        <div className="wp-bw-line1 grow">
                          <span className="wp-bw-name mono truncate">{b.name}</span>
                          {b.name === branchesInfo?.defaultBranch && (
                            <span className="wp-bw-quiet-tag">{tx('defaultBranchTag')}</span>
                          )}
                          {wt && !b.current && <span className="wp-bw-quiet-tag">{tx('worktreeTag')}</span>}
                        </div>
                        <div className="wp-bw-right">
                          <span className="wp-bw-stat mono">
                            {syncHint || (b.merged ? tx('mergedTagShort') : '')}
                            {b.current && (
                              <span className="wp-bw-check">
                                <Icon name="check" size={13} />
                              </span>
                            )}
                          </span>
                          <div className="wp-bw-acts" onClick={(e) => e.stopPropagation()}>
                            <button
                              type="button"
                              className="wp-bw-act-btn wp-bw-more-btn"
                              title={tx('moreActions')}
                              onClick={(e) => openLocalBranchMenu(e, b, wt)}
                            >
                              <Icon name="more" size={13} />
                            </button>
                          </div>
                        </div>
                      </div>
                    );
                  })}
                </>
              )}
            </>
          )}

          {tab === 'remote' && (
            <>
              {showLoadingBranches ? (
                <div className="wp-bw-loading">
                  <span className="spinner mini" />
                  <span>{tx('loading')}</span>
                </div>
              ) : remoteList.length === 0 ? (
                <div className="wp-bw-empty">{tx('noMatchingBranches')}</div>
              ) : (
                remoteList.map((name) => {
                  const key = `rem:${name}`;
                  const idx = indexOfKey(key);
                  return (
                    <div
                      key={name}
                      data-bw-idx={idx}
                      role="button"
                      tabIndex={0}
                      className={`wp-bw-item ${sel === idx ? 'sel' : ''}`}
                      title={name}
                      onMouseMove={() => {
                        if (idx >= 0 && sel !== idx) {
                          navMode.current = 'mouse';
                          setSel(idx);
                        }
                      }}
                      onClick={() => {
                        const item = idx >= 0 ? selectable[idx] : undefined;
                        if (item) triggerItem(item);
                      }}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter') {
                          const item = idx >= 0 ? selectable[idx] : undefined;
                          if (item) triggerItem(item);
                        }
                      }}
                    >
                      <Icon name="cloud" size={13} />
                      <span className="wp-bw-name mono truncate">{name}</span>
                      <div className="wp-bw-right">
                        <div className="wp-bw-acts" onClick={(e) => e.stopPropagation()}>
                          <button
                            type="button"
                            className="wp-bw-act-btn wp-bw-more-btn"
                            title={tx('moreActions')}
                            onClick={(e) => openRemoteBranchMenu(e, name)}
                          >
                            <Icon name="more" size={13} />
                          </button>
                        </div>
                      </div>
                    </div>
                  );
                })
              )}
            </>
          )}
        </div>

        {/* Zone 4: Minimal single-row footer */}
        <div className="wp-bw-foot">
          <button
            type="button"
            className="wp-bw-foot-btn"
            onClick={() => {
              onClose();
              onCreateWorktree();
            }}
          >
            <Icon name="plus" size={12} />
            <span>{tx('newWorktreeFoot')}</span>
          </button>
          <span className="wp-bw-foot-sep" aria-hidden="true" />
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
        </div>
      </div>
    </div>,
    document.body,
  );
}
