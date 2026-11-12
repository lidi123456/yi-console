import { structuredPatch } from 'diff';
import { memo, useMemo } from 'react';
import { getT, useLang } from '../i18n';
import type { FileDiff } from '../types';
import { basename } from '../util';
import { Icon } from './Icon';

const t = getT('diffView');
const tx = getT('filePanel');

interface Line {
  t: 'add' | 'del' | 'ctx' | 'hunk';
  text: string;
  /** Line number: from the old file for deleted lines, from the new file for the rest */
  n?: number;
}

/** Take the old and new start line numbers from the @@ header, then number every line */
function numbered(raw: Line[]): Line[] {
  let o = 0;
  let nn = 0;
  return raw.map((l) => {
    if (l.t === 'hunk') {
      const m = /^@@ -(\d+)(?:,\d+)? \+(\d+)/.exec(l.text);
      if (m) {
        o = +m[1];
        nn = +m[2];
      }
      return l;
    }
    if (l.t === 'del') return { ...l, n: o++ };
    if (l.t === 'add') return { ...l, n: nn++ };
    o++;
    return { ...l, n: nn++ };
  });
}

export function diffLines(d: FileDiff): Line[] {
  if (d.unified) {
    return numbered(
      // A patch ends with a newline: drop it so it doesn't show up as an extra empty context line
      d.unified
        .replace(/\n$/, '')
        .split('\n')
        // "\ No newline at end of file" is a marker, not a line of the file
        .filter((l) => !l.startsWith('\\'))
        .map((l): Line => {
          if (l.startsWith('@@')) return { t: 'hunk', text: l };
          if (l.startsWith('+++') || l.startsWith('---')) return { t: 'hunk', text: l };
          if (l.startsWith('+')) return { t: 'add', text: l.slice(1) };
          if (l.startsWith('-')) return { t: 'del', text: l.slice(1) };
          return { t: 'ctx', text: l.startsWith(' ') ? l.slice(1) : l };
        }),
    );
  }
  const p = structuredPatch(d.path, d.path, d.oldText ?? '', d.newText ?? '', '', '', { context: 3 });
  const out: Line[] = [];
  for (const h of p.hunks) {
    out.push({ t: 'hunk', text: `@@ -${h.oldStart},${h.oldLines} +${h.newStart},${h.newLines} @@` });
    for (const l of h.lines) {
      if (l.startsWith('\\')) continue;
      out.push({ t: l[0] === '+' ? 'add' : l[0] === '-' ? 'del' : 'ctx', text: l.slice(1) });
    }
  }
  return numbered(out);
}

export const DiffView = memo(function DiffView({ diff }: { diff: FileDiff }) {
  useLang();
  const lines = useMemo(() => diffLines(diff), [diff]);
  const adds = lines.filter((l) => l.t === 'add').length;
  const dels = lines.filter((l) => l.t === 'del').length;
  const isNew = diff.oldText === undefined && !diff.unified;
  return (
    <div className="diff">
      <div className="diff-head" title={diff.path}>
        <span className="mono truncate">{basename(diff.path)}</span>
        {isNew && <span className="pill">{t('newFile')}</span>}
        <span className="diff-stat">
          <span className="add">+{adds}</span> <span className="del">-{dels}</span>
        </span>
      </div>
      <div className="diff-body mono">
        {lines.slice(0, 800).map((l, i) => (
          <div key={i} className={`dl dl-${l.t}`}>
            <span className="dl-sign" />
            <span className="dl-text">{l.text || ' '}</span>
          </div>
        ))}
        {lines.length > 800 && <div className="dl dl-hunk">{t('moreLines', { v: lines.length - 800 })}</div>}
      </div>
    </div>
  );
});

// ---------- Full-page diff in the file panel: unified or side by side (split), with line numbers on both sides ----------

interface PLine {
  t: 'add' | 'del' | 'ctx' | 'hunk';
  text: string;
  o?: number;
  n?: number;
}

function paneLines(d: FileDiff): PLine[] {
  const raw = diffLines(d);
  let o = 0;
  let n = 0;
  return raw
    .filter((l) => !(l.t === 'hunk' && (l.text.startsWith('---') || l.text.startsWith('+++'))))
    .map((l) => {
      if (l.t === 'hunk') {
        const m = /^@@ -(\d+)(?:,\d+)? \+(\d+)/.exec(l.text);
        if (m) {
          o = +m[1];
          n = +m[2];
        }
        return { t: 'hunk', text: l.text };
      }
      if (l.t === 'del') return { t: 'del', text: l.text, o: o++ };
      if (l.t === 'add') return { t: 'add', text: l.text, n: n++ };
      return { t: 'ctx', text: l.text, o: o++, n: n++ };
    });
}

type Cell = { t: PLine['t'] | 'empty'; text: string; num?: number };

const PANE_MAX = 5000;

interface PaneHunkInfo {
  fromLine: number;
  toLine: number;
  added?: number;
  deleted?: number;
  content: string;
}

