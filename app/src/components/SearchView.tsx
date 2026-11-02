import { useEffect, useRef, useState, type ReactNode } from 'react';
import { tr } from '../i18n';
import { navigate, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type SearchHit } from '../types';
import { basename, timeAgo } from '../util';
import { Icon, ToolBadge } from './Icon';

interface Group {
  hostId: string;
  hostName: string;
  sessionId: string;
  title: string;
  tool: SearchHit['tool'];
  cwd: string;
  hits: SearchHit[];
}

/** 命中的词加粗；先转义再拼，避免把消息内容当 HTML */
function Highlight({ text, q }: { text: string; q: string }) {
  if (!q) return <>{text}</>;
  const parts: ReactNode[] = [];
  const lower = text.toLowerCase();
  const ql = q.toLowerCase();
  let i = 0;
  let n = 0;
  for (;;) {
    const j = lower.indexOf(ql, i);
    if (j < 0) break;
    parts.push(text.slice(i, j), <mark key={n++}>{text.slice(j, j + q.length)}</mark>);
    i = j + q.length;
  }
  parts.push(text.slice(i));
  return <>{parts}</>;
}

export function SearchView({ initial }: { initial?: string }) {
  const hosts = useStore((s) => s.hosts);
  const [q, setQ] = useState(initial ?? '');
  const [groups, setGroups] = useState<Group[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [errors, setErrors] = useState<string[]>([]);
  const seq = useRef(0);
  const connected = hosts.filter((h) => h.status.status === 'connected');

  const run = async (query: string) => {
    const my = ++seq.current;
    if (!query.trim()) {
      setGroups(null);
      return;
    }
    setBusy(true);
    const errs: string[] = [];
    const results = await Promise.all(
      connected.map((h) =>
        rpc<SearchHit[]>(h.config.id, 'search.messages', { query: query.trim(), limit: 100 })
          .then((hits) => ({ h, hits }))
          .catch((e) => {
            errs.push(`${h.config.name}: ${String(e?.message ?? e).includes('未知方法') ? tr('守护进程版本太旧，重启后才能搜索', 'Daemon is too old. Restart it to search.') : e?.message ?? e}`);
            return { h, hits: [] as SearchHit[] };
          }),
      ),
    );
    if (my !== seq.current) return;
    const map = new Map<string, Group>();
    for (const { h, hits } of results) {
      for (const hit of hits) {
        const k = `${h.config.id}::${hit.sessionId}`;
        let g = map.get(k);
        if (!g) {
          g = { hostId: h.config.id, hostName: h.config.name, sessionId: hit.sessionId, title: hit.title, tool: hit.tool, cwd: hit.cwd, hits: [] };
          map.set(k, g);
        }
        g.hits.push(hit);
      }
    }
    // 最近命中的会话在前
    setGroups([...map.values()].sort((a, b) => b.hits[0].ts - a.hits[0].ts));
    setErrors(errs);
    setBusy(false);
  };

  useEffect(() => {
    const t = setTimeout(() => run(q), 300);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [q, connected.length]);

  const total = groups?.reduce((n, g) => n + g.hits.length, 0) ?? 0;
  return (
    <div className="page">
      <div className="page-head">
        <h2>{tr('搜索消息', 'Search messages')}</h2>
      </div>
      <div className="search-box">
        <Icon name="search" size={16} />
        <input autoFocus value={q} placeholder={tr('搜索所有主机上会话里的用户消息和回复', 'Search user messages and replies on all hosts')} onChange={(e) => setQ(e.target.value)} />
        {busy && <span className="spinner" />}
      </div>
      {errors.map((e) => (
        <div key={e} className="muted small">
          {e}
        </div>
      ))}
      {groups && (
        <div className="muted small search-sum">
          {total
            ? tr(
                `${groups.length} 个会话里找到 ${total} 处${total >= 100 ? '（每台主机最多显示 100 处）' : ''}`,
                `${total} hits in ${groups.length} sessions${total >= 100 ? ' (up to 100 per host)' : ''}`,
              )
            : tr('没有找到', 'No matches')}
        </div>
      )}
      {!groups && <div className="muted small">{tr('只搜 Yi Console 里的会话；还没导入的工具历史，可以在「历史」里先打开。', 'Only Yi Console sessions are searched. Open a tool history from History first if it has not been imported.')}</div>}
      {groups?.map((g) => (
        <div key={g.hostId + g.sessionId} className="card search-group">
          <button className="search-session" onClick={() => navigate({ kind: 'session', hostId: g.hostId, sessionId: g.sessionId })}>
            <ToolBadge tool={g.tool} size={18} />
            <span className="truncate">{g.title || tr('新会话', 'New session')}</span>
            <span className="muted small truncate">
              {TOOL_LABEL[g.tool]} · {g.hostName} · {basename(g.cwd)}
            </span>
          </button>
          {g.hits.slice(0, 5).map((h) => (
            <button
              key={h.seq}
              className="search-hit"
              onClick={() => navigate({ kind: 'session', hostId: g.hostId, sessionId: g.sessionId, seq: h.seq })}
            >
              <span className={`pill ${h.kind === 'user' ? '' : 'pill-ok'}`}>{h.kind === 'user' ? tr('我', 'Me') : 'AI'}</span>
              <span className="search-snippet">
                <Highlight text={h.snippet} q={q.trim()} />
              </span>
              <span className="muted small">{timeAgo(h.ts)}</span>
            </button>
          ))}
          {g.hits.length > 5 && <div className="muted small pad-x">{tr(`还有 ${g.hits.length - 5} 处`, `${g.hits.length - 5} more`)}</div>}
        </div>
      ))}
    </div>
  );
}
