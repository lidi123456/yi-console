import { useEffect, useMemo, useRef, useState } from 'react';
import { getT, useLang } from '../i18n';
import { rpc } from '../transport';
import { fuzzyScore } from '../util';
import { KIND_LABEL, outline } from '../outline';
import { FileIcon } from './FilePanel';
import { Icon } from './Icon';

const t = getT('quickOpen');

// Quick open in the file panel (Ctrl+P), following VS Code:
//   plain text: fuzzy match on file names; starting with @: functions, classes, headings in the current file; starting with :: go to a line

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
  /** Opened files (relative paths), listed when nothing is typed */
  recent: string[];
  /** Current file: used by @ and : */
  current: { name: string; text: string; lines: number } | null;
  initial: QuickMode;
  /** rel is the path relative to root */
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

  // Finding files: ask 120 ms after typing stops
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
      if (!n) return [{ key: 'tip', label: t('enterALine1', { lines: current.lines }), run: () => {} }];
      const line = Math.min(Math.max(1, n), current.lines);
      return [{ key: 'line', label: t('goToLine', { line }), detail: current.name, run: () => onGoto(line) }];
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
        detail: t('line', { line: s.line }),
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
        ? t('openAFileFirst')
        : symbols.length === 0
          ? t('noFunctionsClassesOrHeadings')
          : t('noMatchingSymbols')
      : mode === ':'
        ? t('openAFileFirst')
        : term
          ? files === null
            ? t('searching')
            : t('noMatchingFiles')
          : t('typeAFileName');

  return (
    <div className="qo-scrim" onMouseDown={onClose}>
      <div className="qo" onMouseDown={(e) => e.stopPropagation()} role="dialog" aria-label={t('quickOpen')}>
        <div className="qo-input">
          <Icon name="search" size={14} />
          <input
            ref={input}
            value={q}
            placeholder={t('fileNameSymbolsLine')}
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
              ['', t('files')],
              ['@', t('symbols')],
              [':', t('line2')],
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
          {!term && mode === '' && recent.length > 0 && <span className="muted small qo-hint">{t('recent')}</span>}
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
