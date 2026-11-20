import { useEffect, useMemo, useRef, useState } from 'react';
import { useBack } from '../back';
import { getT, useLang } from '../i18n';
import { errorToast, toast } from '../store';
import { rpc } from '../transport';
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
