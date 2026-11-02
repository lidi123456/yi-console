import { structuredPatch } from 'diff';
import { memo, useMemo } from 'react';
import { tr, useLang } from '../i18n';
import type { FileDiff } from '../types';
import { basename } from '../util';

interface Line {
  t: 'add' | 'del' | 'ctx' | 'hunk';
  text: string;
  /** 行号：删除行是旧文件的，其余是新文件的 */
  n?: number;
}

/** 从 @@ 头里取新旧起始行号，再给每行编号 */
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
      d.unified.split('\n').map((l): Line => {
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
        {isNew && <span className="pill">{tr('新文件', 'New file')}</span>}
        <span className="diff-stat">
          <span className="add">+{adds}</span> <span className="del">-{dels}</span>
        </span>
      </div>
      <div className="diff-body mono">
        {lines.slice(0, 800).map((l, i) => (
          <div key={i} className={`dl dl-${l.t}`}>
            <span className="dl-sign">{l.t === 'add' ? '+' : l.t === 'del' ? '-' : l.t === 'hunk' ? '' : ' '}</span>
            <span className="dl-text">{l.text || ' '}</span>
          </div>
        ))}
        {lines.length > 800 && <div className="dl dl-hunk">{tr(`… 还有 ${lines.length - 800} 行`, `… ${lines.length - 800} more lines`)}</div>}
      </div>
    </div>
  );
});

// ---------- 文件面板里的整页 diff：上下（unified）或左右并排（split），两边都有行号 ----------

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

/** 并排：上下文两边都放；连续的删除和新增配成一行行左右对照 */
function splitRows(lines: PLine[]): [Cell, Cell][] {
  const rows: [Cell, Cell][] = [];
  let i = 0;
  const empty: Cell = { t: 'empty', text: '' };
  while (i < lines.length) {
    const l = lines[i];
    if (l.t === 'hunk') {
      rows.push([{ t: 'hunk', text: l.text }, { t: 'hunk', text: '' }]);
      i++;
    } else if (l.t === 'ctx') {
      rows.push([
        { t: 'ctx', text: l.text, num: l.o },
        { t: 'ctx', text: l.text, num: l.n },
      ]);
      i++;
    } else {
      const dels: PLine[] = [];
      const adds: PLine[] = [];
      while (i < lines.length && lines[i].t === 'del') dels.push(lines[i++]);
      while (i < lines.length && lines[i].t === 'add') adds.push(lines[i++]);
      for (let k = 0; k < Math.max(dels.length, adds.length); k++) {
        const d = dels[k];
        const a = adds[k];
        rows.push([d ? { t: 'del', text: d.text, num: d.o } : empty, a ? { t: 'add', text: a.text, num: a.n } : empty]);
      }
    }
  }
  return rows;
}

const PANE_MAX = 5000;

export const PaneDiff = memo(function PaneDiff({ diff, layout }: { diff: FileDiff; layout: 'unified' | 'split' }) {
  const lines = useMemo(() => paneLines(diff), [diff]);
  const rows = useMemo(() => (layout === 'split' ? splitRows(lines) : []), [lines, layout]);
  const more = (layout === 'split' ? rows.length : lines.length) - PANE_MAX;
  if (!lines.some((l) => l.t !== 'hunk')) return <div className="pd-empty muted small">{tr('两边内容一样，没有差异。', 'Both sides are the same.')}</div>;
  return (
    <div className={`pd mono pd-${layout}`}>
      {layout === 'unified'
        ? lines.slice(0, PANE_MAX).map((l, i) =>
            l.t === 'hunk' ? (
              <div key={i} className="pd-row pd-hunk">
                {l.text}
              </div>
            ) : (
              <div key={i} className={`pd-row pd-${l.t}`}>
                <span className="pd-num">{l.o ?? ''}</span>
                <span className="pd-num">{l.n ?? ''}</span>
                <span className="pd-sign">{l.t === 'add' ? '+' : l.t === 'del' ? '-' : ''}</span>
                <span className="pd-text">{l.text || ' '}</span>
              </div>
            ),
          )
        : rows.slice(0, PANE_MAX).map(([a, b], i) =>
            a.t === 'hunk' ? (
              <div key={i} className="pd-row pd-hunk">
                {a.text}
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
      {more > 0 && <div className="pd-row pd-hunk">{tr(`… 还有 ${more} 行没显示`, `… ${more} more lines not shown`)}</div>}
    </div>
  );
});
