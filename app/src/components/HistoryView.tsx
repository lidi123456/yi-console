import { useEffect, useMemo, useState } from 'react';
import { useBack } from '../back';
import { isEn, tr } from '../i18n';
import { errorToast, hostStatus, setState } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type HistoryEntry, type HubEvent, type ToolKind } from '../types';
import { shortPath, timeAgo } from '../util';
import { ChatView } from './ChatView';
import { Icon, ToolBadge } from './Icon';

const HISTORY_TOOLS: ToolKind[] = ['claude', 'codex', 'cursor', 'kiro', 'qoder', 'codebuddy'];
const PAGE = 100;

/** For comparing directories: expand ~, normalize slashes, strip the trailing slash; Windows paths are case-insensitive */
function normDir(p: string, home?: string): string {
  let d = p.trim();
  if (home && (d === '~' || d.startsWith('~/') || d.startsWith('~\\'))) d = home + d.slice(1);
  d = d.replace(/\\/g, '/').replace(/\/+$/, '');
  return /^[a-z]:/i.test(d) ? d.toLowerCase() : d;
}

/** p is dir itself or somewhere under it */
function underDir(p: string, dir: string): boolean {
  return p === dir || p.startsWith(dir + '/');
}

export function HistoryView({ hostId, cwd }: { hostId: string; cwd?: string }) {
  const [list, setList] = useState<HistoryEntry[] | null>(null);
  const [tool, setTool] = useState<ToolKind | ''>('');
  const [q, setQ] = useState('');
  const [sel, setSel] = useState<HistoryEntry | null>(null);
  useBack(!!sel, () => setSel(null));
  const [preview, setPreview] = useState<HubEvent[] | null>(null);
  const [pending, setPending] = useState<ToolKind[]>([]);
  const [limit, setLimit] = useState(PAGE);
  // Only one directory: passed in when coming from a directory's context menu in the sidebar
  const [dir, setDir] = useState(cwd ?? '');
  useEffect(() => setDir(cwd ?? ''), [cwd]);
  const hello = hostStatus(hostId).hello;
  const home = hello?.home;

  // One request per tool, run in parallel: Claude/Codex read local files quickly, Cursor/Kiro need to start a CLI and are slower;
  // show whatever arrives first instead of waiting for the slowest
  const load = () => {
    const tools = HISTORY_TOOLS.filter((k) => !hello || hello.tools.some((t) => t.kind === k && t.available));
    let alive = true;
    setList(tools.length ? null : []);
    setPending(tools);
    for (const t of tools) {
      rpc<HistoryEntry[]>(hostId, 'history.list', { tool: t })
        .then((r) => {
          if (!alive) return;
          setList((old) =>
            [...(old ?? []).filter((h) => h.tool !== t), ...r].sort((a, b) => b.updatedAt - a.updatedAt),
          );
        })
        .catch((e) => alive && errorToast(e))
        .finally(() => {
          if (!alive) return;
          setPending((p) => p.filter((x) => x !== t));
          setList((old) => old ?? []);
        });
    }
    return () => {
      alive = false;
    };
  };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(load, [hostId]);
  useEffect(() => setLimit(PAGE), [tool, q, dir]);

  useEffect(() => {
    setPreview(null);
    if (!sel?.readable) return;
    rpc<HubEvent[]>(hostId, 'history.read', { tool: sel.tool, id: sel.id }).then(setPreview).catch(errorToast);
  }, [sel, hostId]);

  const inDir = useMemo(() => {
    if (!dir) return list ?? [];
    const d = normDir(dir, home);
    return (list ?? []).filter((h) => h.cwd && underDir(normDir(h.cwd, home), d));
  }, [list, dir, home]);

  const filtered = useMemo(() => {
    const ql = q.trim().toLowerCase();
    return inDir.filter(
      (h) => (!tool || h.tool === tool) && (!ql || h.title.toLowerCase().includes(ql) || h.cwd.toLowerCase().includes(ql)),
    );
  }, [inDir, tool, q]);

  const counts = useMemo(() => {
    const c: Record<string, number> = {};
    for (const h of inDir) c[h.tool] = (c[h.tool] ?? 0) + 1;
    return c;
  }, [inDir]);

  const resume = (h: HistoryEntry) =>
    setState({ newSession: { hostId, tool: h.tool, cwd: h.cwd || '~', resumeId: h.id, title: h.title } });

  return (
    <div className={`page split ${sel ? 'has-sel' : ''}`}>
      <div className="split-list">
        <div className="page-head">
          <h2>{tr('历史会话', 'History')}</h2>
          <button className="icon-btn" onClick={() => void load()} title={tr('刷新', 'Refresh')}>
            <Icon name="refresh" size={16} />
          </button>
        </div>
        <p className="muted small">{tr('各工具自己保存的会话记录，可以在这里接着聊。', 'Sessions saved by each tool. Continue one from here.')}</p>
        {dir && (
          <div className="history-dir" title={dir}>
            <Icon name="folder" size={13} />
            <span className="truncate grow">{shortPath(dir, home)}{tr('（含子目录）', ' (including subfolders)')}</span>
            <button className="icon-btn" title={tr('查看全部目录', 'All folders')} onClick={() => setDir('')}>
              <Icon name="x" size={13} />
            </button>
          </div>
        )}
        <div className="row">
          <input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder={dir ? tr('在这个目录的会话里搜索标题', 'Search titles in this folder') : tr('搜索标题或目录', 'Search titles or folders')}
            autoFocus={!!dir}
          />
        </div>
        <div className="seg wrap">
          <button className={tool === '' ? 'active' : ''} onClick={() => setTool('')}>
            {tr('全部', 'All')} {list ? inDir.length : ''}
          </button>
          {HISTORY_TOOLS.map((t) => (
            <button key={t} className={tool === t ? 'active' : ''} onClick={() => setTool(t)}>
              {TOOL_LABEL[t]} {pending.includes(t) ? '…' : (counts[t] ?? 0)}
            </button>
          ))}
        </div>
        {pending.length > 0 && (
          <div className="muted small">{tr('正在读取', 'Reading')} {pending.map((t) => TOOL_LABEL[t]).join(isEn() ? ', ' : '、')}…</div>
        )}
        <div className="list">
          {filtered.slice(0, limit).map((h) => (
            <button key={h.tool + h.id} className={`list-row ${sel?.id === h.id ? 'active' : ''}`} onClick={() => setSel(h)}>
              <ToolBadge tool={h.tool} size={20} />
              <div className="grow min0">
                <div className="truncate">{h.title}</div>
                <div className="muted small truncate">
                  {shortPath(h.cwd, home) || tr('未知目录', 'Unknown folder')} · {timeAgo(h.updatedAt)}
                  {h.messageCount ? tr(` · ${h.messageCount} 条`, ` · ${h.messageCount} msgs`) : ''}
                </div>
              </div>
            </button>
          ))}
          {filtered.length > limit && (
            <button className="btn more-btn" onClick={() => setLimit((n) => n + PAGE)}>
              {tr(`显示更多（还有 ${filtered.length - limit} 条）`, `Show more (${filtered.length - limit} left)`)}
            </button>
          )}
          {list && pending.length === 0 && filtered.length === 0 && (
            <div className="muted pad">{dir && !q && !tool ? tr('这个目录下没有历史会话', 'No history in this folder') : tr('没有记录', 'Nothing here')}</div>
          )}
        </div>
      </div>
      <div className="split-detail">
        {sel ? (
          <>
            <div className="detail-head">
              <button className="icon-btn show-mobile" onClick={() => setSel(null)}>
                <Icon name="x" />
              </button>
              <div className="grow min0">
                <div className="truncate"><b>{sel.title}</b></div>
                <div className="muted small truncate">{sel.cwd}</div>
              </div>
              <button className="btn primary" onClick={() => resume(sel)}>
                <Icon name="play" size={14} /> {tr('继续此会话', 'Continue')}
              </button>
            </div>
            {sel.readable ? (
              preview ? (
                <ChatView events={preview} live={new Map()} onAnswer={() => {}} />
              ) : (
                <div className="muted pad">{tr('加载中…', 'Loading…')}</div>
              )
            ) : (
              <div className="center-msg">
                {tr(`${TOOL_LABEL[sel.tool]} 的历史内容需要继续会话后由工具回放显示。`, `${TOOL_LABEL[sel.tool]} history is shown after you continue the session.`)}
                <button className="btn primary" onClick={() => resume(sel)}>
                  {tr('继续此会话', 'Continue')}
                </button>
              </div>
            )}
          </>
        ) : (
          <div className="center-msg muted">{tr('选择左侧的会话查看内容', 'Select a session to view')}</div>
        )}
      </div>
    </div>
  );
}
