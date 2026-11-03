import { useCallback, useEffect, useRef, useState } from 'react';
import { writeClipboard } from '../clipboard';
import { tr, useLang } from '../i18n';
import { toast } from '../store';
import { rpc } from '../transport';
import type { CommitFile, CommitInfo } from '../types';
import { lsGet, lsSet, timeAgo } from '../util';
import { FileIcon, gitMark, gitText, LineStat } from './FilePanel';
import { Icon } from './Icon';
import { openMenu } from './Overlay';

// "Commit history" box under the Changes view of the file panel: commits of HEAD, newest first, paged as you scroll;
// clicking a commit lists the files it changed, clicking a file opens that file's diff in the commit as a read-only tab

/** Tab key for a file in a commit: git:<hash>:<path relative to the repo root> */
export const COMMIT_PREFIX = 'git:';
export const commitTab = (hash: string, rel: string) => `${COMMIT_PREFIX}${hash}:${rel}`;
export const isCommitTab = (t: string) => t.startsWith(COMMIT_PREFIX) && /^git:[0-9a-f]{4,64}:/.test(t);
export function parseCommitTab(t: string): { hash: string; rel: string } | null {
  const m = /^git:([0-9a-f]{4,64}):(.+)$/.exec(t);
  return m ? { hash: m[1], rel: m[2] } : null;
}

const PAGE = 50;
const HEAD_MIN = 28;

/** "HEAD -> main" -> "main"; skip a bare HEAD (detached) and remote HEAD pointers */
function refLabels(refs: string[]): string[] {
  return refs
    .map((r) => (r.startsWith('HEAD -> ') ? r.slice(8) : r))
    .filter((r) => r !== 'HEAD' && !r.endsWith('/HEAD'));
}

const baseName = (p: string) => p.slice(p.lastIndexOf('/') + 1);
const dirName = (p: string) => (p.includes('/') ? p.slice(0, p.lastIndexOf('/')) : '');

