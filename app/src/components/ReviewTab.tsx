import { useEffect, useMemo, useRef, useState } from 'react';
import { useBack } from '../back';
import { getT, isEn, trErr, useLang } from '../i18n';
import { permModes } from '../perm';
import { errorToast, hostStatus, loadSession, registerVisibleChildSession, skey, toast, useSessionData, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type CommitInfo, type SessionInfo, type ToolKind } from '../types';
import { timeAgo } from '../util';
import { ChatMarkdown } from './ChatMarkdown';
import type { BranchesResult } from './GitOpsDialogs';
import { Icon, ToolBadge } from './Icon';
import { INSERT_COMPOSER } from './FilePanel';

const tx = getT('filePanel');
const GIT_HEAD_REF = 'HEAD';

export const REVIEW_PREFIX = 'review:';
export const isReviewTab = (t: string) => t.startsWith(REVIEW_PREFIX);
export const reviewTabSession = (t: string) => t.slice(REVIEW_PREFIX.length);

export type ReviewScope = 'uncommitted' | 'branch' | 'commits' | 'range';
export type UncommittedFilter = 'all' | 'tracked' | 'staged';

export interface ReviewLaunchPreset {
  scope?: ReviewScope;
  uncommittedFilter?: UncommittedFilter;
  base?: string;
  target?: string;
  includeUncommitted?: boolean;
  commits?: string[];
  commitInfos?: CommitInfo[];
  range?: string;
}

export interface ReviewScopeFile {
  path: string;
  status: string;
  added?: number | null;
  deleted?: number | null;
  size?: number | null;
  binary?: boolean;
  untracked?: boolean;
  staged?: boolean;
}

export interface ReviewScopeResult {
  repo: string | null;
  mode: ReviewScope;
  branch?: string;
  uncommittedFilter?: UncommittedFilter;
  counts?: { all: number; tracked: number; staged: number };
  base?: string;
  target?: string;
  targetIsHead?: boolean;
  includeUncommitted?: boolean;
  mergeBase?: string | null;
  baseRev?: string | null;
  endRev?: string | null;
  rangeExpr?: string;
  contiguous?: boolean;
  commitCount?: number | null;
  commits?: CommitInfo[];
  total?: number | null;
  files: ReviewScopeFile[];
  error?: string | null;
}

export interface ReviewMeta {
  sessionId: string;
  parentSessionId: string;
  tool: ToolKind;
  scope: ReviewScope;
  uncommittedFilter?: UncommittedFilter;
  base?: string;
  target?: string;
  includeUncommitted?: boolean;
  mergeBase?: string;
  baseRev?: string;
  endRev?: string;
  commits?: string[];
  contiguous?: boolean;
  rangeExpr?: string;
  scopeLabel?: string;
  includedFilesCount?: number;
  totalFilesCount?: number;
  cwd: string;
}

export interface ReviewIssue {
  id: string;
  severity: 'high' | 'medium' | 'low';
  title: string;
  file?: string;
  line?: number;
  summary: string;
  suggestion?: string;
}

/** In-memory metadata for review tabs opened in this window (fallback to SessionInfo on reload) */
const reviewMetaMap = new Map<string, ReviewMeta>();
export function setReviewMeta(meta: ReviewMeta) {
  reviewMetaMap.set(meta.sessionId, meta);
}
export function getReviewMeta(reviewSid: string): ReviewMeta | undefined {
  return reviewMetaMap.get(reviewSid);
}
export function deleteReviewMeta(reviewSid: string) {
  reviewMetaMap.delete(reviewSid);
}

const LOCK_FILE_RE = /^(package-lock\.json|yarn\.lock|pnpm-lock\.yaml|Cargo\.lock|poetry\.lock|composer\.lock|Gemfile\.lock|go\.sum)$/;

/** Identify binary, very large, lockfile, or minified files for one-click noise exclusion */
export function isLargeOrNoiseFile(f: ReviewScopeFile): boolean {
  if (f.binary) return true;
  if ((f.added ?? 0) + (f.deleted ?? 0) >= 300) return true;
  if ((f.size ?? 0) >= 100 * 1024) return true;
  const base = f.path.slice(f.path.lastIndexOf('/') + 1);
  return LOCK_FILE_RE.test(base) || /\.min\.(js|css)$/i.test(base) || /\.map$/i.test(base);
}

function statusBadge(status: string): [string, string] {
  if (status === 'new' || status === '??') return ['U', 'st-new'];
  if (status.includes('D')) return ['D', 'st-d'];
  if (status.includes('A')) return ['A', 'st-a'];
  if (status.includes('R')) return ['R', 'st-m'];
  return ['M', 'st-m'];
}

/** Read-only permission mode for a tool (plan for all tools that support it, else default) */
export function readonlyPermFor(tool: ToolKind): string {
  return permModes(tool, 'structured').some((m) => m[0] === 'plan') ? 'plan' : 'default';
}

/** Default reviewer tool: prefer a different installed AI tool than the current session's tool (Claude <-> Codex first) */
export function pickReviewerTool(hostId: string, currentTool?: ToolKind): ToolKind | null {
  const tools = (hostStatus(hostId).hello?.tools ?? []).filter(
    (t) => t.available && t.kind !== 'shell' && t.kind !== 'files',
  );
  if (!tools.length) return null;
  if (currentTool === 'claude') {
    const codex = tools.find((t) => t.kind === 'codex');
    if (codex) return codex.kind;
  }
  if (currentTool === 'codex') {
    const claude = tools.find((t) => t.kind === 'claude');
    if (claude) return claude.kind;
  }
  const other = tools.find((t) => t.kind !== currentTool);
  return (other ?? tools[0]).kind;
}

