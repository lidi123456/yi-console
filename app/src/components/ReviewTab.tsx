import { useEffect, useMemo, useState } from 'react';
import { useBack } from '../back';
import { getT, isEn, useLang } from '../i18n';
import { permModes } from '../perm';
import { errorToast, hostStatus, loadSession, skey, toast, useSessionData, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type SessionInfo, type ToolKind } from '../types';
import { ChatMarkdown } from './ChatMarkdown';
import { Icon, ToolBadge } from './Icon';
import { INSERT_COMPOSER } from './FilePanel';

const tx = getT('filePanel');

export const REVIEW_PREFIX = 'review:';
export const isReviewTab = (t: string) => t.startsWith(REVIEW_PREFIX);
export const reviewTabSession = (t: string) => t.slice(REVIEW_PREFIX.length);

export type ReviewScope = 'uncommitted' | 'branch';

export interface ReviewMeta {
  sessionId: string;
  parentSessionId: string;
  tool: ToolKind;
  scope: ReviewScope;
  base?: string;
  mergeBase?: string;
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
  base?: string;
  mergeBase?: string;
  focus?: string;
  filesCount: number;
  zh: boolean;
}): string {
  const diffCmd =
    opts.scope === 'branch' && opts.mergeBase
      ? `git diff ${opts.mergeBase} (and untracked files via git status --short)`
      : `git diff HEAD (and untracked files via git status --short)`;
  const scopeDesc =
    opts.scope === 'branch' && opts.base
      ? `all branch + working-tree changes relative to ${opts.base}`
      : `all uncommitted working-tree changes relative to HEAD`;
  const langRule = opts.zh ? 'Write all titles, summaries, and suggestions in concise Chinese.' : 'Write all titles, summaries, and suggestions in concise English.';
  const focusLine = opts.focus?.trim() ? `\nExtra review focus from the user: ${opts.focus.trim()}` : '';
  return [
    `You are performing a READ-ONLY code review of ${scopeDesc} (${opts.filesCount} changed files).`,
    `DO NOT edit, write, or commit any files. Inspect the changes using read-only commands such as \`${diffCmd}\` and read surrounding code as needed.`,
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
    .filter((l, i, arr) => !(l === '' && arr[i - 1] === ''))
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
  defaultScope,
  branchBase,
  branchMergeBase,
  uncommittedCount,
  branchCount,
  onStarted,
  onClose,
}: {
  hostId: string;
  cwd: string;
  parentSessionId: string;
  currentTool?: ToolKind;
  defaultScope: ReviewScope;
  branchBase?: string;
  branchMergeBase?: string;
  uncommittedCount: number;
  branchCount: number;
  onStarted: (reviewSessionId: string) => void;
  onClose: () => void;
}) {
  useLang();
  useBack(true, onClose);
  const tools = useMemo(
    () => (hostStatus(hostId).hello?.tools ?? []).filter((t) => t.available && t.kind !== 'shell' && t.kind !== 'files'),
    [hostId],
  );
  const [tool, setTool] = useState<ToolKind>(() => pickReviewerTool(hostId, currentTool) ?? (tools[0]?.kind ?? 'claude'));
  const [scope, setScope] = useState<ReviewScope>(defaultScope);
  const [focus, setFocus] = useState('');
  const [starting, setStarting] = useState(false);

  const canBranch = !!branchMergeBase;
  const effScope: ReviewScope = scope === 'branch' && canBranch ? 'branch' : 'uncommitted';
  const filesCount = effScope === 'branch' ? branchCount : uncommittedCount;

  const start = async () => {
    if (starting || !tools.length) return;
    setStarting(true);
    try {
      const perm = readonlyPermFor(tool);
      const created = await rpc<SessionInfo>(hostId, 'session.create', {
        tool,
        cwd,
        mode: 'structured',
        parentId: parentSessionId,
        permissionMode: perm,
        title: `Review (${TOOL_LABEL[tool]})`,
      });
      setReviewMeta({
        sessionId: created.id,
        parentSessionId,
        tool,
        scope: effScope,
        base: branchBase,
        mergeBase: branchMergeBase,
        cwd,
      });
      const prompt = buildReviewPrompt({
        scope: effScope,
        base: branchBase,
        mergeBase: branchMergeBase,
        focus,
        filesCount,
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
          <label className="wp-review-field">
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
          </label>
          <label className="wp-review-field">
            <span>{tx('reviewScopeLabel')}</span>
            <div className="wp-review-scopes">
              <button
                type="button"
                className={`wp-review-scope ${effScope === 'uncommitted' ? 'active' : ''}`}
                onClick={() => setScope('uncommitted')}
              >
                <span>{tx('diffScopeUncommitted')}</span>
                <span className="muted small">{tx('files2', { length: uncommittedCount })}</span>
              </button>
              <button
                type="button"
                className={`wp-review-scope ${effScope === 'branch' ? 'active' : ''}`}
                disabled={!canBranch}
                onClick={() => setScope('branch')}
              >
                <span>{branchBase ? tx('diffScopeBranchVs', { base: branchBase }) : tx('diffScopeBranch')}</span>
                <span className="muted small">{canBranch ? tx('files2', { length: branchCount }) : tx('noBranchBase')}</span>
              </button>
            </div>
          </label>
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
          <button type="button" className="btn primary" onClick={() => void start()} disabled={starting || !tools.length}>
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
              {meta.scope === 'branch' && meta.base ? tx('diffScopeBranchVs', { base: meta.base }) : tx('diffScopeUncommitted')}
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
