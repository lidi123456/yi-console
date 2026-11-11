import { filePath } from '../chatImages';
import { writeClipboard } from '../clipboard';
import { formatLineRange, type ContextRef } from '../contextRefs';
import { getT, useLang } from '../i18n';
import { toast } from '../store';
import { basename } from '../util';
import { FileIcon } from './FilePanel';
import { commitTab } from './GitHistory';
import { Icon } from './Icon';
import { openMenu, type MenuItem } from './Overlay';
import { openFileInPanel } from './SessionActions';

const t = getT('composer');

function openRef(r: ContextRef, hostId?: string, sessionId?: string, cwd?: string) {
  if (!hostId || !sessionId || r.untitled) return;
  if (r.commit) {
    openFileInPanel(hostId, sessionId, commitTab(r.commit, r.path), { line: r.fromLine, endLine: r.toLine, diff: true });
    return;
  }
  let p = filePath(r.path, cwd);
  if (/^[a-zA-Z]:[\\/]/.test(p)) p = p.replace(/\//g, '\\');
  openFileInPanel(hostId, sessionId, p, { line: r.fromLine, endLine: r.toLine, diff: r.kind === 'diff' });
}

/** Hover card: where the snippet is from and its first lines (diff lines colored), like Cursor's context pill preview */
const PREVIEW_LINES = 12;
function RefPreview({ r, loc, hint }: { r: ContextRef; loc: string; hint: string }) {
  const all = r.content.split('\n');
  const body = r.kind === 'diff' ? all.filter((l) => !l.startsWith('@@')) : all;
  const shown = body.slice(0, PREVIEW_LINES);
  return (
    <div className="cref-pop" role="tooltip">
      <div className="cref-pop-loc mono truncate">{loc}</div>
      <pre className="cref-pop-code">
        {shown.map((l, i) => (
          <div key={i} className={r.kind === 'diff' ? (l[0] === '+' ? 'add' : l[0] === '-' ? 'del' : '') : ''}>
            {l || ' '}
          </div>
        ))}
        {body.length > shown.length && <div className="more">{t('moreLines', { n: body.length - shown.length })}</div>}
      </pre>
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
  className = '',
}: {
  refs: readonly ContextRef[];
  hostId?: string;
  sessionId?: string;
  cwd?: string;
  onRemove?: (id: string) => void;
  className?: string;
}) {
  useLang();
  if (!refs.length) return null;
  const canOpen = !!hostId && !!sessionId;

  const menu = (e: React.MouseEvent, r: ContextRef) => {
    e.preventDefault();
    e.stopPropagation();
    const lines = formatLineRange(r.fromLine, r.toLine);
    const items: (MenuItem | '-')[] = [
      ...(canOpen && !r.untitled
        ? [{ label: t('openInFilePanel'), icon: 'file', run: () => openRef(r, hostId, sessionId, cwd) }]
        : []),
      {
        label: t('copyReferencedSnippet'),
        icon: 'copy',
        run: async () => toast((await writeClipboard(r.content)) ? t('snippetCopied') : t('copyFailed')),
      },
      {
        label: t('copyPathWithLine'),
        run: async () => toast((await writeClipboard(`${r.path}:${lines}`)) ? t('pathCopied') : t('copyFailed')),
      },
      ...(onRemove ? ['-' as const, { label: t('remove'), icon: 'x', run: () => onRemove(r.id) }] : []),
    ];
    openMenu(e, items);
  };

  return (
    <div className={`cref-row ${className}`.trim()}>
      {refs.map((r) => {
        const lines = formatLineRange(r.fromLine, r.toLine);
        const name = basename(r.path);
        const clickable = canOpen && !r.untitled;
        const fullLoc = `${r.path}:${lines}${r.commit ? ` @${r.commit.slice(0, 7)}` : ''}`;
        // Like Cursor: "name (10-11)", the × replaces nothing and shows on hover, the code shows in a preview on hover
        return (
          <div
            key={r.id}
            className={`cref-chip ${clickable ? 'clickable' : ''} ${onRemove ? 'removable' : ''}`}
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
            <FileIcon name={name} size={13} />
            <span className="cref-name truncate">{name}</span>
            <span className="cref-lines">{lines}</span>
            {r.commit && <span className="cref-hash mono">{r.commit.slice(0, 7)}</span>}
            {r.kind === 'diff' && (!!r.added || !!r.deleted) && (
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
  );
}
