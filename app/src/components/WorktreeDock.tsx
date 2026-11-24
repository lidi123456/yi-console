import { useEffect, useState } from 'react';
import { getT, useLang } from '../i18n';
import { errorToast, toast, useStore } from '../store';
import { rpc } from '../transport';
import { isHostSession, normDirPath, sessionProjectDir, type GitStatus, type SessionInfo } from '../types';
import { Icon } from './Icon';

const t = getT('sessionView');

/**
 * Lightweight conflict guard above the composer:
 * when a main-directory session shares a Git repository with another running session, warns and offers 1-click worktree isolation.
 */
export function WorktreeDock({
  hostId,
  info,
  connected,
}: {
  hostId: string;
  info: SessionInfo;
  connected: boolean;
}) {
  useLang();
  const hostSessions = useStore((st) => st.sessions[hostId]);
  const [dismissedConflict, setDismissedConflict] = useState(false);
  const [detailsOpen, setDetailsOpen] = useState(false);
  const [isGitRepo, setIsGitRepo] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setDismissedConflict(false);
    setDetailsOpen(false);
  }, [info.id, info.cwd]);

  const busyTurn = info.status === 'running' || info.status === 'starting' || info.status === 'waiting-permission';
  const eligible = isHostSession(info) && info.mode === 'structured' && info.tool !== 'shell' && info.tool !== 'files';

  // Detect another top-level session running concurrently in the same main directory
  const conflictingSession =
    !info.worktree && eligible && !dismissedConflict
      ? (hostSessions ?? []).find(
          (x) =>
            x.id !== info.id &&
            isHostSession(x) &&
            !x.worktree &&
            normDirPath(sessionProjectDir(x)) === normDirPath(info.cwd) &&
            (x.status === 'running' || x.status === 'starting' || x.status === 'waiting-permission'),
        )
      : undefined;

  const conflictDir = conflictingSession ? info.cwd : '';
  useEffect(() => {
    if (!conflictDir) setDetailsOpen(false);
    if (!connected || !conflictDir) {
      setIsGitRepo(false);
      return;
    }
    let alive = true;
    rpc<GitStatus | null>(hostId, 'git.status', { cwd: conflictDir })
      .then((r) => alive && setIsGitRepo(!!r))
      .catch(() => alive && setIsGitRepo(false));
    return () => {
      alive = false;
    };
  }, [hostId, connected, conflictDir]);

  if (!conflictingSession || !isGitRepo) return null;

  const enterWorktree = async () => {
    setBusy(true);
    try {
      const updated = await rpc<SessionInfo>(hostId, 'session.enterWorktree', { sessionId: info.id });
      toast(t('enteredWorktreeToast', { branch: updated.worktree?.branch ?? '' }), 'info');
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const conflictTitle = conflictingSession.title || t('newSession');
  const conflictTip = t('worktreeConflictTip', { title: conflictTitle });
  const conflictSummary = busyTurn
    ? t('worktreeConflictSummaryBusy', { title: conflictTitle })
    : t('worktreeConflictSummary', { title: conflictTitle });

  return (
    <div className={`wt-dock wt-conflict-dock${detailsOpen ? ' expanded' : ''}`} role="status">
      <div className="wt-dock-main">
        <button
          type="button"
          className="wt-dock-toggle"
          aria-expanded={detailsOpen}
          title={detailsOpen ? undefined : conflictTip}
          onClick={() => setDetailsOpen((v) => !v)}
        >
          <Icon name="alert" size={14} />
          <span className="wt-dock-text">{t('worktreeConflictNotice')}</span>
          {!detailsOpen && <span className="wt-dock-sub hide-mobile">{conflictSummary}</span>}
          <span className="wt-dock-chev" aria-hidden="true">
            <Icon name="chevron" size={12} />
          </span>
        </button>
        <button
          type="button"
          className="btn small primary"
          disabled={!connected || busy || busyTurn}
          title={busyTurn ? t('worktreeSwitchBusyTip') : undefined}
          onClick={() => void enterWorktree()}
        >
          {busy ? <span className="spinner mini" /> : <Icon name="git-branch" size={12} />}
          <span>{t('forkWorktreeAndContinue')}</span>
        </button>
        <button
          type="button"
          className="wt-dock-close"
          disabled={busy}
          title={t('keepSharedDirectory')}
          aria-label={t('keepSharedDirectory')}
          onClick={() => setDismissedConflict(true)}
        >
          <Icon name="x" size={14} />
        </button>
      </div>
      {detailsOpen && (
        <div className="wt-dock-detail">
          <div>{conflictTip}</div>
          {busyTurn && <div className="wt-dock-detail-busy">{t('worktreeSwitchBusyTip')}</div>}
        </div>
      )}
    </div>
  );
}
