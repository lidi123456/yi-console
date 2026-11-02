import { useEffect, useMemo, useRef, useState } from 'react';
import { tr, useLang } from '../i18n';
import { rpc } from '../transport';
import { fuzzyScore } from '../util';
import { KIND_LABEL, outline } from '../outline';
import { FileIcon } from './FilePanel';
import { Icon } from './Icon';

// 文件面板的快速打开（Ctrl+P），照 VS Code：
//   直接打字：按文件名模糊找；@ 开头：当前文件里的函数、类、标题；: 开头：跳到第几行

export type QuickMode = '' | '@' | ':';

interface Item {
  key: string;
  label: string;
  detail?: string;
  badge?: string;
  file?: string;
  depth?: number;
  run: () => void;
}

const base = (p: string) => p.slice(p.lastIndexOf('/') + 1);
const dirOf = (p: string) => (p.includes('/') ? p.slice(0, p.lastIndexOf('/')) : '');

export function QuickOpen({
  hostId,
  root,
  recent,
  current,
  initial,
  onOpen,
  onGoto,
  onClose,
}: {
  hostId: string;
  root: string;
  /** 打开过的文件（相对路径），没输入时列出来 */
  recent: string[];
  /** 当前文件：@ 和 : 用 */
  current: { name: string; text: string; lines: number } | null;
  initial: QuickMode;
  /** rel 是相对 root 的路径 */
  onOpen: (rel: string) => void;
  onGoto: (line: number) => void;
  onClose: () => void;
}) {
  const [, lang] = useLang();
  const [q, setQ] = useState<string>(initial);
  const [files, setFiles] = useState<string[] | null>(null);
  const [sel, setSel] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const mode: QuickMode = q.startsWith('@') ? '@' : q.startsWith(':') ? ':' : '';
  const term = mode ? q.slice(1).trim() : q.trim();

  useEffect(() => input.current?.focus(), []);

  // 找文件：停手 120ms 再问
  useEffect(() => {
    if (mode || !term) {
      setFiles(null);
      return;
    }
    let alive = true;
    const t = setTimeout(() => {
      rpc<{ path: string; isDir: boolean }[]>(hostId, 'fs.search', { root, query: term, limit: 60 })
        .then((r) => alive && setFiles(r.filter((x) => !x.isDir).map((x) => x.path)))
        .catch(() => alive && setFiles([]));
    }, 120);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [hostId, root, term, mode]);

  const symbols = useMemo(() => (current && mode === '@' ? outline(current.name, current.text) : []), [current, mode]);

  const items: Item[] = useMemo(() => {
    if (mode === ':') {
      const n = parseInt(term, 10);
      if (!current) return [];
      if (!n) return [{ key: 'tip', label: tr(`输入行号（1–${current.lines}）`, `Enter a line (1–${current.lines})`), run: () => {} }];
      const line = Math.min(Math.max(1, n), current.lines);
      return [{ key: 'line', label: tr(`跳到第 ${line} 行`, `Go to line ${line}`), detail: current.name, run: () => onGoto(line) }];
    }
    if (mode === '@') {
      const hits = term
        ? symbols
            .map((s) => ({ s, sc: fuzzyScore(s.name, term) }))
            .filter((x) => x.sc >= 0)
            .sort((a, b) => b.sc - a.sc)
            .map((x) => x.s)
        : symbols;
      return hits.slice(0, 300).map((s) => ({
        key: `${s.line}:${s.name}`,
        label: s.name,
        detail: tr(`第 ${s.line} 行`, `Line ${s.line}`),
        badge: KIND_LABEL[s.kind] ?? s.kind[0],
        depth: term ? 0 : s.depth,
        run: () => onGoto(s.line),
      }));
    }
    const paths = term ? files ?? [] : recent;
    return paths.map((p) => ({ key: p, label: base(p), detail: dirOf(p), file: p, run: () => onOpen(p) }));
  }, [mode, term, files, recent, symbols, current, onGoto, onOpen, lang]);

  useEffect(() => setSel(0), [q]);
  useEffect(() => {
    list.current?.querySelector('.qo-item.sel')?.scrollIntoView({ block: 'nearest' });
  }, [sel]);

  const pick = (it?: Item) => {
    if (!it || it.key === 'tip') return;
    it.run();
    onClose();
  };

  const empty =
    mode === '@'
      ? !current
        ? tr('先打开一个文件', 'Open a file first')
        : symbols.length === 0
          ? tr('这个文件里没找到函数、类或标题', 'No functions, classes, or headings in this file')
          : tr('没有匹配的符号', 'No matching symbols')
      : mode === ':'
        ? tr('先打开一个文件', 'Open a file first')
        : term
          ? files === null
            ? tr('查找中…', 'Searching…')
            : tr('没有匹配的文件', 'No matching files')
          : tr('输入文件名查找', 'Type a file name');

  return (
    <div className="qo-scrim" onMouseDown={onClose}>
      <div className="qo" onMouseDown={(e) => e.stopPropagation()} role="dialog" aria-label={tr('快速打开', 'Quick open')}>
        <div className="qo-input">
          <Icon name="search" size={14} />
          <input
            ref={input}
            value={q}
            placeholder={tr('按文件名查找；@ 找函数和标题；: 跳到行', 'File name; @ symbols; : line')}
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Escape') {
                e.preventDefault();
                onClose();
              } else if (e.key === 'ArrowDown') {
                e.preventDefault();
                setSel((s) => Math.min(items.length - 1, s + 1));
              } else if (e.key === 'ArrowUp') {
                e.preventDefault();
                setSel((s) => Math.max(0, s - 1));
              } else if (e.key === 'Enter') {
                e.preventDefault();
                pick(items[sel]);
              }
            }}
          />
        </div>
        <div className="qo-modes">
          {(
            [
              ['', tr('文件', 'Files')],
              ['@', tr('@ 符号', '@ Symbols')],
              [':', tr(': 行号', ': Line')],
            ] as [QuickMode, string][]
          ).map(([m, l]) => (
            <button
              key={l}
              className={mode === m ? 'active' : ''}
              onClick={() => {
                setQ(m + term);
                input.current?.focus();
              }}
            >
              {l}
            </button>
          ))}
          {!term && mode === '' && recent.length > 0 && <span className="muted small qo-hint">{tr('最近打开', 'Recent')}</span>}
        </div>
        <div className="qo-list" ref={list}>
          {items.length === 0 ? (
            <div className="qo-empty">{empty}</div>
          ) : (
            items.map((it, i) => (
              <div
                key={it.key}
                className={`qo-item ${i === sel ? 'sel' : ''}`}
                style={it.depth ? { paddingLeft: 10 + it.depth * 14 } : undefined}
                onMouseMove={() => setSel(i)}
                onClick={() => pick(it)}
              >
                {it.file ? <FileIcon name={it.label} /> : it.badge ? <span className="qo-badge">{it.badge}</span> : null}
                <span className="qo-label truncate">{it.label}</span>
                {it.detail && <span className="qo-detail truncate">{it.detail}</span>}
              </div>
            ))
          )}
        </div>
      </div>
    </div>
  );
}