/** Build the structured read-only code review prompt sent to the reviewer session */
export function buildReviewPrompt(opts: {
  scope: ReviewScope;
  uncommittedFilter?: UncommittedFilter;
  base?: string;
  target?: string;
  includeUncommitted?: boolean;
  mergeBase?: string;
  baseRev?: string;
  endRev?: string;
  commits?: string[];
  contiguous?: boolean;
  rangeExpr?: string;
  includedFiles?: string[];
  excludedFiles?: string[];
  focus?: string;
  filesCount: number;
  zh: boolean;
}): string {
  let diffCmd = 'git diff HEAD (and untracked files via git status --short)';
  let scopeDesc = 'all uncommitted working-tree changes relative to HEAD';

  if (opts.scope === 'uncommitted') {
    if (opts.uncommittedFilter === 'staged') {
      diffCmd = 'git diff --cached';
      scopeDesc = 'staged uncommitted changes relative to HEAD';
    } else if (opts.uncommittedFilter === 'tracked') {
      diffCmd = 'git diff HEAD';
      scopeDesc = 'tracked uncommitted working-tree changes relative to HEAD (excluding untracked files)';
    }
  } else if (opts.scope === 'branch') {
    const pairLabel =
      opts.base && opts.target
        ? `from ${opts.base} to ${opts.target}`
        : opts.base
          ? `relative to ${opts.base}`
          : 'on the current branch';
    if (!opts.includeUncommitted && opts.baseRev && opts.endRev) {
      diffCmd = `git diff ${opts.baseRev} ${opts.endRev}`;
      scopeDesc = `committed branch changes ${pairLabel} (excluding uncommitted working-tree files)`;
    } else {
      const mb = opts.mergeBase ?? opts.baseRev;
      diffCmd = mb ? `git diff ${mb} (and untracked files via git status --short)` : 'git diff HEAD (and untracked files via git status --short)';
      scopeDesc = `branch + working-tree changes ${pairLabel}`;
    }
  } else if (opts.scope === 'commits') {
    const shortList = (opts.commits ?? []).map((h) => h.slice(0, 7)).join(', ');
    if (opts.contiguous && opts.baseRev && opts.endRev) {
      diffCmd = `git diff ${opts.baseRev} ${opts.endRev}`;
      scopeDesc = `changes introduced by commit(s) ${shortList || opts.endRev.slice(0, 7)}`;
    } else {
      diffCmd = `git show ${(opts.commits ?? ['HEAD']).join(' ')}`;
      scopeDesc = `changes across selected commits (${shortList})`;
    }
  } else if (opts.scope === 'range') {
    const expr =
      opts.rangeExpr ||
      (opts.baseRev && opts.endRev ? `${opts.baseRev.slice(0, 7)}..${opts.endRev.slice(0, 7)}` : 'HEAD');
    diffCmd = opts.baseRev && opts.endRev ? `git diff ${opts.baseRev} ${opts.endRev}` : `git diff ${expr}`;
    scopeDesc = `changes in range ${expr}`;
  }

  let fileScopeBlock = '';
  if (opts.excludedFiles && opts.excludedFiles.length > 0) {
    const inc = opts.includedFiles ?? [];
    if (inc.length > 0 && inc.length <= 40) {
      fileScopeBlock = [
        `IMPORTANT: The user excluded ${opts.excludedFiles.length} file(s). Review ONLY these ${inc.length} file(s) and ignore all others:`,
        ...inc.map((p) => `- ${p}`),
      ].join('\n');
    } else if (opts.excludedFiles.length <= 40) {
      fileScopeBlock = [
        `IMPORTANT: Skip and DO NOT review these ${opts.excludedFiles.length} excluded file(s) (you can exclude them with \`:!<path>\` pathspecs):`,
        ...opts.excludedFiles.map((p) => `- ${p}`),
      ].join('\n');
    } else if (inc.length > 0) {
      fileScopeBlock = [
        `IMPORTANT: The user excluded ${opts.excludedFiles.length} file(s). Review ONLY the ${inc.length} included file(s) (first 40 shown below) and skip excluded files:`,
        ...inc.slice(0, 40).map((p) => `- ${p}`),
        `- ...and ${inc.length - 40} more included file(s)`,
      ].join('\n');
    }
  }

  const langRule = opts.zh ? 'Write all titles, summaries, and suggestions in concise Chinese.' : 'Write all titles, summaries, and suggestions in concise English.';
  const focusLine = opts.focus?.trim() ? `\nExtra review focus from the user: ${opts.focus.trim()}` : '';
  return [
    `You are performing a READ-ONLY code review of ${scopeDesc} (${opts.filesCount} changed files).`,
    `DO NOT edit, write, or commit any files. Inspect the changes using read-only commands such as \`${diffCmd}\` and read surrounding code as needed.`,
    fileScopeBlock,
    `Focus on real bugs, logic errors, security/safety issues, race conditions, regressions, and edge cases. Skip trivial style nitpicks unless important.`,
    langRule,
    focusLine,
    '',
    'Provide a brief summary first, then ALWAYS end your response with a fenced JSON block tagged `yi-review-json` containing an array of findings (or `[]` if no issues found):',
    '```yi-review-json',
    '[',
    '  {',
    '    "severity": "high" | "medium" | "low",',
    '    "title": "short issue title",',
    '    "file": "repo/relative/path.ext",',
    '    "line": 42,',
    '    "summary": "why this is a problem",',
    '    "suggestion": "concrete fix"',
    '  }',
    ']',
    '```',
  ]
    .filter((l, i, arr) => l !== '' || (i > 0 && arr[i - 1] !== ''))
    .join('\n');
}

/** Parse structured findings from a reviewer's markdown output (returns null if no valid JSON block found) */
export function parseReviewIssues(text: string): { issues: ReviewIssue[]; prose: string } | null {
  if (!text) return null;
  const match = /```yi-review-json\s*\n([\s\S]*?)```/i.exec(text) ?? /```json\s*\n(\s*\[[\s\S]*?\])\s*```/i.exec(text);
  if (!match) return null;
  try {
    const raw = JSON.parse(match[1].trim());
    if (!Array.isArray(raw)) return null;
    const issues: ReviewIssue[] = [];
    for (let i = 0; i < raw.length; i++) {
      const it = raw[i];
      if (!it || typeof it !== 'object') continue;
      const sevRaw = String(it.severity ?? 'medium').toLowerCase();
      const severity: 'high' | 'medium' | 'low' =
        sevRaw === 'high' || sevRaw === 'critical' || sevRaw === 'error'
          ? 'high'
          : sevRaw === 'low' || sevRaw === 'nit' || sevRaw === 'info'
            ? 'low'
            : 'medium';
      const title = String(it.title ?? '').trim();
      const summary = String(it.summary ?? it.description ?? '').trim();
      if (!title && !summary) continue;
      const lineNum = Number(it.line);
      issues.push({
        id: `ri-${i}`,
        severity,
        title: title || summary.slice(0, 60),
        file: typeof it.file === 'string' && it.file.trim() ? it.file.trim() : undefined,
        line: Number.isFinite(lineNum) && lineNum > 0 ? Math.floor(lineNum) : undefined,
        summary,
        suggestion: typeof it.suggestion === 'string' && it.suggestion.trim() ? it.suggestion.trim() : undefined,
      });
    }
    const prose = text.replace(match[0], '').trim();
    return { issues, prose };
  } catch {
    return null;
  }
}