function buildPaneHunks(lines: PLine[]): Map<number, PaneHunkInfo> {
  const map = new Map<number, PaneHunkInfo>();
  let i = 0;
  while (i < lines.length) {
    if (lines[i].t !== 'hunk') {
      i++;
      continue;
    }
    const headIdx = i;
    const header = lines[i++].text;
    const body: string[] = [header];
    let added = 0;
    let deleted = 0;
    let firstAdd = 0;
    let lastAdd = 0;
    let firstAny = 0;
    while (i < lines.length && lines[i].t !== 'hunk') {
      const l = lines[i++];
      if (l.n && !firstAny) firstAny = l.n;
      if (l.t === 'add') {
        added++;
        if (l.n) {
          if (!firstAdd) firstAdd = l.n;
          lastAdd = l.n;
        }
        body.push(`+${l.text}`);
      } else if (l.t === 'del') {
        deleted++;
        body.push(`-${l.text}`);
      } else {
        body.push(` ${l.text}`);
      }
    }
    if (added || deleted) {
      const fromLine = firstAdd || firstAny || 1;
      const toLine = lastAdd || fromLine;
      map.set(headIdx, {
        fromLine,
        toLine,
        added: added || undefined,
        deleted: deleted || undefined,
        content: body.join('\n'),
      });
    }
  }
  return map;
}

export const PaneDiff = memo(function PaneDiff({
  diff,
  layout,
  onAddHunk,
  onCopyHunk,
}: {
  diff: FileDiff;
  layout: 'unified' | 'split';
  onAddHunk?: (hunk: PaneHunkInfo) => void;
  onCopyHunk?: (content: string) => void;
}) {
  useLang();
  const lines = useMemo(() => paneLines(diff), [diff]);
  const hunks = useMemo(() => buildPaneHunks(lines), [lines]);
  const rows = useMemo(() => {
    if (layout !== 'split') return [];
    // Keep track of original line index for hunk headers
    const out: { pair: [Cell, Cell]; hunk?: PaneHunkInfo }[] = [];
    let i = 0;
    const empty: Cell = { t: 'empty', text: '' };
    while (i < lines.length) {
      const l = lines[i];
      if (l.t === 'hunk') {
        out.push({ pair: [{ t: 'hunk', text: l.text }, { t: 'hunk', text: '' }], hunk: hunks.get(i) });
        i++;
      } else if (l.t === 'ctx') {
        out.push({ pair: [{ t: 'ctx', text: l.text, num: l.o }, { t: 'ctx', text: l.text, num: l.n }] });
        i++;
      } else {
        const dels: PLine[] = [];
        const adds: PLine[] = [];
        while (i < lines.length && lines[i].t === 'del') dels.push(lines[i++]);
        while (i < lines.length && lines[i].t === 'add') adds.push(lines[i++]);
        for (let k = 0; k < Math.max(dels.length, adds.length); k++) {
          const d = dels[k];
          const a = adds[k];
          out.push({ pair: [d ? { t: 'del', text: d.text, num: d.o } : empty, a ? { t: 'add', text: a.text, num: a.n } : empty] });
        }
      }
    }
    return out;
  }, [lines, hunks, layout]);
  const more = (layout === 'split' ? rows.length : lines.length) - PANE_MAX;
  if (!lines.some((l) => l.t !== 'hunk')) return <div className="pd-empty muted small">{t('bothSidesAreTheSame')}</div>;

  const hunkActs = (h?: PaneHunkInfo) =>
    h && (onAddHunk || onCopyHunk) ? (
      <span className="pd-hunk-acts">
        {onAddHunk && (
          <button type="button" className="wp-hunk-btn primary" title={tx('addDiffHunkToChat')} onClick={() => onAddHunk(h)}>
            <Icon name="chat" size={12} />
            <span>{tx('addToChat')}</span>
          </button>
        )}
        {onCopyHunk && (
          <button type="button" className="wp-hunk-btn" title={tx('copyDiffHunk')} onClick={() => onCopyHunk(h.content)}>
            <Icon name="copy" size={12} />
            <span>{tx('copy')}</span>
          </button>
        )}
      </span>
    ) : null;

  return (
    <div className={`pd mono pd-${layout}`}>
      {layout === 'unified'
        ? lines.slice(0, PANE_MAX).map((l, i) =>
            l.t === 'hunk' ? (
              <div key={i} className="pd-row pd-hunk">
                <span className="pd-hunk-text">{l.text}</span>
                {hunkActs(hunks.get(i))}
              </div>
            ) : (
              <div key={i} className={`pd-row pd-${l.t}`}>
                <span className="pd-num">{l.o ?? ''}</span>
                <span className="pd-num">{l.n ?? ''}</span>
                <span className="pd-sign" />
                <span className="pd-text">{l.text || ' '}</span>
              </div>
            ),
          )
        : rows.slice(0, PANE_MAX).map(({ pair: [a, b], hunk }, i) =>
            a.t === 'hunk' ? (
              <div key={i} className="pd-row pd-hunk">
                <span className="pd-hunk-text">{a.text}</span>
                {hunkActs(hunk)}
              </div>
            ) : (
              <div key={i} className="pd-row pd-pair">
                {[a, b].map((c, k) => (
                  <div key={k} className={`pd-half pd-${c.t}`}>
                    <span className="pd-num">{c.num ?? ''}</span>
                    <span className="pd-text">{c.text || ' '}</span>
                  </div>
                ))}
              </div>
            ),
          )}
      {more > 0 && <div className="pd-row pd-hunk">{t('moreLinesNotShown', { more })}</div>}
    </div>
  );
});