export function GitHistory({
  hostId,
  cwd,
  head,
  selected,
  grow,
  onOpen,
}: {
  hostId: string;
  cwd: string;
  /** Current HEAD commit: reload the first page when it changes (a new commit, a checkout) */
  head?: string | null;
  /** Path of the current tab, to highlight the open commit file */
  selected?: string;
  /** Nothing above (no uncommitted changes): take all the remaining height */
  grow?: boolean;
  onOpen: (tab: string) => void;
}) {
  useLang();
  const [open, setOpen] = useState(() => lsGet('wp-hist-open', !window.matchMedia('(max-width: 760px)').matches));
  const [height, setHeight] = useState(() => lsGet('wp-hist-h', 280));
  const [commits, setCommits] = useState<CommitInfo[]>([]);
  const [more, setMore] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [expanded, setExpanded] = useState<string | null>(null);
  const [files, setFiles] = useState<Record<string, CommitFile[] | 'loading' | 'error'>>({});
  const box = useRef<HTMLDivElement>(null);
  // Bumped on every reload so a slow older page doesn't land on top of the new list
  const gen = useRef(0);

  const loadPage = useCallback(
    (skip: number) => {
      const g = skip === 0 ? ++gen.current : gen.current;
      setLoading(true);
      rpc<{ repo: string | null; commits: CommitInfo[]; more: boolean }>(hostId, 'git.log', { cwd, skip, limit: PAGE })
        .then((r) => {
          if (g !== gen.current) return;
          setError('');
          setCommits((c) => (skip === 0 ? r.commits : [...c, ...r.commits.filter((x) => !c.some((y) => y.hash === x.hash))]));
          setMore(r.more);
        })
        .catch((e) => g === gen.current && setError(String((e as Error)?.message ?? e)))
        .finally(() => g === gen.current && setLoading(false));
    },
    [hostId, cwd],
  );

  // Load when first opened and whenever HEAD moves
  useEffect(() => {
    if (open) loadPage(0);
  }, [open, head, loadPage]);

  const toggleOpen = () => {
    lsSet('wp-hist-open', !open);
    setOpen(!open);
  };

  const toggleCommit = (c: CommitInfo) => {
    if (expanded === c.hash) return setExpanded(null);
    setExpanded(c.hash);
    if (Array.isArray(files[c.hash]) || files[c.hash] === 'loading') return;
    setFiles((m) => ({ ...m, [c.hash]: 'loading' }));
    rpc<{ files: CommitFile[] }>(hostId, 'git.show', { cwd, hash: c.hash })
      .then((r) => setFiles((m) => ({ ...m, [c.hash]: r.files })))
      .catch(() => setFiles((m) => ({ ...m, [c.hash]: 'error' })));
  };

  const copy = async (text: string) => {
    if (await writeClipboard(text)) toast(tr('已复制', 'Copied'));
  };
  const commitMenu = (e: React.MouseEvent, c: CommitInfo) => {
    e.preventDefault();
    openMenu(e, [
      { label: tr('复制提交号', 'Copy commit hash'), icon: 'copy', run: () => copy(c.hash) },
      { label: tr('复制提交说明', 'Copy commit message'), run: () => copy(c.subject) },
    ]);
  };

  const onScroll = (e: React.UIEvent<HTMLDivElement>) => {
    const el = e.currentTarget;
    if (more && !loading && el.scrollTop + el.clientHeight > el.scrollHeight - 80) loadPage(commits.length);
  };

  // Drag the top edge: the box grows upwards; keep at least 80px for each part
  const drag = (e: React.PointerEvent) => {
    e.preventDefault();
    const side = box.current?.parentElement;
    const y0 = e.clientY;
    const h0 = box.current?.offsetHeight ?? height;
    const max = Math.max(120, (side?.clientHeight ?? 600) - 160);
    let h = h0;
    const move = (ev: PointerEvent) => {
      h = Math.round(Math.min(max, Math.max(80, h0 + y0 - ev.clientY)));
      setHeight(h);
    };
    const up = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
      document.body.classList.remove('resizing-y');
      lsSet('wp-hist-h', h);
    };
    document.body.classList.add('resizing-y');
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  };

  return (
    <div
      ref={box}
      className={`wp-hist ${open ? 'open' : ''} ${open && grow ? 'grow' : ''}`}
      style={open && !grow ? { height: Math.max(HEAD_MIN, height) } : undefined}
    >
      {open && !grow && <div className="wp-hist-split" onPointerDown={drag} title={tr('拖动调整高度', 'Drag to resize')} />}
      <div className="wp-hist-head">
        <button className="wp-hist-title" onClick={toggleOpen} aria-expanded={open}>
          <span className={`chev ${open ? 'open' : ''}`}>
            <Icon name="chevron" size={13} />
          </span>
          {tr('提交历史', 'Commits')}
        </button>
        <div className="grow" />
        {open && (
          <button className="icon-btn" title={tr('刷新', 'Refresh')} onClick={() => loadPage(0)}>
            <Icon name="refresh" size={13} />
          </button>
        )}
      </div>
      {open && (
        <div className="wp-hist-list" role="tree" aria-label={tr('提交历史', 'Commit history')} onScroll={onScroll}>
          {error && !commits.length ? (
            <div className="wp-tip">
              <b>{tr('读取提交历史失败', 'Cannot read commit history')}</b>
              {error}
            </div>
          ) : !commits.length ? (
            <div className="wp-tip">{loading ? tr('正在读取…', 'Loading…') : tr('还没有提交。', 'No commits yet.')}</div>
          ) : (
            <>
              {commits.map((c) => {
                const isOpen = expanded === c.hash;
                const fl = files[c.hash];
                const refs = refLabels(c.refs);
                return (
                  <div key={c.hash} className="wp-commit-group">
                    <div
                      role="treeitem"
                      aria-expanded={isOpen}
                      className={`wp-row wp-commit ${isOpen ? 'open' : ''}`}
                      title={`${c.hash}\n${c.author} · ${new Date(c.time * 1000).toLocaleString()}${c.parents.length > 1 ? `\n${tr('合并提交（和第一个父提交比较）', 'Merge commit (compared with its first parent)')}` : ''}\n\n${c.subject}`}
                      onClick={() => toggleCommit(c)}
                      onContextMenu={(e) => commitMenu(e, c)}
                    >
                      <span className="wp-twist">
                        <span className={`chev ${isOpen ? 'open' : ''}`}>
                          <Icon name="chevron" size={13} />
                        </span>
                      </span>
                      <span className="truncate wp-name">{c.subject}</span>
                      {refs.slice(0, 2).map((r) => (
                        <span key={r} className="wp-ref truncate" title={r}>
                          {r.replace(/^tag: /, '')}
                        </span>
                      ))}
                      <span className="wp-commit-time">{timeAgo(c.time * 1000)}</span>
                    </div>
                    {isOpen &&
                      (fl === 'loading' || !fl ? (
                        <div className="wp-limit wp-commit-sub">{tr('正在读取…', 'Loading…')}</div>
                      ) : fl === 'error' ? (
                        <div className="wp-limit wp-commit-sub">{tr('读取失败', 'Failed to load')}</div>
                      ) : fl.length === 0 ? (
                        <div className="wp-limit wp-commit-sub">{tr('这个提交没有改动文件。', 'This commit changes no files.')}</div>
                      ) : (
                        fl.map((f) => {
                          const tab = commitTab(c.hash, f.path);
                          const mark = gitMark(f.status);
                          const dir = dirName(f.path);
                          return (
                            <div
                              key={f.path}
                              role="treeitem"
                              aria-selected={selected === tab}
                              className={`wp-row wp-commit-file ${selected === tab ? 'sel' : ''} ${mark[1]}`}
                              title={`${f.path}\n${gitText(mark[0])}`}
                              onClick={() => onOpen(tab)}
                            >
                              <span className="wp-twist">
                                <FileIcon name={baseName(f.path)} />
                              </span>
                              <span className="truncate wp-name">
                                {baseName(f.path)}
                                {dir && <span className="wp-dir"> {dir}</span>}
                              </span>
                              <LineStat a={f.added} d={f.deleted} />
                              <span className="wp-git">{mark[0]}</span>
                            </div>
                          );
                        })
                      ))}
                  </div>
                );
              })}
              {loading && <div className="wp-limit">{tr('正在读取…', 'Loading…')}</div>}
            </>
          )}
        </div>
      )}
    </div>
  );
}