export function ReviewLaunchDialog({
  hostId,
  cwd,
  parentSessionId,
  currentTool,
  currentBranch,
  branchesInfo,
  defaultScope,
  branchBase,
  branchMergeBase,
  uncommittedCount,
  branchCount,
  preset,
  onStarted,
  onClose,
}: {
  hostId: string;
  cwd: string;
  parentSessionId: string;
  currentTool?: ToolKind;
  currentBranch?: string;
  branchesInfo?: BranchesResult | null;
  defaultScope: ReviewScope;
  branchBase?: string;
  branchMergeBase?: string;
  uncommittedCount: number;
  branchCount: number;
  preset?: ReviewLaunchPreset | null;
  onStarted: (reviewSessionId: string) => void;
  onClose: () => void;
}) {
  const [, lang] = useLang();
  useBack(true, onClose);
  const tools = useMemo(
    () => (hostStatus(hostId).hello?.tools ?? []).filter((t) => t.available && t.kind !== 'shell' && t.kind !== 'files'),
    [hostId],
  );
  const [tool, setTool] = useState<ToolKind>(() => pickReviewerTool(hostId, currentTool) ?? (tools[0]?.kind ?? 'claude'));
  const [scope, setScope] = useState<ReviewScope>(() => preset?.scope ?? defaultScope);
  const [uncommittedFilter, setUncommittedFilter] = useState<UncommittedFilter>(() => preset?.uncommittedFilter ?? 'all');
  const headTarget = currentBranch && currentBranch !== 'HEAD' ? currentBranch : 'HEAD';
  const [base, setBase] = useState<string>(() => preset?.base ?? branchBase ?? branchesInfo?.defaultBranch ?? '');
  const [target, setTarget] = useState<string>(() => preset?.target ?? headTarget);
  const [includeUncommitted, setIncludeUncommitted] = useState<boolean>(() => preset?.includeUncommitted ?? false);
  const [branchPicker, setBranchPicker] = useState<'base' | 'target' | null>(null);
  const [branchSearch, setBranchSearch] = useState('');

  const [selectedCommits, setSelectedCommits] = useState<string[]>(() => preset?.commits ?? []);
  const [commitMetaMap, setCommitMetaMap] = useState<Record<string, CommitInfo>>(() => {
    const m: Record<string, CommitInfo> = {};
    for (const c of preset?.commitInfos ?? []) m[c.hash] = c;
    return m;
  });
  const [commitPickerMode, setCommitPickerMode] = useState<'single' | 'multi' | null>(null);
  const [commitSearch, setCommitSearch] = useState('');
  const [debouncedCommitSearch, setDebouncedCommitSearch] = useState('');
  const [recentCommits, setRecentCommits] = useState<CommitInfo[]>([]);
  const [searchedCommits, setSearchedCommits] = useState<CommitInfo[]>([]);
  const [commitsLoading, setCommitsLoading] = useState(false);

  const [rangeInput, setRangeInput] = useState<string>(() => preset?.range ?? '');
  const [debouncedRange, setDebouncedRange] = useState<string>(() => preset?.range ?? '');

  const [uncommittedCounts, setUncommittedCounts] = useState<{ all: number; tracked: number; staged: number } | null>(null);
  const [scopeRes, setScopeRes] = useState<ReviewScopeResult | null>(null);
  const [scopeLoading, setScopeLoading] = useState(false);
  const [legacyDaemon, setLegacyDaemon] = useState(false);

  const [filesExpanded, setFilesExpanded] = useState(false);
  const [excluded, setExcluded] = useState<Set<string>>(() => new Set());
  const [fileFilter, setFileFilter] = useState('');
  const [fileLimit, setFileLimit] = useState(150);

  const [focus, setFocus] = useState('');
  const [starting, setStarting] = useState(false);
  const scopeReqId = useRef(0);
  const commitReqId = useRef(0);

  // Backfill base branch if branchesInfo arrives after dialog opens
  useEffect(() => {
    if (!base && branchesInfo?.defaultBranch) {
      setBase(branchesInfo.defaultBranch);
    }
  }, [base, branchesInfo?.defaultBranch]);

  // Debounce custom range typing
  useEffect(() => {
    const t = setTimeout(() => setDebouncedRange(rangeInput), 260);
    return () => clearTimeout(t);
  }, [rangeInput]);

  // Debounce commit search typing
  useEffect(() => {
    const t = setTimeout(() => setDebouncedCommitSearch(commitSearch), 220);
    return () => clearTimeout(t);
  }, [commitSearch]);

  // Load recent commits when entering commits mode
  useEffect(() => {
    if (scope !== 'commits' || recentCommits.length > 0) return;
    setCommitsLoading(true);
    rpc<{ commits: CommitInfo[] }>(hostId, 'git.log', { cwd, skip: 0, limit: 50 })
      .then((r) => {
        const list = r.commits ?? [];
        setRecentCommits(list);
        setCommitMetaMap((cur) => {
          const next = { ...cur };
          for (const c of list) next[c.hash] = c;
          return next;
        });
      })
      .catch(() => {})
      .finally(() => setCommitsLoading(false));
  }, [scope, hostId, cwd, recentCommits.length]);

  // Search commit history on server when user types a query
  useEffect(() => {
    const q = debouncedCommitSearch.trim();
    if (commitPickerMode === null || !q) {
      setSearchedCommits([]);
      return;
    }
    const reqId = ++commitReqId.current;
    setCommitsLoading(true);
    rpc<{ commits: CommitInfo[] }>(hostId, 'git.log', { cwd, skip: 0, limit: 50, query: q })
      .then((r) => {
        if (reqId !== commitReqId.current) return;
        const list = r.commits ?? [];
        setSearchedCommits(list);
        setCommitMetaMap((cur) => {
          const next = { ...cur };
          for (const c of list) next[c.hash] = c;
          return next;
        });
      })
      .catch(() => {})
      .finally(() => {
        if (reqId === commitReqId.current) setCommitsLoading(false);
      });
  }, [commitPickerMode, debouncedCommitSearch, hostId, cwd]);

  const commitsKey = selectedCommits.join(',');
  const scopeKey =
    scope === 'uncommitted'
      ? `uncommitted:${uncommittedFilter}`
      : scope === 'branch'
        ? `branch:${base}:${target}:${includeUncommitted ? 1 : 0}`
        : scope === 'commits'
          ? `commits:${commitsKey}`
          : `range:${debouncedRange.trim()}`;

  // Reset manual exclusions whenever the active review scope changes
  useEffect(() => {
    setExcluded(new Set());
    setFileFilter('');
    setFileLimit(150);
  }, [scopeKey]);

  // Fetch file list and stats for the active scope
  useEffect(() => {
    if (scope === 'commits' && selectedCommits.length === 0) {
      scopeReqId.current++;
      setScopeRes(null);
      setScopeLoading(false);
      return;
    }
    if (scope === 'range' && !debouncedRange.trim()) {
      scopeReqId.current++;
      setScopeRes(null);
      setScopeLoading(false);
      return;
    }
    const reqId = ++scopeReqId.current;
    setScopeRes((cur) => (cur && cur.mode === scope && !cur.error ? cur : null));
    setScopeLoading(true);
    const params: Record<string, unknown> = { cwd, mode: scope, diff: false };
    if (scope === 'uncommitted') {
      params.uncommittedFilter = uncommittedFilter;
    } else if (scope === 'branch') {
      if (base.trim()) params.base = base.trim();
      if (target.trim()) params.target = target.trim();
      params.includeUncommitted = includeUncommitted;
    } else if (scope === 'commits') {
      params.commits = selectedCommits;
    } else if (scope === 'range') {
      params.range = debouncedRange.trim();
    }

    rpc<ReviewScopeResult>(hostId, 'git.reviewScope', params)
      .then((r) => {
        if (reqId !== scopeReqId.current) return;
        setLegacyDaemon(false);
        setScopeRes(r);
        setScopeLoading(false);
        if (r.counts) setUncommittedCounts(r.counts);
        if (r.commits?.length) {
          setCommitMetaMap((cur) => {
            const next = { ...cur };
            for (const c of r.commits!) next[c.hash] = c;
            return next;
          });
        }
      })
      .catch((e) => {
        if (reqId !== scopeReqId.current) return;
        const msg = String((e as Error)?.message ?? e ?? '');
        if (/unknown|not found|未知|不支持的方法|-32601/i.test(msg)) {
          setLegacyDaemon(true);
          setScopeRes(null);
        } else {
          setScopeRes({ repo: null, mode: scope, files: [], error: msg });
        }
        setScopeLoading(false);
      });
  }, [hostId, cwd, scope, uncommittedFilter, base, target, includeUncommitted, commitsKey, debouncedRange]); // eslint-disable-line react-hooks/exhaustive-deps

  const targetIsHead = target === 'HEAD' || (!!currentBranch && currentBranch !== 'HEAD' && target === currentBranch);
  const scopeFiles = scopeRes?.files ?? [];
  const includedFiles = useMemo(() => scopeFiles.filter((f) => !excluded.has(f.path)), [scopeFiles, excluded]);
  const excludedFiles = useMemo(() => scopeFiles.filter((f) => excluded.has(f.path)), [scopeFiles, excluded]);

  const includedTotals = useMemo(() => {
    let added = 0;
    let deleted = 0;
    let any = false;
    for (const f of includedFiles) {
      if (f.added != null || f.deleted != null) any = true;
      added += f.added ?? 0;
      deleted += f.deleted ?? 0;
    }
    return { added, deleted, any };
  }, [includedFiles]);

  const filteredPreviewFiles = useMemo(() => {
    const q = fileFilter.trim().toLowerCase();
    if (!q) return scopeFiles;
    return scopeFiles.filter((f) => f.path.toLowerCase().includes(q));
  }, [scopeFiles, fileFilter]);

  const { filteredLocalBranches, filteredRemoteBranches } = useMemo(() => {
    const q = branchSearch.trim().toLowerCase();
    const local = (branchesInfo?.branches ?? []).filter((b) => !q || b.name.toLowerCase().includes(q));
    const remote = (branchesInfo?.remoteBranches ?? []).filter((name) => !q || name.toLowerCase().includes(q));
    return { filteredLocalBranches: local, filteredRemoteBranches: remote };
  }, [branchesInfo, branchSearch]);

  const displayedCommits = debouncedCommitSearch.trim() ? searchedCommits : recentCommits;
  const typedHex = commitSearch.trim();
  const canAddDirectHash =
    /^[0-9a-f]{4,40}$/i.test(typedHex) &&
    !displayedCommits.some((c) => c.hash.toLowerCase().startsWith(typedHex.toLowerCase()));

  const legacyFilesCount = scope === 'branch' && branchMergeBase ? branchCount : uncommittedCount;
  const effectiveFilesCount = legacyDaemon ? legacyFilesCount : includedFiles.length;
  const rangePending = scope === 'range' && rangeInput.trim() !== debouncedRange.trim();

  const canStart =
    !starting &&
    tools.length > 0 &&
    !scopeLoading &&
    !rangePending &&
    !scopeRes?.error &&
    (legacyDaemon
      ? scope === 'uncommitted' || (scope === 'branch' && !!branchMergeBase)
      : (scope !== 'commits' || selectedCommits.length > 0) &&
        (scope !== 'range' || !!debouncedRange.trim()) &&
        includedFiles.length > 0);

  const toggleFile = (path: string) => {
    setExcluded((cur) => {
      const next = new Set(cur);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  };

  const excludeLargeFiles = () => {
    const matches = scopeFiles.filter((f) => isLargeOrNoiseFile(f));
    if (!matches.length) {
      toast(tx('reviewNoLargeFilesFound'));
      return;
    }
    setExcluded((cur) => {
      const next = new Set(cur);
      for (const m of matches) next.add(m.path);
      return next;
    });
    setFilesExpanded(true);
    toast(tx('reviewLargeFilesExcluded', { n: matches.length }));
  };

  const excludeMatchedFiles = () => {
    if (!filteredPreviewFiles.length) return;
    setExcluded((cur) => {
      const next = new Set(cur);
      for (const f of filteredPreviewFiles) next.add(f.path);
      return next;
    });
  };

  const pickBranchRef = (name: string) => {
    if (branchPicker === 'base') setBase(name);
    else if (branchPicker === 'target') setTarget(name);
    setBranchPicker(null);
    setBranchSearch('');
  };

  const pickCommitItem = (hash: string) => {
    if (commitPickerMode === 'multi') {
      setSelectedCommits((cur) => (cur.includes(hash) ? cur.filter((x) => x !== hash) : [...cur, hash]));
    } else {
      setSelectedCommits([hash]);
      setCommitPickerMode(null);
      setCommitSearch('');
    }
  };

  const pickRecentN = (n: number) => {
    const hashes = recentCommits.slice(0, n).map((c) => c.hash);
    if (!hashes.length) return;
    setSelectedCommits(hashes);
    setCommitPickerMode(null);
    setCommitSearch('');
  };

  const scopeLabel = useMemo(() => {
    if (scope === 'uncommitted') {
      if (uncommittedFilter === 'tracked') return tx('reviewScopeUncommittedTracked');
      if (uncommittedFilter === 'staged') return tx('reviewScopeUncommittedStaged');
      return tx('diffScopeUncommitted');
    }
    if (scope === 'branch') {
      const b = scopeRes?.base || base || branchBase || 'HEAD';
      const t = scopeRes?.target || target || headTarget;
      return `${b} ← ${t}${includeUncommitted && targetIsHead ? ` + ${tx('diffScopeUncommitted')}` : ''}`;
    }
    if (scope === 'commits') {
      if (selectedCommits.length === 1) {
        return tx('reviewScopeCommitSingle', { short: selectedCommits[0].slice(0, 7) });
      }
      return tx('reviewScopeCommitsMulti', { n: selectedCommits.length });
    }
    return debouncedRange.trim() || tx('reviewModeRange');
  }, [scope, uncommittedFilter, scopeRes?.base, scopeRes?.target, base, branchBase, target, headTarget, includeUncommitted, targetIsHead, selectedCommits, debouncedRange, lang]);

  const start = async () => {
    if (!canStart) return;
    setStarting(true);
    try {
      const effScope: ReviewScope = legacyDaemon
        ? scope === 'branch' && branchMergeBase
          ? 'branch'
          : 'uncommitted'
        : scope;
      const perm = readonlyPermFor(tool);
      const created = await rpc<SessionInfo>(hostId, 'session.create', {
        tool,
        cwd,
        mode: 'structured',
        parentId: parentSessionId,
        permissionMode: perm,
        title: `Review (${TOOL_LABEL[tool]})`,
      });
      const resolvedBase = scopeRes?.base || base || branchBase;
      const resolvedTarget = scopeRes?.target || target || headTarget;
      const resolvedCommits = scopeRes?.commits?.length ? scopeRes.commits.map((c) => c.hash) : selectedCommits;
      setReviewMeta({
        sessionId: created.id,
        parentSessionId,
        tool,
        scope: effScope,
        uncommittedFilter: effScope === 'uncommitted' ? uncommittedFilter : undefined,
        base: resolvedBase,
        target: effScope === 'branch' ? resolvedTarget : undefined,
        includeUncommitted: effScope === 'branch' ? (scopeRes?.includeUncommitted ?? false) : undefined,
        mergeBase: scopeRes?.mergeBase ?? branchMergeBase,
        baseRev: scopeRes?.baseRev ?? undefined,
        endRev: scopeRes?.endRev ?? undefined,
        commits: effScope === 'commits' ? resolvedCommits : undefined,
        contiguous: scopeRes?.contiguous,
        rangeExpr: effScope === 'range' ? debouncedRange.trim() : undefined,
        scopeLabel,
        includedFilesCount: legacyDaemon ? undefined : includedFiles.length,
        totalFilesCount: legacyDaemon ? undefined : scopeFiles.length,
        cwd,
      });
      const prompt = buildReviewPrompt({
        scope: effScope,
        uncommittedFilter,
        base: resolvedBase,
        target: resolvedTarget,
        includeUncommitted: legacyDaemon ? true : (scopeRes?.includeUncommitted ?? false),
        mergeBase: scopeRes?.mergeBase ?? branchMergeBase,
        baseRev: scopeRes?.baseRev ?? undefined,
        endRev: scopeRes?.endRev ?? undefined,
        commits: resolvedCommits,
        contiguous: scopeRes?.contiguous,
        rangeExpr: debouncedRange.trim(),
        includedFiles: legacyDaemon ? undefined : includedFiles.map((f) => f.path),
        excludedFiles: legacyDaemon ? undefined : excludedFiles.map((f) => f.path),
        focus,
        filesCount: effectiveFilesCount,
        zh: !isEn(),
      });
      await rpc(hostId, 'session.send', { sessionId: created.id, text: prompt, queue: true });
      onStarted(created.id);
      onClose();
    } catch (e) {
      errorToast(e);
      setStarting(false);
    }
  };

  const uncommittedBadgeCount = uncommittedCounts?.all ?? uncommittedCount;
  const previewNames = includedFiles
    .slice(0, 3)
    .map((f) => f.path.slice(f.path.lastIndexOf('/') + 1))
    .join(', ');
  const defaultBaseCand = branchesInfo?.defaultBranch;

  return (
    <div className="modal-bg" onClick={onClose}>
      <div className="modal wp-review-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <div className="row">
            <Icon name="sparkles" size={16} />
            <h3>{tx('crossToolReviewTitle')}</h3>
          </div>
          <button type="button" className="icon-btn" onClick={onClose} title={tx('close')}>
            <Icon name="x" size={15} />
          </button>
        </div>

        <div className="wp-review-form">
          <div className="wp-review-readonly">
            <Icon name="shield" size={13} />
            <span>{tx('reviewReadonlyNotice')}</span>
          </div>

          <div className="wp-review-field">
            <span>{tx('reviewToolLabel')}</span>
            <div className="wp-review-tools">
              {tools.map((t) => (
                <button
                  key={t.kind}
                  type="button"
                  className={`wp-review-tool ${tool === t.kind ? 'active' : ''}`}
                  onClick={() => setTool(t.kind)}
                >
                  <ToolBadge tool={t.kind} size={15} />
                  <span>{TOOL_LABEL[t.kind]}</span>
                  {currentTool && t.kind !== currentTool && <span className="wp-review-badge">{tx('crossToolBadge')}</span>}
                </button>
              ))}
            </div>
          </div>

          <div className="wp-review-scope-box">
            <div className="wp-review-scope-top">
              <span>{tx('reviewScopeLabel')}</span>
              <span className="muted small">{tx('reviewScopeHintOptionB')}</span>
            </div>

            <div className="wp-review-mode-tabs">
              <button
                type="button"
                className={`wp-review-mode-tab ${scope === 'uncommitted' ? 'active' : ''}`}
                onClick={() => {
                  setScope('uncommitted');
                  setBranchPicker(null);
                  setCommitPickerMode(null);
                }}
              >
                <span>{tx('reviewModeUncommitted')}</span>
                <span className="wp-review-count">{uncommittedBadgeCount}</span>
              </button>
              <button
                type="button"
                className={`wp-review-mode-tab ${scope === 'branch' ? 'active' : ''}`}
                onClick={() => {
                  setScope('branch');
                  setCommitPickerMode(null);
                }}
              >
                <span>{tx('reviewModeBranch')}</span>
              </button>
              <button
                type="button"
                className={`wp-review-mode-tab ${scope === 'commits' ? 'active' : ''}`}
                disabled={legacyDaemon}
                onClick={() => {
                  setScope('commits');
                  setBranchPicker(null);
                  if (selectedCommits.length === 0) setCommitPickerMode('single');
                }}
              >
                <span>{tx('reviewModeCommits')}</span>
              </button>
              <button
                type="button"
                className={`wp-review-mode-tab ${scope === 'range' ? 'active' : ''}`}
                disabled={legacyDaemon}
                onClick={() => {
                  setScope('range');
                  setBranchPicker(null);
                  setCommitPickerMode(null);
                }}
              >
                <span>{tx('reviewModeRange')}</span>
              </button>
            </div>

            {scope === 'uncommitted' && !legacyDaemon && (
              <div className="wp-review-subfilters">
                <button
                  type="button"
                  className={`wp-review-chip ${uncommittedFilter === 'all' ? 'active' : ''}`}
                  onClick={() => setUncommittedFilter('all')}
                >
                  <span>{tx('reviewFilterAll')}</span>
                  {uncommittedCounts && <span className="wp-review-count">{uncommittedCounts.all}</span>}
                </button>
                <button
                  type="button"
                  className={`wp-review-chip ${uncommittedFilter === 'tracked' ? 'active' : ''}`}
                  onClick={() => setUncommittedFilter('tracked')}
                >
                  <span>{tx('reviewFilterTracked')}</span>
                  {uncommittedCounts && <span className="wp-review-count">{uncommittedCounts.tracked}</span>}
                </button>
                <button
                  type="button"
                  className={`wp-review-chip ${uncommittedFilter === 'staged' ? 'active' : ''}`}
                  onClick={() => setUncommittedFilter('staged')}
                >
                  <span>{tx('reviewFilterStaged')}</span>
                  {uncommittedCounts && <span className="wp-review-count">{uncommittedCounts.staged}</span>}
                </button>
              </div>
            )}

            {scope === 'branch' && (
              <>
                <div className="wp-review-compare-bar">
                  <button
                    type="button"
                    className={`wp-review-ref-btn ${branchPicker === 'base' ? 'active' : ''}`}
                    onClick={() => {
                      setBranchSearch('');
                      setBranchPicker((v) => (v === 'base' ? null : 'base'));
                    }}
                    title={base || branchesInfo?.defaultBranch || 'HEAD'}
                  >
                    <span className="muted">{tx('reviewBaseLabel')}</span>
                    <span className="mono truncate">{base || branchesInfo?.defaultBranch || '…'}</span>
                    <Icon name="down" size={12} />
                  </button>
                  <button
                    type="button"
                    className="wp-review-swap-btn"
                    title={tx('reviewSwapBranches')}
                    disabled={!base || !target || base === target}
                    onClick={() => {
                      const prevBase = base;
                      setBase(target);
                      setTarget(prevBase);
                      setBranchPicker(null);
                    }}
                  >
                    <Icon name="back" size={13} />
                  </button>
                  <button
                    type="button"
                    className={`wp-review-ref-btn ${branchPicker === 'target' ? 'active' : ''}`}
                    onClick={() => {
                      setBranchSearch('');
                      setBranchPicker((v) => (v === 'target' ? null : 'target'));
                    }}
                    title={target}
                  >
                    <span className="muted">{tx('reviewTargetLabel')}</span>
                    <span className="mono truncate">{target}</span>
                    <Icon name="down" size={12} />
                  </button>
                  <label
                    className={`wp-review-uncomm-toggle ${!targetIsHead ? 'disabled' : ''} ${includeUncommitted && targetIsHead ? 'active' : ''}`}
                    title={!targetIsHead ? tx('reviewIncludeUncommittedDisabledTip') : undefined}
                  >
                    <input
                      type="checkbox"
                      checked={targetIsHead && includeUncommitted}
                      disabled={!targetIsHead}
                      onChange={(e) => setIncludeUncommitted(e.target.checked)}
                    />
                    <span>{tx('reviewIncludeUncommitted')}</span>
                  </label>
                </div>

                {branchPicker && (
                  <div className="wp-review-picker">
                    <div className="wp-review-picker-search">
                      <Icon name="search" size={13} />
                      <input
                        autoFocus
                        value={branchSearch}
                        placeholder={tx('reviewSearchBranch')}
                        onChange={(e) => setBranchSearch(e.target.value)}
                        onKeyDown={(e) => {
                          if (e.key === 'Escape' && !e.nativeEvent.isComposing) {
                            e.stopPropagation();
                            setBranchPicker(null);
                          } else if (e.key === 'Enter' && !e.nativeEvent.isComposing && branchSearch.trim()) {
                            e.preventDefault();
                            const first = filteredLocalBranches[0]?.name ?? filteredRemoteBranches[0] ?? branchSearch.trim();
                            pickBranchRef(first);
                          }
                        }}
                      />
                      {branchSearch && (
                        <button type="button" className="icon-btn" title={tx('clear')} onClick={() => setBranchSearch('')}>
                          <Icon name="x" size={12} />
                        </button>
                      )}
                    </div>
                    <div className="wp-review-picker-list">
                      {filteredLocalBranches.length > 0 && (
                        <>
                          <div className="wp-review-picker-sec">{tx('localBranchesSection', { count: filteredLocalBranches.length })}</div>
                          {filteredLocalBranches.map((b) => {
                            const active = (branchPicker === 'base' ? base : target) === b.name;
                            return (
                              <button
                                key={b.name}
                                type="button"
                                className={`wp-review-picker-row ${active ? 'active' : ''}`}
                                onClick={() => pickBranchRef(b.name)}
                              >
                                <Icon name="git-branch" size={12} />
                                <span className="mono truncate grow">{b.name}</span>
                                {b.name === branchesInfo?.defaultBranch && <span className="wp-bw-tag">{tx('defaultBranchTag')}</span>}
                                {b.current && <span className="wp-bw-tag">{GIT_HEAD_REF}</span>}
                                {active && <Icon name="check" size={12} />}
                              </button>
                            );
                          })}
                        </>
                      )}
                      {filteredRemoteBranches.length > 0 && (
                        <>
                          <div className="wp-review-picker-sec">{tx('remoteBranchesSection', { count: filteredRemoteBranches.length })}</div>
                          {filteredRemoteBranches.map((name) => {
                            const active = (branchPicker === 'base' ? base : target) === name;
                            return (
                              <button
                                key={name}
                                type="button"
                                className={`wp-review-picker-row ${active ? 'active' : ''}`}
                                onClick={() => pickBranchRef(name)}
                              >
                                <Icon name="git-branch" size={12} />
                                <span className="mono truncate grow">{name}</span>
                                {active && <Icon name="check" size={12} />}
                              </button>
                            );
                          })}
                        </>
                      )}
                      {filteredLocalBranches.length === 0 && filteredRemoteBranches.length === 0 && (
                        branchSearch.trim() ? (
                          <button
                            type="button"
                            className="wp-review-picker-row"
                            onClick={() => pickBranchRef(branchSearch.trim())}
                          >
                            <Icon name="git-branch" size={12} />
                            <span className="mono truncate grow">{tx('reviewUseBranchRef', { ref: branchSearch.trim() })}</span>
                          </button>
                        ) : (
                          <div className="wp-review-picker-empty">{tx('noMatchingBranches')}</div>
                        )
                      )}
                    </div>
                  </div>
                )}
              </>
            )}

            {scope === 'commits' && (
              <>
                <div className="wp-review-commit-bar">
                  <button
                    type="button"
                    className={`wp-review-commit-sel ${commitPickerMode === 'single' ? 'active' : ''}`}
                    onClick={() => setCommitPickerMode((m) => (m === 'single' ? null : 'single'))}
                  >
                    <Icon name="git-commit" size={13} />
                    {selectedCommits.length === 0 ? (
                      <span className="muted truncate grow">{tx('reviewSelectCommitPlaceholder')}</span>
                    ) : selectedCommits.length === 1 ? (
                      <>
                        <span className="wp-review-hash-pill mono">
                          {commitMetaMap[selectedCommits[0]]?.short ?? selectedCommits[0].slice(0, 7)}
                        </span>
                        <span className="truncate grow">
                          {commitMetaMap[selectedCommits[0]]?.subject || selectedCommits[0]}
                        </span>
                      </>
                    ) : (
                      <>
                        <span className="wp-review-hash-pill mono">
                          {commitMetaMap[selectedCommits[0]]?.short ?? selectedCommits[0].slice(0, 7)}
                        </span>
                        <span className="truncate grow">{tx('reviewCommitsSelectedSummary', { n: selectedCommits.length })}</span>
                      </>
                    )}
                    <Icon name={commitPickerMode === 'single' ? 'up' : 'down'} size={12} />
                  </button>
                  <button
                    type="button"
                    className={`wp-review-add-commit-btn ${commitPickerMode === 'multi' ? 'active' : ''}`}
                    onClick={() => setCommitPickerMode((m) => (m === 'multi' ? null : 'multi'))}
                  >
                    {commitPickerMode === 'multi' ? tx('reviewDoneSelecting') : tx('reviewAddCommit')}
                  </button>
                </div>

                {selectedCommits.length > 1 && (
                  <div className="wp-review-commit-chips">
                    {selectedCommits.map((h) => {
                      const info = commitMetaMap[h];
                      return (
                        <span key={h} className="wp-review-commit-chip" title={info?.subject ?? h}>
                          <span className="mono">{info?.short ?? h.slice(0, 7)}</span>
                          <button
                            type="button"
                            title={tx('reviewRemoveCommit')}
                            onClick={() => setSelectedCommits((cur) => cur.filter((x) => x !== h))}
                          >
                            <Icon name="x" size={11} />
                          </button>
                        </span>
                      );
                    })}
                    <button type="button" className="wp-review-link" onClick={() => setSelectedCommits([])}>
                      {tx('clearSelection')}
                    </button>
                  </div>
                )}

                {commitPickerMode !== null && (
                  <div className="wp-review-picker">
                    <div className="wp-review-picker-search">
                      <Icon name="search" size={13} />
                      <input
                        autoFocus
                        value={commitSearch}
                        placeholder={tx('reviewSearchCommits')}
                        onChange={(e) => setCommitSearch(e.target.value)}
                        onKeyDown={(e) => {
                          if (e.key === 'Escape' && !e.nativeEvent.isComposing) {
                            e.stopPropagation();
                            setCommitPickerMode(null);
                          }
                        }}
                      />
                      {commitSearch && (
                        <button type="button" className="icon-btn" title={tx('clear')} onClick={() => setCommitSearch('')}>
                          <Icon name="x" size={12} />
                        </button>
                      )}
                    </div>
                    <div className="wp-review-picker-list">
                      {canAddDirectHash && (
                        <button
                          type="button"
                          className="wp-review-picker-row"
                          onClick={() => pickCommitItem(typedHex)}
                        >
                          <span className="wp-review-hash-pill mono">{typedHex.slice(0, 7)}</span>
                          <span className="grow truncate">{tx('reviewAddCommitHash', { hash: typedHex })}</span>
                        </button>
                      )}
                      {commitsLoading && displayedCommits.length === 0 ? (
                        <div className="wp-review-picker-empty">
                          <span className="spinner mini" /> {tx('loading')}
                        </div>
                      ) : displayedCommits.length === 0 && !canAddDirectHash ? (
                        <div className="wp-review-picker-empty">{tx('reviewNoCommitsFound')}</div>
                      ) : (
                        displayedCommits.map((c) => {
                          const picked = selectedCommits.includes(c.hash);
                          return (
                            <button
                              key={c.hash}
                              type="button"
                              className={`wp-review-picker-row ${picked ? 'active' : ''}`}
                              onClick={() => pickCommitItem(c.hash)}
                            >
                              <span className="wp-review-hash-pill mono">{c.short}</span>
                              <span className="truncate grow">{c.subject}</span>
                              <span className="wp-review-picker-time">{timeAgo(c.time * 1000)}</span>
                              {picked && <Icon name="check" size={12} />}
                            </button>
                          );
                        })
                      )}
                    </div>
                    <div className="wp-review-picker-foot">
                      <span className="muted small">{tx('reviewQuickRange')}</span>
                      <div className="row">
                        {[1, 3, 5].map((n) => (
                          <button
                            key={n}
                            type="button"
                            className="wp-review-chip"
                            disabled={recentCommits.length === 0}
                            onClick={() => pickRecentN(n)}
                          >
                            {tx('reviewLastNCommits', { n })}
                          </button>
                        ))}
                      </div>
                    </div>
                  </div>
                )}
              </>
            )}

            {scope === 'range' && (
              <div className="wp-review-range-box">
                <input
                  type="text"
                  className="mono"
                  value={rangeInput}
                  placeholder={tx('reviewRangePlaceholder')}
                  onChange={(e) => setRangeInput(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' && !e.nativeEvent.isComposing) {
                      e.preventDefault();
                      setDebouncedRange(rangeInput);
                    }
                  }}
                />
                <div className="wp-review-range-hints">
                  <span className="muted small">{tx('reviewRangeHint')}</span>
                  <div className="wp-review-subfilters">
                    {[
                      'HEAD~1..HEAD',
                      'HEAD~3..HEAD',
                      ...(defaultBaseCand && defaultBaseCand !== currentBranch ? [`${defaultBaseCand}...HEAD`] : []),
                    ].map((expr) => (
                      <button
                        key={expr}
                        type="button"
                        className={`wp-review-chip mono ${rangeInput.trim() === expr ? 'active' : ''}`}
                        onClick={() => {
                          setRangeInput(expr);
                          setDebouncedRange(expr);
                        }}
                      >
                        {expr}
                      </button>
                    ))}
                  </div>
                </div>
              </div>
            )}

            <div className="wp-review-files-box">
              {legacyDaemon ? (
                <div className="wp-review-files-msg muted small">{tx('reviewLegacyDaemonHint')}</div>
              ) : scopeRes?.error ? (
                <div className="wp-review-scope-err">
                  <Icon name="alert" size={13} />
                  <span>{trErr(scopeRes.error)}</span>
                </div>
              ) : scopeLoading && !scopeRes ? (
                <div className="wp-review-files-msg muted small">
                  <span className="spinner mini" /> {tx('reviewLoadingScope')}
                </div>
              ) : scope === 'commits' && selectedCommits.length === 0 ? (
                <div className="wp-review-files-msg muted small">{tx('reviewSelectCommitPrompt')}</div>
              ) : scope === 'range' && !debouncedRange.trim() ? (
                <div className="wp-review-files-msg muted small">{tx('reviewEnterRangePrompt')}</div>
              ) : scopeFiles.length === 0 ? (
                <div className="wp-review-files-msg muted small">{tx('reviewNoFilesInScope')}</div>
              ) : (
                <>
                  <div className="wp-review-files-head">
                    <div className="wp-review-files-summary">
                      <span className="wp-review-files-count">
                        {excluded.size > 0
                          ? tx('reviewWillReviewSelectedOfTotal', { n: includedFiles.length, total: scopeFiles.length })
                          : tx('reviewWillReviewFiles', { n: scopeFiles.length })}
                      </span>
                      {includedTotals.any && (
                        <span className="wp-stat">
                          {!!includedTotals.added && <span className="add">+{includedTotals.added}</span>}
                          {!!includedTotals.deleted && <span className="del">−{includedTotals.deleted}</span>}
                        </span>
                      )}
                      {scopeLoading && <span className="spinner mini" />}
                    </div>
                    <div className="wp-review-files-acts">
                      <button type="button" className="wp-review-link" onClick={excludeLargeFiles}>
                        {tx('reviewExcludeLargeFiles')}
                      </button>
                      {excluded.size > 0 && (
                        <button type="button" className="wp-review-link" onClick={() => setExcluded(new Set())}>
                          {tx('selectAll')}
                        </button>
                      )}
                      <button
                        type="button"
                        className="wp-review-link"
                        onClick={() => setFilesExpanded((v) => !v)}
                      >
                        {filesExpanded ? tx('reviewCollapseFilter') : tx('reviewExpandFilter')}{' '}
                        <Icon name={filesExpanded ? 'up' : 'down'} size={11} />
                      </button>
                    </div>
                  </div>

                  {!filesExpanded && previewNames && (
                    <div className="wp-review-files-preview truncate muted small">
                      {previewNames}
                      {includedFiles.length > 3 ? `, … (+${includedFiles.length - 3})` : ''}
                    </div>
                  )}

                  {filesExpanded && (
                    <>
                      {scopeFiles.length > 10 && (
                        <div className="wp-review-file-filter">
                          <Icon name="search" size={12} />
                          <input
                            value={fileFilter}
                            placeholder={tx('reviewFilterFilesPlaceholder')}
                            onChange={(e) => setFileFilter(e.target.value)}
                          />
                          {fileFilter.trim() && filteredPreviewFiles.length > 0 && (
                            <button type="button" className="wp-review-link" onClick={excludeMatchedFiles}>
                              {tx('reviewExcludeFiltered', { n: filteredPreviewFiles.length })}
                            </button>
                          )}
                          {fileFilter && (
                            <button type="button" className="icon-btn" title={tx('clear')} onClick={() => setFileFilter('')}>
                              <Icon name="x" size={11} />
                            </button>
                          )}
                        </div>
                      )}
                      <div className="wp-review-file-list">
                        {filteredPreviewFiles.slice(0, fileLimit).map((f) => {
                          const [letter, cls] = statusBadge(f.status);
                          const isExcluded = excluded.has(f.path);
                          return (
                            <label key={f.path} className={`wp-review-file-row ${isExcluded ? 'excluded' : ''}`}>
                              <input type="checkbox" checked={!isExcluded} onChange={() => toggleFile(f.path)} />
                              <span className={`wp-commit-st ${cls}`}>{letter}</span>
                              <span className="mono truncate grow" title={f.path}>
                                {f.path}
                              </span>
                              {isExcluded && <span className="wp-review-excluded-tag">{tx('reviewExcludedBadge')}</span>}
                              {f.binary ? (
                                <span className="wp-review-bin-tag">{tx('reviewBinaryBadge')}</span>
                              ) : (
                                (f.added != null || f.deleted != null) && (
                                  <span className="wp-stat">
                                    {!!f.added && <span className="add">+{f.added}</span>}
                                    {!!f.deleted && <span className="del">−{f.deleted}</span>}
                                  </span>
                                )
                              )}
                            </label>
                          );
                        })}
                        {filteredPreviewFiles.length > fileLimit && (
                          <button
                            type="button"
                            className="wp-review-link wp-review-more-files"
                            onClick={() => setFileLimit(filteredPreviewFiles.length)}
                          >
                            {tx('reviewShowMoreFiles', { n: filteredPreviewFiles.length })}
                          </button>
                        )}
                      </div>
                    </>
                  )}
                </>
              )}
            </div>
          </div>

          <label className="wp-review-field">
            <span>{tx('reviewFocusLabel')}</span>
            <input
              type="text"
              value={focus}
              placeholder={tx('reviewFocusPlaceholder')}
              onChange={(e) => setFocus(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && !e.nativeEvent.isComposing) {
                  e.preventDefault();
                  void start();
                }
              }}
            />
          </label>
        </div>

        <div className="row end">
          <button type="button" className="btn" onClick={onClose} disabled={starting}>
            {tx('cancel')}
          </button>
          <button type="button" className="btn primary" onClick={() => void start()} disabled={!canStart}>
            {starting && <span className="spinner mini" />}
            {tx('startReview')}
          </button>
        </div>
      </div>
    </div>
  );
}

