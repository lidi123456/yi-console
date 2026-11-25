import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { getT } from '../i18n';
import { errorToast, toast } from '../store';
import { rpc } from '../transport';
import { lsGet, lsSet } from '../util';
import { FileIcon } from './FilePanel';
import { Icon } from './Icon';
import { askConfirm } from './Overlay';

const tx = getT('grepPanel');

// The file panel's "Search", modelled on VS Code's search view: searches file contents in the working directory
// (yi-agentd's fs.grep; git repos respect .gitignore), results grouped by file, one row per match. Clicking a match
// jumps to it in the editor; with the replace box open it shows a preview diff instead. Replace (fs.replace) only
// touches the results listed here, skips files changed since the search, and can be undone once

/** [column in the line (characters), length, matched text, replacement (when searched with replace)] */
type RawMatch = [number, number, string, string?];

interface Hit {
  path: string;
  line: number;
  /** First match within text (older yi-agentd sends only this) */
  col: number;
  len: number;
  /** Character offset in the original line where text starts */
  offset: number;
  text: string;
  matches?: RawMatch[];
}

interface Result {
  q: string;
  hits: Hit[];
  truncated: boolean;
  files: number;
  /** [modified, size] per file, sent back with replace so changed files are skipped */
  stat?: Record<string, [number, number]>;
  /** Whether the search included the replace text (rows then show the replacement) */
  withRep?: boolean;
  error?: string;
}

interface Item {
  key: string;
  path: string;
  line: number;
  col: number;
  len: number;
  text: string;
  rep?: string;
  hit: Hit;
  pre: string;
  mid: string;
  post: string;
}

type GrepRow =
  | { kind: 'head'; key: string; path: string; name: string; dir: string; closed: boolean; items: Item[] }
  | { kind: 'hit'; key: string; path: string; item: Item };

interface Undo {
  files: { abs: string; before: string; modified: number; size: number }[];
  draft?: { path: string; rev: number; before: string; after: string };
}

interface ReplaceOut {
  files: { path: string; abs: string; count: number; before?: string | null; after?: string; modified?: number; size?: number }[];
  skipped: { path: string; reason: string }[];
}

const base = (p: string) => p.slice(p.lastIndexOf('/') + 1);
const dirOf = (p: string) => (p.includes('/') ? p.slice(0, p.lastIndexOf('/')) : '');
const keyOf = (i: { path: string; line: number; col: number }) => `${i.path}\n${i.line}\n${i.col}`;
/** "Find in folder" request already applied (kept across remounts so an old request isn't applied again) */
let appliedInclude = 0;
let appliedQuery = 0;

/** Input history like VS Code's search inputs: Up / Down step through earlier entries */
function useHistory(key: string, value: string, set: (v: string) => void) {
  const pos = useRef<number | null>(null);
  const push = () => {
    const v = value.trim();
    if (!v) return;
    const h = lsGet<string[]>(key, []).filter((x) => x !== v);
    lsSet(key, [...h, v].slice(-20));
    pos.current = null;
  };
  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return;
    const h = lsGet<string[]>(key, []);
    if (!h.length) return;
    e.preventDefault();
    let i = pos.current ?? h.length;
    if (pos.current == null && h[h.length - 1] === value) i = h.length - 1;
    i = e.key === 'ArrowUp' ? Math.max(0, i - 1) : i + 1;
    if (i >= h.length) {
      pos.current = null;
      set('');
    } else {
      pos.current = i;
      set(h[i]);
    }
  };
  return { push, onKeyDown };
}

