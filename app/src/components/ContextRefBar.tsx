import { filePath } from '../chatImages';
import { writeClipboard } from '../clipboard';
import { formatLineRange, type ContextRef } from '../contextRefs';
import { getT, useLang } from '../i18n';
import { navigate, toast } from '../store';
import { basename } from '../util';
import { FileIcon } from './FilePanel';
import { commitTab } from './GitHistory';
import { Icon } from './Icon';
import { openMenu, type MenuItem } from './Overlay';
import { openFileInPanel } from './SessionActions';

const t = getT('composer');

function openRef(r: ContextRef, hostId?: string, sessionId?: string, cwd?: string) {
  if (!hostId || !sessionId || r.untitled || r.kind === 'terminal') return;
  if (r.kind === 'side') {
    if (r.sideSessionId) openFileInPanel(hostId, sessionId, `side:${r.sideSessionId}`);
    return;
  }
  if (r.kind === 'session') {
    if (r.sideSessionId) navigate({ kind: 'session', hostId, sessionId: r.sideSessionId });
    return;
  }
  if (r.commit) {
    openFileInPanel(hostId, sessionId, commitTab(r.commit, r.path), { line: r.fromLine, endLine: r.toLine, diff: true });
    return;
  }
  let p = filePath(r.path, cwd);
  if (/^[a-zA-Z]:[\\/]/.test(p)) p = p.replace(/\//g, '\\');
  openFileInPanel(hostId, sessionId, p, { line: r.fromLine, endLine: r.toLine, diff: r.kind === 'diff' || r.kind === 'review' });
}

/** Hover card: where the snippet is from, optional review comment, and its first lines (diff lines colored) */
const PREVIEW_LINES = 12;
function RefPreview({ r, loc, hint }: { r: ContextRef; loc: string; hint: string }) {
  const all = r.content.split('\n');
  const isDiffLike = r.kind === 'diff' || r.kind === 'review';
  const body = isDiffLike ? all.filter((l) => !l.startsWith('@@')) : all;
  const shown = body.slice(0, PREVIEW_LINES);
  return (
    <div className="cref-pop" role="tooltip">
      <div className="cref-pop-loc mono truncate">{loc}</div>
      {r.kind === 'review' && r.reviewSource && <div className="cref-pop-hint">{t('aiReviewSourceLabel', { source: r.reviewSource })}</div>}
      {r.kind === 'review' && r.comment && <div className="cref-pop-comment">{r.comment}</div>}
      {r.content && (
        <pre className="cref-pop-code">
          {shown.map((l, i) => (
            <div key={i} className={isDiffLike ? (l[0] === '+' ? 'add' : l[0] === '-' ? 'del' : '') : ''}>
              {l || ' '}
            </div>
          ))}
          {body.length > shown.length && <div className="more">{t('moreLines', { n: body.length - shown.length })}</div>}
        </pre>
      )}
      {hint && <div className="cref-pop-hint">{hint}</div>}
    </div>
  );
}

export function ContextRefBar({
  refs,
  hostId,
  sessionId,
  cwd,
  onRemove,
  onEditReview,
  onSendReviews,
  onClearReviews,
  sending,
  className = '',
}: {
  refs: readonly ContextRef[];
  hostId?: string;
  sessionId?: string;
  cwd?: string;
  onRemove?: (id: string) => void;
  onEditReview?: (ref: ContextRef) => void;
  onSendReviews?: () => void;
  onClearReviews?: () => void;
  sending?: boolean;
  className?: string;
}) {
  useLang();
  if (!refs.length) return null;
  const canOpen = !!hostId && !!sessionId;
  const reviewCount = refs.filter((r) => r.kind === 'review').length;

  const menu = (e: React.MouseEvent, r: ContextRef) => {
    e.preventDefault();
    e.stopPropagation();
    const lines = formatLineRange(r.fromLine, r.toLine);
    const items: (MenuItem | '-')[] = [
      ...(canOpen && !r.untitled && r.kind !== 'terminal' && ((r.kind !== 'side' && r.kind !== 'session') || !!r.sideSessionId)
        ? [
            {
              label: r.kind === 'side' ? t('openSideChat') : r.kind === 'session' ? t('openReferencedSession') : t('openInFilePanel'),
              icon: r.kind === 'side' ? 'sideChat' : r.kind === 'session' ? 'chat' : 'file',
              run: () => openRef(r, hostId, sessionId, cwd),
            },
          ]
        : []),
      ...(canOpen && r.kind === 'review' && r.sideSessionId
        ? [{ label: t('openReviewSideChat'), icon: 'sparkles', run: () => openFileInPanel(hostId, sessionId, `side:${r.sideSessionId}`) }]
        : []),
      ...(r.kind === 'review' && onEditReview
        ? [{ label: t('editReviewComment'), icon: 'edit', run: () => onEditReview(r) }]
        : []),
      {
        label: t('copyReferencedSnippet'),
        icon: 'copy',
        run: async () => toast((await writeClipboard(r.content)) ? t('snippetCopied') : t('copyFailed')),
      },
      ...(r.kind !== 'terminal' && r.kind !== 'side' && r.kind !== 'session'
        ? [
            {
              label: t('copyPathWithLine'),
              run: async () => toast((await writeClipboard(`${r.path}:${lines}`)) ? t('pathCopied') : t('copyFailed')),
            },
          ]
        : []),
      ...(onRemove ? ['-' as const, { label: t('remove'), icon: 'x', run: () => onRemove(r.id) }] : []),
    ];
    openMenu(e, items);
  };

  return (
    <div className={`cref-wrap ${className}`.trim()}>
      {reviewCount > 0 && onSendReviews && (
        <div className="cref-review-bar">
          <span className="cref-review-title">
            <Icon name="comment" size={12} />
            {t('reviewQueueCount', { n: reviewCount })}
          </span>
          <div className="cref-review-acts">
            {onClearReviews && (
              <button type="button" className="cref-review-btn" onClick={onClearReviews} disabled={sending}>
                {t('clear')}
              </button>
            )}
            <button type="button" className="cref-review-btn primary" onClick={onSendReviews} disabled={sending}>
              <Icon name="send" size={11} />
              {t('sendReviewComments', { n: reviewCount })}
            </button>
          </div>
        </div>
      )}
      <div className="cref-row">
        {refs.map((r) => {
          const lines = formatLineRange(r.fromLine, r.toLine);
          const isTerm = r.kind === 'terminal';
          const isReview = r.kind === 'review';
          const isSide = r.kind === 'side';
          const isSession = r.kind === 'session';
          const name = isTerm || isSide || isSession ? r.path : basename(r.path);
          const clickable = canOpen && !r.untitled && !isTerm && ((!isSide && !isSession) || !!r.sideSessionId);
          const lineCount = Math.max(1, r.toLine - r.fromLine + 1);
          const fullLoc = isSide
            ? `${t('sideChatRefBadge')} · ${r.path}`
            : isSession
              ? t('sessionRefLoc', { tool: r.comment ?? '', title: r.path })
              : isTerm
              ? `${r.path} · ${t('terminalLines', { n: lineCount })}${r.exitCode !== undefined ? ` · exit ${r.exitCode}` : ''}`
              : `${r.reviewSource ? `${r.reviewSource} · ` : ''}${r.path}:${lines}${r.commit ? ` @${r.commit.slice(0, 7)}` : ''}`;
          return (
            <div
              key={r.id}
              className={`cref-chip ${clickable ? 'clickable' : ''} ${onRemove ? 'removable' : ''} ${isReview ? 'review' : ''} ${r.reviewSource ? 'ai-review' : ''} ${isTerm ? 'terminal' : ''} ${isSide || isSession ? 'side' : ''}`}
              role={clickable ? 'button' : undefined}
              tabIndex={clickable ? 0 : undefined}
              aria-label={fullLoc}
              onClick={() => clickable && openRef(r, hostId, sessionId, cwd)}
              onKeyDown={(e) => {
                if (clickable && (e.key === 'Enter' || e.key === ' ')) {
                  e.preventDefault();
                  openRef(r, hostId, sessionId, cwd);
                }
              }}
              onContextMenu={(e) => menu(e, r)}
            >
              {isSide ? (
                <Icon name="sideChat" size={13} />
              ) : isSession ? (
                <Icon name="chat" size={13} />
              ) : isTerm ? (
                <Icon name="terminal" size={13} />
              ) : isReview && r.reviewSource ? (
                <Icon name="sparkles" size={13} />
              ) : isReview ? (
                <Icon name="comment" size={13} />
              ) : (
                <FileIcon name={name} size={13} />
              )}
              <span className="cref-name truncate">{name}</span>
              {isSide ? (
                r.comment ? <span className="cref-comment truncate">{r.comment}</span> : <span className="cref-lines">{t('sideChatRefBadge')}</span>
              ) : isSession ? (
                <span className="cref-lines">{r.comment || t('sessionRefBadge')}</span>
              ) : isTerm ? (
                <span className="cref-lines">{t('terminalLines', { n: lineCount })}</span>
              ) : (
                <span className="cref-lines">{lines}</span>
              )}
              {isTerm && r.exitCode !== undefined && r.exitCode !== 0 && (
                <span className="cref-exit">{t('exitCodeBadge', { code: r.exitCode })}</span>
              )}
              {isReview && r.comment && <span className="cref-comment truncate">{r.comment}</span>}
              {r.commit && <span className="cref-hash mono">{r.commit.slice(0, 7)}</span>}
              {(r.kind === 'diff' || isReview) && (!!r.added || !!r.deleted) && !isReview && (
                <span className="cref-stat">
                  {!!r.added && <span className="add">{`+${r.added}`}</span>}
                  {!!r.deleted && <span className="del">{`−${r.deleted}`}</span>}
                </span>
              )}
              {onRemove && (
                <button
                  type="button"
                  className="cref-x"
                  title={t('remove')}
                  aria-label={t('remove')}
                  onClick={(e) => {
                    e.stopPropagation();
                    onRemove(r.id);
                  }}
                >
                  <Icon name="x" size={11} />
                </button>
              )}
              <RefPreview r={r} loc={fullLoc} hint={clickable ? t('clickToOpenContextRefHint') : ''} />
            </div>
          );
        })}
      </div>
    </div>
  );
}
