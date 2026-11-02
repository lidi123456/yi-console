import { useEffect, useMemo, useRef, useState } from 'react';
import { tr } from '../i18n';
import { rpc } from '../transport';
import { lsGet, lsSet } from '../util';
import { FileIcon } from './FilePanel';
import { Icon } from './Icon';

// The file panel's "Search": searches file contents in the working directory (yi-agentd's fs.grep; git repos respect .gitignore),
// results grouped by file; clicking a row jumps to that line in the editor and selects the match

interface Hit {
  path: string;
  line: number;
  /** Character positions of the matches within text */
  col: number;
  len: number;
  /** Character offset in the original line where text starts */
  offset: number;
  text: string;
}

interface Result {
  q: string;
  hits: Hit[];
  truncated: boolean;
  files: number;
  error?: string;
}

const base = (p: string) => p.slice(p.lastIndexOf('/') + 1);
const dirOf = (p: string) => (p.includes('/') ? p.slice(0, p.lastIndexOf('/')) : '');

export function GrepPanel({
  hostId,
  root,
  onOpen,
}: {
  hostId: string;
  root: string;
  onOpen: (rel: string, line: number, col: number, len: number) => void;
}) {
  const [q, setQ] = useState(() => lsGet('wp-grep-q', ''));
  const [cs, setCs] = useState(() => lsGet('wp-grep-cs', false));
  const [ww, setWw] = useState(() => lsGet('wp-grep-ww', false));
  const [res, setRes] = useState<Result | null>(null);
  const [busy, setBusy] = useState(false);
  const [folded, setFolded] = useState<Set<string>>(new Set());
  const input = useRef<HTMLInputElement>(null);
  const term = q.trim() ? q : '';

  useEffect(() => input.current?.focus(), []);

  useEffect(() => {
    lsSet('wp-grep-q', q);
    if (!term) {
      setRes(null);
      return;
    }
    let alive = true;
    const t = setTimeout(() => {
      setBusy(true);
      rpc<Omit<Result, 'q'>>(hostId, 'fs.grep', { root, query: term, caseSensitive: cs, wholeWord: ww, limit: 1000 })
        .then((r) => alive && (setRes({ ...r, q: term }), setFolded(new Set())))
        .catch((e) =>
          alive &&
          setRes({
            q: term,
            hits: [],
            truncated: false,
            files: 0,
            error: String((e as Error)?.message ?? e).includes('未知方法') ? tr('这台主机的守护进程太旧，重启守护进程后可用', 'This host’s daemon is too old. Restart it to search.') : String((e as Error)?.message ?? e),
          }),
        )
        .finally(() => alive && setBusy(false));
    }, 300);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [hostId, root, term, cs, ww]);

  const groups = useMemo(() => {
    const m = new Map<string, Hit[]>();
    for (const h of res?.hits ?? []) {
      if (!m.has(h.path)) m.set(h.path, []);
      m.get(h.path)!.push(h);
    }
    return [...m.entries()];
  }, [res]);

  const toggle = (k: 'cs' | 'ww') => {
    if (k === 'cs') {
      lsSet('wp-grep-cs', !cs);
      setCs(!cs);
    } else {
      lsSet('wp-grep-ww', !ww);
      setWw(!ww);
    }
  };

  return (
    <div className="grep">
      <div className="wp-filter grep-input">
        <Icon name="search" size={13} />
        <input
          ref={input}
          value={q}
          onChange={(e) => setQ(e.target.value)}
          placeholder={tr('在文件内容里搜索…', 'Search in files…')}
          aria-label={tr('在文件内容里搜索', 'Search in files')}
          onKeyDown={(e) => e.key === 'Escape' && setQ('')}
        />
        <button className={`grep-opt ${cs ? 'on' : ''}`} title={tr('区分大小写', 'Match case')} onClick={() => toggle('cs')}>
          Aa
        </button>
        <button className={`grep-opt ${ww ? 'on' : ''}`} title={tr('全词匹配', 'Whole word')} onClick={() => toggle('ww')}>
          <u>ab</u>
        </button>
      </div>
      <div className="grep-sum">
        {busy ? (
          <span className="spinner" />
        ) : res && !res.error ? (
          res.hits.length ? (
            <>
              {tr(`${res.files} 个文件里 ${res.hits.length}${res.truncated ? '+' : ''} 处`, `${res.hits.length}${res.truncated ? '+' : ''} in ${res.files} files`)}
              {groups.length > 1 && (
                <button
                  className="link-btn grep-fold"
                  onClick={() => setFolded(folded.size ? new Set() : new Set(groups.map((g) => g[0])))}
                >
                  {folded.size ? tr('全部展开', 'Expand all') : tr('全部收起', 'Collapse all')}
                </button>
              )}
            </>
          ) : (
            tr('没有找到', 'No matches')
          )
        ) : null}
      </div>
      <div className="wp-tree grep-list">
        {res?.error && (
          <div className="wp-tip">
            <b>{tr('搜索失败', 'Search failed')}</b>
            {res.error}
          </div>
        )}
        {!term && <div className="wp-tip">{tr('git 仓库会跳过 .gitignore 里的文件；二进制和超过 2MB 的文件不搜。', 'Git repos skip .gitignore. Binary files and files over 2MB are not searched.')}</div>}
        {groups.map(([path, hits]) => {
          const closed = folded.has(path);
          return (
            <div key={path} className="grep-file">
              <div
                className="wp-row grep-head"
                title={path}
                onClick={() =>
                  setFolded((s) => {
                    const n = new Set(s);
                    if (n.has(path)) n.delete(path);
                    else n.add(path);
                    return n;
                  })
                }
              >
                <span className="wp-twist">
                  <span className={`chev ${closed ? '' : 'open'}`}>
                    <Icon name="chevron" size={12} />
                  </span>
                </span>
                <FileIcon name={base(path)} />
                <span className="truncate grep-name">{base(path)}</span>
                <span className="truncate grep-dir">{dirOf(path)}</span>
                <span className="grep-n">{hits.length}</span>
              </div>
              {!closed &&
                hits.map((h) => {
                  const chars = [...h.text];
                  // Keep only a short lead-in before the match so it stays visible in a narrow panel
                  const head = chars.slice(0, h.col).join('').replace(/^\s+/, '');
                  const pre = head.length > 16 ? '…' + head.slice(-14) : head;
                  const mid = chars.slice(h.col, h.col + h.len).join('');
                  const post = chars.slice(h.col + h.len).join('');
                  return (
                    <div
                      key={`${h.line}:${h.col}`}
                      className="wp-row grep-hit"
                      title={`${path}:${h.line}`}
                      onClick={() => onOpen(path, h.line, h.offset + h.col, h.len)}
                    >
                      <span className="grep-line">{h.line}</span>
                      <span className="truncate grep-text">
                        {pre}
                        <mark>{mid}</mark>
                        {post}
                      </span>
                    </div>
                  );
                })}
            </div>
          );
        })}
        {res?.truncated && <div className="wp-limit">{tr(`结果太多，只显示前 ${res.hits.length} 处，请换个更具体的词。`, `Too many results. Showing the first ${res.hits.length}. Try a more specific term.`)}</div>}
      </div>
    </div>
  );
}