export function GrepPanel({
  hostId,
  root,
  visible = true,
  onOpen,
  openFiles,
  dirty,
  getLiveDirty,
  includeRequest,
  queryRequest,
  onPreview,
  onDraft,
  onReplaced,
}: {
  hostId: string;
  root: string;
  /** False while FilePanel is hidden via CSS (pauses F4 shortcuts and focus stealing) */
  visible?: boolean;
  onOpen: (rel: string, line: number, col: number, len: number) => void;
  /** Open file tabs under root (relative paths), for "search only in open files" */
  openFiles: string[];
  /** The open file with unsaved edits: replacing in it changes the draft, as VS Code does with dirty editors */
  dirty: { rel: string; draft: string } | null;
  /** Read current live editor draft synchronously (including unsynced edits within the 200ms debounce window) */
  getLiveDirty?: () => { path: string; rel: string; rev: number; draft: string } | null;
  /** "Find in folder" from the tree: fill "files to include" */
  includeRequest: { value: string; n: number } | null;
  /** Jump from Peek References: populate query and flags */
  queryRequest?: { query: string; caseSensitive?: boolean; wholeWord?: boolean; regex?: boolean; n: number } | null;
  /** Show the file with its replacements applied as a diff */
  onPreview: (rel: string, line: number, col: number, len: number, after: string) => void;
  /** Replace the unsaved draft of the open file; returns new rev or null if stale/changed */
  onDraft: (next: string, expect: { path: string; rev: number; draft: string }) => number | null;
  /** Files written by replace or undo (absolute paths): refresh the tree, git status and open files */
  onReplaced: (abs: string[]) => void;
}) {
  const [q, setQ] = useState(() => lsGet('wp-grep-q', ''));
  const [cs, setCs] = useState(() => lsGet('wp-grep-cs', false));
  const [ww, setWw] = useState(() => lsGet('wp-grep-ww', false));
  const [re, setRe] = useState(() => lsGet('wp-grep-re', false));
  // Files to include / exclude (comma-separated globs, as in VS Code), always shown
  const [inc, setInc] = useState(() => lsGet('wp-grep-inc', ''));
  const [exc, setExc] = useState(() => lsGet('wp-grep-exc', ''));
  const [openOnly, setOpenOnly] = useState(() => lsGet('wp-grep-open', false));
  const [showRep, setShowRep] = useState(() => lsGet('wp-grep-rep-open', false));
  const [rep, setRep] = useState(() => lsGet('wp-grep-rep', ''));
  const [pc, setPc] = useState(() => lsGet('wp-grep-pc', false));
  const [res, setRes] = useState<Result | null>(null);
  const [busy, setBusy] = useState(false);
  const [working, setWorking] = useState(false);
  const [folded, setFolded] = useState<Set<string>>(new Set());
  // Results the user removed from the list (VS Code's "Dismiss"): not replaced
  const [dismissed, setDismissed] = useState<Set<string>>(new Set());
  const [gone, setGone] = useState<Set<string>>(new Set());
  const [cur, setCur] = useState<string | null>(null);
  const [nonce, setNonce] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const rowsBoxRef = useRef<HTMLDivElement>(null);
  const pendingScrollKeyRef = useRef<string | null>(null);
  const term = q.trim() ? q : '';
  const hq = useHistory('wp-grep-hist', q, setQ);
  const hr = useHistory('wp-grep-rep-hist', rep, setRep);
  const hi = useHistory('wp-grep-inc-hist', inc, setInc);
  const he = useHistory('wp-grep-exc-hist', exc, setExc);
  const openKey = openOnly ? openFiles.join('\n') : '';

  useEffect(() => {
    if (visible) input.current?.focus();
  }, [visible]);

  useEffect(() => {
    if (!includeRequest || includeRequest.n <= appliedInclude) return;
    appliedInclude = includeRequest.n;
    setInc(includeRequest.value);
    input.current?.focus();
  }, [includeRequest?.n]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (!queryRequest || queryRequest.n <= appliedQuery) return;
    appliedQuery = queryRequest.n;
    if (queryRequest.caseSensitive != null) {
      lsSet('wp-grep-cs', queryRequest.caseSensitive);
      setCs(queryRequest.caseSensitive);
    }
    if (queryRequest.wholeWord != null) {
      lsSet('wp-grep-ww', queryRequest.wholeWord);
      setWw(queryRequest.wholeWord);
    }
    if (queryRequest.regex != null) {
      lsSet('wp-grep-re', queryRequest.regex);
      setRe(queryRequest.regex);
    }
    setQ(queryRequest.query);
    input.current?.focus();
  }, [queryRequest?.n]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    lsSet('wp-grep-q', q);
    lsSet('wp-grep-inc', inc);
    lsSet('wp-grep-exc', exc);
    lsSet('wp-grep-rep', rep);
    if (!term) {
      // The pending search was cancelled by the cleanup below, so its finally won't clear busy
      setRes(null);
      setBusy(false);
      return;
    }
    let alive = true;
    const t = setTimeout(() => {
      setBusy(true);
      const range = { include: inc.trim(), exclude: exc.trim(), ...(openOnly ? { paths: openFiles } : {}) };
      const withRep = showRep;
      rpc<Omit<Result, 'q'>>(hostId, 'fs.grep', {
        root,
        query: term,
        caseSensitive: cs,
        wholeWord: ww,
        regex: re,
        ...range,
        ...(withRep ? { replace: rep, preserveCase: pc } : {}),
        limit: 2000,
      })
        .then((r) => {
          if (!alive) return;
          setRes({ ...r, q: term, withRep });
          setDismissed(new Set());
          setGone(new Set());
        })
        .catch(
          (e) =>
            alive &&
            setRes({
              q: term,
              hits: [],
              truncated: false,
              files: 0,
              error: String((e as Error)?.message ?? e).includes('未知方法') ? tx('thisHostSDaemonIs') : String((e as Error)?.message ?? e),
            }),
        )
        .finally(() => alive && setBusy(false));
    }, 300);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [hostId, root, term, cs, ww, re, inc, exc, openOnly, openKey, showRep, showRep && rep, showRep && pc, nonce]); // eslint-disable-line react-hooks/exhaustive-deps

  // One item per match, grouped by file, without dismissed ones (precompute display segments once)
  const groups = useMemo(() => {
    const m = new Map<string, Item[]>();
    for (const h of res?.hits ?? []) {
      if (gone.has(h.path)) continue;
      const chars = [...h.text];
      const raw: RawMatch[] = h.matches ?? [[h.offset + h.col, h.len, chars.slice(h.col, h.col + h.len).join('')]];
      for (const [col, len, text, r] of raw) {
        const key = `${h.path}\n${h.line}\n${col}`;
        if (dismissed.has(key)) continue;
        const c = col - h.offset;
        const inside = c >= 0 && c < chars.length;
        // Keep only a short lead-in before the match so it stays visible in a narrow panel
        const head = inside ? chars.slice(0, c).join('').replace(/^\s+/, '') : '';
        const pre = !inside ? '…' : head.length > 16 ? '…' + head.slice(-14) : head;
        const mid = inside ? chars.slice(c, c + len).join('') : text;
        const post = inside ? chars.slice(c + len).join('') : '';
        const it: Item = { key, path: h.path, line: h.line, col, len, text, rep: r, hit: h, pre, mid, post };
        if (!m.has(h.path)) m.set(h.path, []);
        m.get(h.path)!.push(it);
      }
    }
    return [...m.entries()];
  }, [res, dismissed, gone]);
  const flat = useMemo(() => groups.flatMap(([, items]) => items), [groups]);
  const flatRows = useMemo(() => {
    const out: GrepRow[] = [];
    for (const [path, items] of groups) {
      const closed = folded.has(path);
      out.push({ kind: 'head', key: `h:${path}`, path, name: base(path), dir: dirOf(path), closed, items });
      if (!closed) {
        for (const item of items) out.push({ kind: 'hit', key: item.key, path, item });
      }
    }
    return out;
  }, [groups, folded]);
  const [rowHeights, setRowHeights] = useState(() =>
    typeof window !== 'undefined' && window.matchMedia('(max-width: 760px)').matches ? { head: 34, hit: 32 } : { head: 26, hit: 24 },
  );
  const rowHeightsRef = useRef(rowHeights);
  rowHeightsRef.current = rowHeights;
  const offsets = useMemo(() => {
    const arr = new Array<number>(flatRows.length + 1);
    arr[0] = 0;
    for (let i = 0; i < flatRows.length; i++) {
      arr[i + 1] = arr[i] + (flatRows[i].kind === 'head' ? rowHeights.head : rowHeights.hit);
    }
    return arr;
  }, [flatRows, rowHeights]);
  const virtual = flatRows.length > 120;
  const OVERSCAN = 20;
  const [win, setWin] = useState<[number, number]>([0, 60]);

  const scrollToRowIndex = useCallback(
    (idx: number) => {
      const el = list.current;
      const box = rowsBoxRef.current;
      if (!el || !box || idx < 0 || idx >= flatRows.length) return;
      const boxTop = box.offsetTop;
      const rowTop = boxTop + offsets[idx];
      const rowBot = boxTop + offsets[idx + 1];
      if (rowTop < el.scrollTop) el.scrollTop = rowTop;
      else if (rowBot > el.scrollTop + el.clientHeight) el.scrollTop = rowBot - el.clientHeight;
    },
    [flatRows.length, offsets],
  );

  useLayoutEffect(() => {
    const el = list.current;
    const box = rowsBoxRef.current;
    if (!virtual || !el || !box) return;
    let raf = 0;
    const findRow = (y: number) => {
      let lo = 0;
      let hi = flatRows.length;
      while (lo < hi) {
        const mid = (lo + hi) >> 1;
        if (offsets[mid + 1] <= y) lo = mid + 1;
        else hi = mid;
      }
      return lo;
    };
    const calc = () => {
      const hEl = box.querySelector<HTMLElement>('.grep-head');
      const mEl = box.querySelector<HTMLElement>('.grep-hit');
      const nextHead = hEl?.offsetHeight || rowHeightsRef.current.head;
      const nextHit = mEl?.offsetHeight || rowHeightsRef.current.hit;
      if (nextHead !== rowHeightsRef.current.head || nextHit !== rowHeightsRef.current.hit) {
        setRowHeights({ head: nextHead, hit: nextHit });
        return;
      }
      const boxTop = box.offsetTop;
      const viewTop = Math.max(0, el.scrollTop - boxTop);
      const viewBot = Math.max(0, el.scrollTop + el.clientHeight - boxTop);
      const from = Math.max(0, findRow(viewTop) - OVERSCAN);
      const to = Math.min(flatRows.length, findRow(viewBot) + 1 + OVERSCAN);
      setWin((p) => (p[0] === from && p[1] === to ? p : [from, to]));
    };
    const onScroll = () => {
      if (raf) return;
      raf = requestAnimationFrame(() => {
        raf = 0;
        calc();
      });
    };
    calc();
    el.addEventListener('scroll', onScroll, { passive: true });
    const ro = new ResizeObserver(onScroll);
    ro.observe(el);
    return () => {
      el.removeEventListener('scroll', onScroll);
      ro.disconnect();
      if (raf) cancelAnimationFrame(raf);
    };
  }, [virtual, flatRows.length, offsets]);

  useLayoutEffect(() => {
    const k = pendingScrollKeyRef.current;
    if (!k) return;
    const idx = flatRows.findIndex((r) => r.kind === 'hit' && r.key === k);
    if (idx < 0) return;
    pendingScrollKeyRef.current = null;
    scrollToRowIndex(idx);
  }, [flatRows, scrollToRowIndex]);

  const total = flat.length;
  const previewing = showRep && !!res?.withRep;
  const canReplace = previewing && total > 0 && !working && !busy;

  const toggle = (k: 'cs' | 'ww' | 're' | 'open' | 'pc') => {
    const [v, set] = ({ cs: [cs, setCs], ww: [ww, setWw], re: [re, setRe], open: [openOnly, setOpenOnly], pc: [pc, setPc] } as const)[k] as [
      boolean,
      (b: boolean) => void,
    ];
    lsSet(`wp-grep-${k}`, !v);
    set(!v);
  };
  const flipRep = () => {
    lsSet('wp-grep-rep-open', !showRep);
    setShowRep(!showRep);
  };

  const opts = () => ({
    root,
    query: res!.q,
    caseSensitive: cs,
    wholeWord: ww,
    regex: re,
    replace: rep,
    preserveCase: pc,
  });

  const replaceIn = async (items: Item[]) => {
    if (!res || !items.length || working) return;
    hq.push();
    hr.push();
    const byFile = new Map<string, Item[]>();
    for (const i of items) byFile.set(i.path, [...(byFile.get(i.path) ?? []), i]);
    const liveDirty = getLiveDirty ? getLiveDirty() : null;
    const draftTarget = liveDirty && byFile.has(liveDirty.rel) ? liveDirty : null;
    const files = [...byFile.entries()]
      .filter(([p]) => p !== draftTarget?.rel)
      .map(([path, its]) => ({ path, stat: res.stat?.[path], matches: its.map((i) => [i.line, i.col]) }));
    setWorking(true);
    try {
      const undo: Undo = { files: [] };
      let count = 0;
      let nfiles = 0;
      const written: string[] = [];
      const skipped: ReplaceOut['skipped'] = [];
      if (files.length) {
        const r = await rpc<ReplaceOut>(hostId, 'fs.replace', { ...opts(), files });
        for (const f of r.files) {
          count += f.count;
          nfiles++;
          written.push(f.abs);
          if (f.before != null && f.modified != null && f.size != null) undo.files.push({ abs: f.abs, before: f.before, modified: f.modified, size: f.size });
        }
        skipped.push(...r.skipped);
      }
      // The open file with unsaved edits: replace in the draft (every match in it, since the draft may differ from disk)
      if (draftTarget) {
        const r = await rpc<ReplaceOut>(hostId, 'fs.replace', { ...opts(), dryRun: true, files: [{ path: draftTarget.rel, content: draftTarget.draft }] });
        const f = r.files[0];
        if (f?.after != null && f.count) {
          const appliedRev = onDraft(f.after, { path: draftTarget.path, rev: draftTarget.rev, draft: draftTarget.draft });
          if (appliedRev != null) {
            undo.draft = { path: draftTarget.path, rev: appliedRev, before: draftTarget.draft, after: f.after };
            count += f.count;
            nfiles++;
          } else {
            skipped.push({ path: draftTarget.rel, reason: tx('draftChanged') });
          }
        }
      }
      if (written.length) onReplaced(written);
      // Hide what was replaced until the search reruns
      const left = new Set(skipped.map((s) => s.path));
      setDismissed((d) => new Set([...d, ...items.filter((i) => !left.has(i.path)).map(keyOf)]));
      if (count) {
        const canUndo = undo.files.length > 0 || !!undo.draft;
        toast(
          tx('replacedInFiles', { occurrences: tx('occurrenceCount', { count }), files: tx('fileCount', { count: nfiles }) }),
          'info',
          canUndo ? { label: tx('undo'), run: () => void runUndo(undo) } : undefined,
        );
      }
      if (skipped.length) {
        const names = skipped.slice(0, 3).map((s) => `${base(s.path)}（${s.reason}）`).join('、');
        toast(tx('filesNotReplaced', { length: skipped.length, names }), 'error');
      }
      setNonce((n) => n + 1);
    } catch (e) {
      errorToast(e);
    } finally {
      setWorking(false);
    }
  };

  const runUndo = async (u: Undo) => {
    const back: string[] = [];
    let failed = 0;
    for (const f of u.files) {
      try {
        await rpc(hostId, 'fs.write', { path: f.abs, content: f.before, expectModified: f.modified, expectSize: f.size });
        back.push(f.abs);
      } catch {
        failed++;
      }
    }
    if (u.draft) {
      if (onDraft(u.draft.before, { path: u.draft.path, rev: u.draft.rev, draft: u.draft.after }) == null) failed++;
    }
    if (back.length) onReplaced(back);
    toast(
      failed
        ? tx('undoneFilesChangedAfterThe', { failed })
        : tx('replaceUndone'),
      failed ? 'error' : 'info',
    );
    setNonce((n) => n + 1);
  };

  const replaceAll = async () => {
    if (!canReplace) return;
    const ok = await askConfirm({
      title: tx('replaceAll'),
      message:
        tx('replaceConfirm', { occurrences: tx('occurrenceCount', { count: total }), files: tx('fileCount', { count: groups.length }), rep }) +
        (res?.truncated ? tx('theListIsIncompleteOnly') : ''),
      okText: tx('replace'),
    });
    if (ok) replaceIn(flat);
  };

  const open = (it: Item) => {
    setCur(keyOf(it));
    if (!previewing) return onOpen(it.path, it.line, it.col, it.len);
    // Preview: the file with this file's listed matches replaced, as a diff (VS Code's replace preview)
    const its = groups.find(([p]) => p === it.path)?.[1] ?? [it];
    const liveDirty = getLiveDirty?.();
    const draft = liveDirty?.rel === it.path ? liveDirty.draft : dirty?.rel === it.path ? dirty.draft : undefined;
    rpc<ReplaceOut>(hostId, 'fs.replace', {
      ...opts(),
      dryRun: true,
      files: [{ path: it.path, ...(draft != null ? { content: draft } : { matches: its.map((i) => [i.line, i.col]) }) }],
    })
      .then((r) => {
        const f = r.files[0];
        if (f?.after != null) onPreview(it.path, it.line, it.col, it.len, f.after);
        else if (r.skipped[0]) toast(`${base(it.path)}：${r.skipped[0].reason}`, 'error');
      })
      .catch(errorToast);
  };

  // F4 / Shift+F4: next / previous result (VS Code)
  const step = useRef<(d: number) => void>(() => {});
  step.current = (d: number) => {
    if (!flat.length) return;
    const i = flat.findIndex((x) => keyOf(x) === cur);
    const n = flat[(i < 0 ? (d > 0 ? 0 : flat.length - 1) : i + d + flat.length) % flat.length];
    setFolded((f) => (f.has(n.path) ? new Set([...f].filter((p) => p !== n.path)) : f));
    open(n);
    const idx = flatRows.findIndex((r) => r.kind === 'hit' && r.key === n.key);
    if (idx >= 0) requestAnimationFrame(() => scrollToRowIndex(idx));
    else pendingScrollKeyRef.current = n.key;
  };
  useEffect(() => {
    if (!visible) return;
    const on = (e: KeyboardEvent) => {
      if (e.key !== 'F4' || e.ctrlKey || e.metaKey || e.altKey) return;
      e.preventDefault();
      step.current(e.shiftKey ? -1 : 1);
    };
    window.addEventListener('keydown', on);
    return () => window.removeEventListener('keydown', on);
  }, [visible]);

  const dismiss = (it: Item) => setDismissed((s) => new Set(s).add(keyOf(it)));

  return (
    <div className="grep">
      <div className="grep-boxes">
        <button
          className="grep-rep-toggle"
          title={showRep ? tx('hideReplace') : tx('toggleReplace')}
          aria-expanded={showRep}
          onClick={flipRep}
        >
          <span className={`chev ${showRep ? 'open' : ''}`}>
            <Icon name="chevron" size={12} />
          </span>
        </button>
        <div className="grep-inputs">
          <div className="wp-filter grep-input">
            <Icon name="search" size={13} />
            <input
              ref={input}
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder={tx('search')}
              aria-label={tx('searchInFiles')}
              spellCheck={false}
              onBlur={hq.push}
              onKeyDown={(e) => {
                if (e.key === 'Escape') setQ('');
                else if (e.key === 'Enter') hq.push();
                else hq.onKeyDown(e);
              }}
            />
            <button className={`grep-opt ${cs ? 'on' : ''}`} title={tx('matchCase')} onClick={() => toggle('cs')}>
              Aa
            </button>
            <button className={`grep-opt ${ww ? 'on' : ''}`} title={tx('matchWholeWord')} onClick={() => toggle('ww')}>
              <u>ab</u>
            </button>
            <button className={`grep-opt ${re ? 'on' : ''}`} title={tx('useRegularExpression')} onClick={() => toggle('re')}>
              .*
            </button>
          </div>
          {showRep && (
            <div className="wp-filter grep-input grep-rep">
              <input
                value={rep}
                onChange={(e) => setRep(e.target.value)}
                placeholder={tx('replace')}
                aria-label={tx('replaceWith')}
                spellCheck={false}
                onBlur={hr.push}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' && e.altKey && (e.ctrlKey || e.metaKey)) {
                    e.preventDefault();
                    replaceAll();
                  } else hr.onKeyDown(e);
                }}
              />
              <button className={`grep-opt ${pc ? 'on' : ''}`} title={tx('preserveCase')} onClick={() => toggle('pc')}>
                AB
              </button>
              <button className="grep-opt" disabled={!canReplace} title={tx('replaceAllCtrlAltEnter')} onClick={replaceAll}>
                <Icon name="check" size={13} />
              </button>
            </div>
          )}
        </div>
      </div>
      {/* Same column as the search / replace boxes (right of the replace toggle) */}
      <div className="grep-inputs grep-scope">
        <div className="wp-filter grep-input">
          <input
            value={inc}
            onChange={(e) => setInc(e.target.value)}
            placeholder={tx('filesToIncludeEG')}
            aria-label={tx('filesToInclude')}
            spellCheck={false}
            onBlur={hi.push}
            onKeyDown={hi.onKeyDown}
          />
          <button className={`grep-opt ${openOnly ? 'on' : ''}`} title={tx('searchOnlyInOpenFiles')} onClick={() => toggle('open')}>
            <Icon name="file" size={13} />
          </button>
        </div>
        <div className="wp-filter grep-input">
          <input
            value={exc}
            onChange={(e) => setExc(e.target.value)}
            placeholder={tx('filesToExcludeEG')}
            aria-label={tx('filesToExclude')}
            spellCheck={false}
            onBlur={he.push}
            onKeyDown={he.onKeyDown}
          />
        </div>
      </div>
      <div className="grep-sum">
        {busy || working ? (
          <span className="spinner" />
        ) : res && !res.error ? (
          total ? (
            <>
              {tx('resultsInFiles', { length: groups.length, total, v: res.truncated ? '+' : '' })}
              {groups.length > 1 && (
                <button className="link-btn grep-fold" onClick={() => setFolded(folded.size ? new Set() : new Set(groups.map((g) => g[0])))}>
                  {folded.size ? tx('expandAll') : tx('collapseAll')}
                </button>
              )}
            </>
          ) : (
            tx('noResults')
          )
        ) : null}
      </div>
      <div className="wp-tree grep-list" ref={list}>
        {res?.error && (
          <div className="wp-tip">
            <b>{tx('searchFailed')}</b>
            {res.error}
          </div>
        )}
        {!term && <div className="wp-tip">{tx('gitReposSkipGitignoreBinary')}</div>}
        {flatRows.length > 0 && (
          <div
            ref={rowsBoxRef}
            style={
              virtual
                ? {
                    paddingTop: offsets[Math.min(win[0], flatRows.length)],
                    paddingBottom: offsets[flatRows.length] - offsets[Math.min(Math.max(win[1], win[0]), flatRows.length)],
                  }
                : undefined
            }
          >
            {flatRows
              .slice(
                virtual ? Math.min(win[0], flatRows.length) : 0,
                virtual ? Math.min(Math.max(win[1], win[0]), flatRows.length) : flatRows.length,
              )
              .map((row) =>
                row.kind === 'head' ? (
                  <div
                    key={row.key}
                    className="wp-row grep-head"
                    title={row.path}
                    onClick={() =>
                      setFolded((s) => {
                        const n = new Set(s);
                        if (n.has(row.path)) n.delete(row.path);
                        else n.add(row.path);
                        return n;
                      })
                    }
                  >
                    <span className="wp-twist">
                      <span className={`chev ${row.closed ? '' : 'open'}`}>
                        <Icon name="chevron" size={12} />
                      </span>
                    </span>
                    <FileIcon name={row.name} />
                    <span className="truncate grep-name">{row.name}</span>
                    <span className="truncate grep-dir">{row.dir}</span>
                    <span className="grep-acts">
                      {previewing && (
                        <button
                          className="icon-btn"
                          title={tx('replaceAllInFile')}
                          disabled={working}
                          onClick={(e) => (e.stopPropagation(), replaceIn(row.items))}
                        >
                          <Icon name="check" size={12} />
                        </button>
                      )}
                      <button className="icon-btn" title={tx('dismiss')} onClick={(e) => (e.stopPropagation(), setGone((g) => new Set(g).add(row.path)))}>
                        <Icon name="x" size={12} />
                      </button>
                    </span>
                    <span className="grep-n">{row.items.length}</span>
                  </div>
                ) : (
                  <div
                    key={row.key}
                    data-k={row.key}
                    className={`wp-row grep-hit ${cur === row.key ? 'sel' : ''}`}
                    title={`${row.path}:${row.item.hit.line}`}
                    onClick={() => open(row.item)}
                  >
                    <span className="grep-line">{row.item.hit.line}</span>
                    <span className="truncate grep-text">
                      {row.item.pre}
                      {previewing ? (
                        <>
                          <del>{row.item.mid}</del>
                          <ins>{row.item.rep ?? ''}</ins>
                        </>
                      ) : (
                        <mark>{row.item.mid}</mark>
                      )}
                      {row.item.post}
                    </span>
                    <span className="grep-acts">
                      {previewing && (
                        <button className="icon-btn" title={tx('replace')} disabled={working} onClick={(e) => (e.stopPropagation(), replaceIn([row.item]))}>
                          <Icon name="check" size={12} />
                        </button>
                      )}
                      <button className="icon-btn" title={tx('dismiss')} onClick={(e) => (e.stopPropagation(), dismiss(row.item))}>
                        <Icon name="x" size={12} />
                      </button>
                    </span>
                  </div>
                ),
              )}
          </div>
        )}
        {res?.truncated && (
          <div className="wp-limit">{tx('tooManyResultsShowingThe', { total })}</div>
        )}
      </div>
    </div>
  );
}