export function ReviewPane({
  hostId,
  reviewSessionId,
  parentSessionId,
  onOpenFile,
}: {
  hostId: string;
  reviewSessionId: string;
  parentSessionId: string;
  onOpenFile: (relPath: string, line?: number) => void;
}) {
  useLang();
  const key = skey(hostId, reviewSessionId);
  const data = useSessionData(key);
  const info = useStore((s) => s.sessions[hostId]?.find((x) => x.id === reviewSessionId));
  const meta = getReviewMeta(reviewSessionId);
  const [showRaw, setShowRaw] = useState(false);
  const [selected, setSelected] = useState<Set<string> | null>(null);

  useEffect(() => {
    loadSession(hostId, reviewSessionId).catch(() => {});
  }, [hostId, reviewSessionId]);

  useEffect(() => registerVisibleChildSession(hostId, reviewSessionId), [hostId, reviewSessionId]);

  const busy = info?.status === 'running' || info?.status === 'starting';
  const tool: ToolKind = meta?.tool ?? info?.tool ?? 'claude';

  const latestText = useMemo(() => {
    const streaming = [...data.live.values()].join('\n').trim();
    if (streaming) return streaming;
    for (let i = data.events.length - 1; i >= 0; i--) {
      const e = data.events[i];
      if (e.kind === 'assistant_text' && e.text.trim()) return e.text;
    }
    return '';
  }, [data.events, data.live, data.version]);

  const lastError = useMemo(() => {
    for (let i = data.events.length - 1; i >= 0; i--) {
      const e = data.events[i];
      if (e.kind === 'error') return e.message;
    }
    return '';
  }, [data.events]);

  const pendingPerm = useMemo(() => {
    const resolved = new Set<string>();
    for (const e of data.events) {
      if (e.kind === 'permission_resolved') resolved.add(e.requestId);
    }
    for (let i = data.events.length - 1; i >= 0; i--) {
      const e = data.events[i];
      if (e.kind === 'permission_request' && !resolved.has(e.requestId)) return e;
    }
    return null;
  }, [data.events]);

  const parsed = useMemo(() => (busy ? null : parseReviewIssues(latestText)), [busy, latestText]);

  // Auto-select high & medium severity findings once parsing completes
  useEffect(() => {
    if (!parsed) return;
    setSelected((cur) => {
      if (cur !== null) return cur;
      return new Set(parsed.issues.filter((i) => i.severity !== 'low').map((i) => i.id));
    });
  }, [parsed]);

  const activeSelected = selected ?? new Set<string>();
  const toggleIssue = (id: string) =>
    setSelected((cur) => {
      const next = new Set(cur ?? []);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const sendBack = () => {
    if (parsed && parsed.issues.length > 0) {
      const chosen = parsed.issues.filter((i) => activeSelected.has(i.id));
      if (!chosen.length) {
        toast(tx('selectAtLeastOneFinding'));
        return;
      }
      const lines = chosen.map((it, idx) => {
        const loc = it.file ? ` (${it.file}${it.line ? `:${it.line}` : ''})` : '';
        const sev = it.severity.toUpperCase();
        const parts = [`${idx + 1}. [${sev}] ${it.title}${loc}`, `   ${it.summary}`];
        if (it.suggestion) parts.push(`   ${isEn() ? 'Suggestion' : '建议'}: ${it.suggestion}`);
        return parts.join('\n');
      });
      const header = tx('reviewSendBackHeader', { tool: TOOL_LABEL[tool], n: chosen.length });
      window.dispatchEvent(
        new CustomEvent(INSERT_COMPOSER, {
          detail: { sessionId: parentSessionId, text: `${header}\n\n${lines.join('\n\n')}` },
        }),
      );
      toast(tx('reviewInsertedToChat'));
      return;
    }
    if (latestText.trim()) {
      const header = tx('reviewSendBackRawHeader', { tool: TOOL_LABEL[tool] });
      window.dispatchEvent(
        new CustomEvent(INSERT_COMPOSER, {
          detail: { sessionId: parentSessionId, text: `${header}\n\n${latestText.trim()}` },
        }),
      );
      toast(tx('reviewInsertedToChat'));
    }
  };

  const stop = () => rpc(hostId, 'session.interrupt', { sessionId: reviewSessionId }).catch(errorToast);
  const answerPerm = (decision: 'allow' | 'deny') => {
    if (!pendingPerm) return;
    rpc(hostId, 'session.respondPermission', {
      sessionId: reviewSessionId,
      requestId: pendingPerm.requestId,
      decision,
    }).catch(errorToast);
  };

  const sevLabel = (s: ReviewIssue['severity']) => (s === 'high' ? tx('sevHigh') : s === 'medium' ? tx('sevMedium') : tx('sevLow'));

  return (
    <div className="wp-review-pane">
      <div className="wp-review-head">
        <div className="wp-review-meta">
          <ToolBadge tool={tool} size={15} />
          <b>{TOOL_LABEL[tool]}</b>
          <span className="wp-review-pill readonly">{tx('readonlyBadge')}</span>
          {meta && (
            <span className="muted small truncate">
              {meta.scopeLabel ??
                (meta.scope === 'branch' && meta.base ? tx('diffScopeBranchVs', { base: meta.base }) : tx('diffScopeUncommitted'))}
              {meta.includedFilesCount != null && meta.totalFilesCount != null && meta.includedFilesCount < meta.totalFilesCount
                ? ` (${meta.includedFilesCount}/${meta.totalFilesCount})`
                : ''}
            </span>
          )}
        </div>
        <div className="wp-review-head-acts">
          {busy ? (
            <button type="button" className="btn small" onClick={stop}>
              <Icon name="stop" size={12} /> {tx('stopReview')}
            </button>
          ) : (
            parsed && (
              <button type="button" className="btn small" onClick={() => setShowRaw((v) => !v)}>
                {showRaw ? tx('showStructuredFindings') : tx('showRawOutput')}
              </button>
            )
          )}
        </div>
      </div>

      {pendingPerm && (
        <div className="wp-review-perm">
          <div className="wp-review-perm-text">
            <Icon name="shield" size={13} />
            <span>{tx('reviewPermNeeded', { tool: pendingPerm.toolName })}</span>
          </div>
          <div className="row">
            <button type="button" className="btn small" onClick={() => answerPerm('deny')}>
              {tx('deny')}
            </button>
            <button type="button" className="btn small primary" onClick={() => answerPerm('allow')}>
              {tx('allowOnce')}
            </button>
          </div>
        </div>
      )}

      <div className="wp-review-body">
        {busy && !latestText ? (
          <div className="wp-review-loading">
            <span className="spinner" />
            <span>{tx('reviewingWithTool', { tool: TOOL_LABEL[tool] })}</span>
          </div>
        ) : lastError && !latestText ? (
          <div className="wp-review-error">{lastError}</div>
        ) : parsed && !showRaw ? (
          <div className="wp-review-results">
            {parsed.prose && (
              <div className="wp-review-prose">
                <ChatMarkdown text={parsed.prose} />
              </div>
            )}
            {parsed.issues.length === 0 ? (
              <div className="wp-review-clean">
                <Icon name="check" size={16} />
                <span>{tx('reviewNoIssuesFound')}</span>
              </div>
            ) : (
              <>
                <div className="wp-review-selbar">
                  <span className="muted small">{tx('reviewFoundCount', { n: parsed.issues.length })}</span>
                  <div className="row">
                    <button
                      type="button"
                      className="wp-review-link"
                      onClick={() => setSelected(new Set(parsed.issues.map((i) => i.id)))}
                    >
                      {tx('selectAll')}
                    </button>
                    <button
                      type="button"
                      className="wp-review-link"
                      onClick={() => setSelected(new Set(parsed.issues.filter((i) => i.severity === 'high').map((i) => i.id)))}
                    >
                      {tx('selectHighOnly')}
                    </button>
                    <button type="button" className="wp-review-link" onClick={() => setSelected(new Set())}>
                      {tx('clearSelection')}
                    </button>
                  </div>
                </div>
                <div className="wp-review-cards">
                  {parsed.issues.map((it) => {
                    const checked = activeSelected.has(it.id);
                    return (
                      <div key={it.id} className={`wp-review-card sev-${it.severity} ${checked ? 'checked' : ''}`}>
                        <label className="wp-review-card-head">
                          <input type="checkbox" checked={checked} onChange={() => toggleIssue(it.id)} />
                          <span className={`wp-review-sev sev-${it.severity}`}>{sevLabel(it.severity)}</span>
                          <span className="wp-review-card-title">{it.title}</span>
                        </label>
                        {it.file && (
                          <button
                            type="button"
                            className="wp-review-loc mono"
                            onClick={() => onOpenFile(it.file!, it.line)}
                            title={tx('openInDiff')}
                          >
                            <Icon name="file" size={12} />
                            <span>
                              {it.file}
                              {it.line ? `:${it.line}` : ''}
                            </span>
                          </button>
                        )}
                        {it.summary && <div className="wp-review-card-desc">{it.summary}</div>}
                        {it.suggestion && (
                          <div className="wp-review-card-sug">
                            <b>{tx('reviewSuggestion')}: </b>
                            {it.suggestion}
                          </div>
                        )}
                      </div>
                    );
                  })}
                </div>
              </>
            )}
          </div>
        ) : latestText ? (
          <div className="wp-review-raw">
            {busy && (
              <div className="wp-review-streaming">
                <span className="spinner mini" />
                <span>{tx('reviewingWithTool', { tool: TOOL_LABEL[tool] })}</span>
              </div>
            )}
            <ChatMarkdown text={latestText} streaming={busy} />
          </div>
        ) : (
          <div className="wp-review-loading muted small">{tx('waitingForReviewOutput')}</div>
        )}
      </div>

      {!busy && latestText && (
        <div className="wp-review-foot">
          <span className="muted small">
            {parsed && parsed.issues.length > 0 ? tx('selectedFindingsCount', { n: activeSelected.size }) : ''}
          </span>
          <button
            type="button"
            className="btn primary small"
            disabled={!!parsed && parsed.issues.length > 0 && activeSelected.size === 0}
            onClick={sendBack}
          >
            <Icon name="send" size={12} />
            {parsed && parsed.issues.length > 0
              ? tx('sendFindingsToMainChat', { n: activeSelected.size })
              : tx('sendReviewSummaryToMainChat')}
          </button>
        </div>
      )}
    </div>
  );
}
