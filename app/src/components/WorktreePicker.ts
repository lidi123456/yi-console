import { useEffect, useMemo, useState } from 'react';
import { getT, useLang } from '../i18n';
import { rpc } from '../transport';
import { isHostSession, normDirPath, sessionProjectDir, type SessionInfo } from '../types';
import { basename, shortPath } from '../util';
import type { BranchesResult, WorktreesResult } from './GitOpsDialogs';
import { askText, type MenuItem } from './Overlay';

const tx = getT('newSession');

export type WorktreeChoice =
  | { kind: 'main' }
  | { kind: 'new'; base: string; branch?: string }
  | { kind: 'existing'; path: string; branch: string };

/** Max local base branches listed directly in the Scheme A worktree dropdown */
const MAX_MENU_BRANCHES = 15;

/**
 * Shared Scheme A worktree selector hook for NewSession and Overview QuickStart:
 * - Probes git branches and existing worktrees when `enabled` and `cwd` is a git repo.
 * - Automatically selects `new` (from the current branch) when another session is actively running in the main directory.
 * - Builds native context menu items: Main Dir / New Worktree (by base branch) / Attach Existing Worktree.
 */
export function useWorktreeChoice({
  hostId,
  connected,
  cwd,
  enabled,
  hostSessions,
}: {
  hostId: string;
  connected: boolean;
  cwd: string;
  enabled: boolean;
  hostSessions: SessionInfo[];
}) {
  const [, lang] = useLang();
  const [probe, setProbe] = useState<{
    key: string;
    branches: BranchesResult | null;
    worktrees: WorktreesResult | null;
  } | null>(null);
  const [manual, setManual] = useState<{ key: string; choice: WorktreeChoice } | null>(null);

  const dirKey = `${hostId}::${normDirPath(cwd)}`;

  useEffect(() => {
    const trimmed = cwd.trim();
    if (!enabled || !hostId || !connected || !trimmed) {
      setProbe(null);
      return;
    }
    const reqKey = dirKey;
    let alive = true;
    Promise.all([
      rpc<BranchesResult>(hostId, 'git.branches', { cwd: trimmed }).catch(() => null),
      rpc<WorktreesResult>(hostId, 'git.worktrees', { cwd: trimmed }).catch(() => null),
    ]).then(([br, wt]) => {
      if (!alive) return;
      setProbe({
        key: reqKey,
        branches: br && br.repo ? br : null,
        worktrees: wt && wt.repo ? wt : null,
      });
    });
    return () => {
      alive = false;
    };
  }, [hostId, connected, cwd, enabled, dirKey]);

  const matched = probe && probe.key === dirKey ? probe : null;
  const branches = matched?.branches ?? null;
  const worktrees = matched?.worktrees ?? null;

  const repoRoot = worktrees?.repo || branches?.repo || '';
  const isGitRepo = enabled && !!repoRoot;
  const mainDir = worktrees?.repo || cwd.trim();
  const mainWt = worktrees?.worktrees.find((w) => w.isMain);
  const currentBranch = mainWt?.branch || branches?.current || branches?.defaultBranch || 'HEAD';

  const curLinkedWt = useMemo(
    () => worktrees?.worktrees.find((w) => w.isCurrent && !w.isMain && !w.bare && !w.prunable) ?? null,
    [worktrees],
  );

  const existingWorktrees = useMemo(
    () => (worktrees?.worktrees ?? []).filter((w) => !w.isMain && !w.bare && !w.prunable),
    [worktrees],
  );

  const busyInMain = useMemo(() => {
    if (!isGitRepo) return false;
    const target = normDirPath(mainDir);
    return hostSessions.some(
      (s) =>
        isHostSession(s) &&
        !s.worktree &&
        normDirPath(sessionProjectDir(s)) === target &&
        (s.status === 'running' || s.status === 'starting' || s.status === 'waiting-permission'),
    );
  }, [isGitRepo, mainDir, hostSessions]);

  const userPick = manual && manual.key === dirKey ? manual.choice : null;

  const choice: WorktreeChoice = useMemo(() => {
    if (!isGitRepo) return { kind: 'main' };
    if (userPick) return userPick;
    if (curLinkedWt) {
      return {
        kind: 'existing',
        path: curLinkedWt.path,
        branch: curLinkedWt.branch || basename(curLinkedWt.path),
      };
    }
    if (busyInMain) return { kind: 'new', base: currentBranch };
    return { kind: 'main' };
  }, [isGitRepo, userPick, curLinkedWt, busyInMain, currentBranch]);

  const autoSelected = isGitRepo && !userPick && !curLinkedWt && busyInMain && choice.kind === 'new';

  const chipLabel = useMemo(() => {
    if (choice.kind === 'new') {
      return choice.branch
        ? tx('worktreeChipNewNamed', { branch: choice.branch })
        : tx('worktreeChipNew', { base: choice.base || 'HEAD' });
    }
    if (choice.kind === 'existing') return tx('worktreeChipExisting', { branch: choice.branch });
    return tx('mainDirectory');
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [choice, lang]);

  const branchNames = useMemo(() => {
    const raw = (branches?.branches ?? []).map((b) => b.name);
    const ordered = [currentBranch, ...raw.filter((b) => b !== currentBranch)].filter(
      (b, idx, arr) => b && (b !== 'HEAD' || raw.length === 0) && arr.indexOf(b) === idx,
    );
    return (ordered.length ? ordered : ['HEAD']).slice(0, MAX_MENU_BRANCHES);
  }, [branches, currentBranch]);

  const buildMenuItems = (home?: string): (MenuItem | '-')[] => {
    if (!isGitRepo) return [];
    const pick = (c: WorktreeChoice) => setManual({ key: dirKey, choice: c });
    const items: (MenuItem | '-')[] = [
      {
        label: tx('menuMainDir', { branch: currentBranch }),
        hint: busyInMain ? tx('menuMainBusyHint') : undefined,
        icon: 'folder',
        active: choice.kind === 'main',
        run: () => pick({ kind: 'main' }),
      },
      '-',
      ...branchNames.map((br) => ({
        label: tx('menuNewFromBranch', { branch: br }),
        hint: br === currentBranch && busyInMain ? tx('menuRecommended') : undefined,
        icon: 'git-branch' as const,
        active: choice.kind === 'new' && choice.base === br,
        run: () => pick({ kind: 'new', base: br, branch: choice.kind === 'new' ? choice.branch : undefined }),
      })),
      {
        label: choice.kind === 'new' ? tx('menuEditCustomBranch') : tx('menuNewCustomBranch'),
        hint: choice.kind === 'new' ? choice.branch : undefined,
        icon: 'edit',
        run: () =>
          void (async () => {
            const res = await askText({
              title: tx('customWorktreeBranchTitle'),
              value: choice.kind === 'new' ? (choice.branch ?? '') : '',
              placeholder: tx('customWorktreeBranchPlaceholder'),
              okText: getT('sessionActions')('save'),
            });
            if (res === null) return;
            const b = res.trim();
            if (choice.kind === 'new') {
              pick({ kind: 'new', base: choice.base, branch: b || undefined });
            } else if (b) {
              pick({ kind: 'new', base: currentBranch, branch: b });
            }
          })(),
      },
    ];
    if (existingWorktrees.length > 0) {
      items.push('-');
      for (const wt of existingWorktrees) {
        const label = wt.branch || basename(wt.path);
        items.push({
          label: tx('menuAttachExisting', { branch: label }),
          hint: shortPath(wt.path, home),
          icon: 'git-branch',
          active: choice.kind === 'existing' && normDirPath(choice.path) === normDirPath(wt.path),
          run: () => pick({ kind: 'existing', path: wt.path, branch: label }),
        });
      }
    }
    return items;
  };

  return {
    isGitRepo,
    mainDir,
    currentBranch,
    busyInMain,
    autoSelected,
    choice,
    chipLabel,
    buildMenuItems,
    resetManual: () => setManual(null),
  };
}
