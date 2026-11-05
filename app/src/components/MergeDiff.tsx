import { forwardRef, useEffect, useImperativeHandle, useRef, useState } from 'react';
import { ChangeSet, EditorState, type Extension } from '@codemirror/state';
import { drawSelection, EditorView, highlightSpecialChars, keymap, lineNumbers } from '@codemirror/view';
import { defaultKeymap } from '@codemirror/commands';
import { LanguageDescription, syntaxHighlighting } from '@codemirror/language';
import { languages } from '@codemirror/language-data';
import { highlightSelectionMatches, searchKeymap } from '@codemirror/search';
import { getChunks, getOriginalDoc, goToNextChunk, goToPreviousChunk, MergeView, originalDocChangeEffect, unifiedMergeView } from '@codemirror/merge';
import { getT, isEn, useLang } from '../i18n';
import { highlight, searchPhrases, theme } from './CodeEditor';

const t = getT('diffView');

// Full-file diff in the file panel, modelled on VS Code's diff editor: both whole files with syntax highlighting,
// changed lines plus character-level marks inside them, and unchanged stretches collapsed.
// - Side by side is two CodeMirror editors (MergeView) aligned line by line, each scrolling itself with the scroll
//   positions synced (syncScroll), as in VS Code; stacked is one editor with the deleted
//   lines shown above the new ones (unifiedMergeView), like VS Code's inline view.
// - Colors are VS Code's diffEditor.* defaults (variables in styles.css).
// - Performance: CodeMirror only renders the visible lines of either side; the diff runs on the main thread, so it
//   gets a 1 s budget (VS Code runs it in a worker with 5 s) and falls back to a coarser diff past that.
//   The caller limits file size (git.blob and fs.read stop at 1 MB) and uses the hunk-only PaneDiff beyond it.
// - Added and deleted files (one side empty) show just the one file, as VS Code's git extension opens untracked / added
//   files directly and deleted ones as their old content (oneSided).
// - When the text changes while open (the agent edits the file, the status refresh brings a new HEAD), the editors are
//   patched in place with the changed span instead of rebuilt, so the scroll position stays put; only opening the diff
//   jumps to the first change, as in VS Code.
// - Side by side, the divider drags to resize the two panes (VS Code's enableSplitViewResizing); double-click resets to
//   half and half. Like VS Code the ratio isn't saved.
// Differences from VS Code: collapsed regions expand all at once (VS Code reveals 20 lines at a time), whitespace
// changes at line ends are not ignored, and the stacked view only numbers the new file's lines.

const DIFF_CONFIG = { scanLimit: 500, timeout: 1000 };
/** VS Code: hideUnchangedRegions contextLineCount 3; CodeMirror's minSize is the smallest region worth collapsing */
const COLLAPSE = { margin: 3, minSize: 4 };

const mergeTheme = EditorView.theme({
  '&.cm-merge-a .cm-changedLine, .cm-deletedChunk': { backgroundColor: 'var(--df-del-line)' },
  '&.cm-merge-b .cm-changedLine, .cm-inlineChangedLine': { backgroundColor: 'var(--df-add-line)' },
  '&.cm-merge-a .cm-changedText, .cm-deletedChunk .cm-deletedText': { background: 'var(--df-del-text)' },
  '&.cm-merge-b .cm-changedText': { background: 'var(--df-add-text)' },
  '&.cm-merge-b .cm-deletedText': { background: 'var(--df-del-text)' },
  // Lines with no counterpart on the other side: VS Code's diagonal fill
  '.cm-mergeSpacer': {
    backgroundImage: 'linear-gradient(-45deg, var(--df-diag) 12.5%, transparent 12.5%, transparent 50%, var(--df-diag) 50%, var(--df-diag) 62.5%, transparent 62.5%, transparent 100%)',
    backgroundSize: '8px 8px',
  },
  '.cm-collapsedLines': {
    background: 'var(--bg-2)',
    color: 'var(--text-2)',
    fontFamily: 'var(--font)',
    fontSize: '12px',
    padding: '3px 10px',
    borderTop: '1px solid var(--border)',
    borderBottom: '1px solid var(--border)',
  },
  '.cm-collapsedLines:hover': { color: 'var(--text)', background: 'var(--bg-3)' },
  '.cm-changeGutter': { width: '3px', paddingLeft: '0' },
  '&.cm-merge-a .cm-changedLineGutter, .cm-deletedLineGutter': { background: 'var(--df-del-mark)' },
  '&.cm-merge-b .cm-changedLineGutter': { background: 'var(--df-add-mark)' },
});

function phrases() {
  const p = isEn() ? { '$ unchanged lines': '$ unchanged lines' } : { '$ unchanged lines': '$ 行未改动' };
  return [searchPhrases(), EditorState.phrases.of(p)];
}

/** Side by side, each editor scrolls itself (so its horizontal scrollbar sits at the bottom of the pane) and the two
 *  follow each other both ways, like VS Code's diff editor (diffEditorViewZones.ts syncs scrollTop and scrollLeft).
 *  MergeView's spacers give both sides the same line positions, so equal offsets keep lines aligned. */
function syncScroll(a: EditorView, b: EditorView): () => void {
  let applying: HTMLElement | null = null;
  const follow = (from: HTMLElement, to: HTMLElement) => () => {
    // Ignore the scroll event caused by our own assignment
    if (applying === from) {
      applying = null;
      return;
    }
    const top = to.scrollTop;
    const left = to.scrollLeft;
    to.scrollTop = from.scrollTop;
    to.scrollLeft = from.scrollLeft;
    // Only an actual change fires a scroll event (a narrower side clamps scrollLeft)
    applying = to.scrollTop !== top || to.scrollLeft !== left ? to : null;
  };
  const fa = follow(a.scrollDOM, b.scrollDOM);
  const fb = follow(b.scrollDOM, a.scrollDOM);
  a.scrollDOM.addEventListener('scroll', fa, { passive: true });
  b.scrollDOM.addEventListener('scroll', fb, { passive: true });
  return () => {
    a.scrollDOM.removeEventListener('scroll', fa);
    b.scrollDOM.removeEventListener('scroll', fb);
  };
}

const norm = (s: string) => s.replace(/\r\n/g, '\n');

/** The change turning `old` into `text`: one replacement of the span between their common prefix and suffix */
function spanChange(old: string, text: string): { from: number; to: number; insert: string } | null {
  if (old === text) return null;
  const max = Math.min(old.length, text.length);
  let start = 0;
  while (start < max && old.charCodeAt(start) === text.charCodeAt(start)) start++;
  let end = 0;
  while (end < max - start && old.charCodeAt(old.length - 1 - end) === text.charCodeAt(text.length - 1 - end)) end++;
  return { from: start, to: old.length - end, insert: text.slice(start, text.length - end) };
}

/** Replace a view's document with `text`, touching only what changed (programmatic changes pass readOnly) */
function patchDoc(view: EditorView, text: string) {
  const c = spanChange(view.state.doc.toString(), norm(text));
  if (c) view.dispatch({ changes: c });
}

/** Same for the original document a unified merge view compares against */
function patchOriginal(view: EditorView, text: string) {
  const old = getOriginalDoc(view.state);
  const c = spanChange(old.toString(), norm(text));
  if (c) view.dispatch({ effects: originalDocChangeEffect(view.state, ChangeSet.of(c, old.length)) });
}

/** The editors of a built diff, for patching them when the text changes */
type Live = { kind: 'split'; a: EditorView; b: EditorView } | { kind: 'unified' | 'single'; view: EditorView };
/** One side empty: an added or deleted file, shown as the single file instead of a diff */
export const oneSided = (a: string, b: string) => (norm(a) === '') !== (norm(b) === '');
/** Narrowest a pane may be dragged, in px */
const MIN_PANE = 120;

export interface MergeDiffHandle {
  next(): void;
  prev(): void;
}

export const MergeDiff = forwardRef<
  MergeDiffHandle,
  {
    /** Old text (left / deleted side) */
    a: string;
    /** New text (right / current side) */
    b: string;
    /** Picks the syntax highlighting */
    filename: string;
    layout: 'split' | 'unified';
    collapse: boolean;
    wrap: boolean;
  }
>(function MergeDiff({ a, b, filename, layout, collapse, wrap }, ref) {
  const host = useRef<HTMLDivElement>(null);
  const nav = useRef<EditorView | null>(null);
  const [, lang] = useLang();

  useImperativeHandle(ref, () => ({
    next: () => nav.current && goToNextChunk(nav.current),
    prev: () => nav.current && goToPreviousChunk(nav.current),
  }));

  const same = norm(a) === norm(b);
  const single = oneSided(a, b);
  const split = layout === 'split' && !single;
  // Share of the width for the left pane
  const [ratio, setRatio] = useState(0.5);
  // Latest texts for a build that finishes after they changed (the language loads asynchronously)
  const text = useRef({ a, b });
  text.current = { a, b };
  const live = useRef<Live | null>(null);
  // Which file a one-sided diff shows: switching sides needs a rebuild, edits to it don't
  const side = single ? (norm(a) === '' ? 'b' : 'a') : '';

  useEffect(() => {
    const el = host.current;
    if (same || !el) return;
    let alive = true;
    let destroy = () => {};
    // Load the language first: the stacked view renders deleted lines once, so they'd stay unhighlighted if it came later
    const desc = LanguageDescription.matchFilename(languages, filename);
    (desc ? desc.load().catch((e) => (console.warn('加载语法高亮失败', filename, e), null)) : Promise.resolve(null)).then((l) => {
      if (alive) destroy = build(el, l ?? []);
    });
    return () => {
      alive = false;
      nav.current = null;
      live.current = null;
      destroy();
    };
  }, [filename, layout, collapse, wrap, lang, same, side]);

  // Text changed with the same layout: patch the open editors so the scroll position and expanded regions stay
  useEffect(() => {
    const l = live.current;
    if (!l) return;
    if (l.kind === 'split') {
      patchDoc(l.a, a);
      patchDoc(l.b, b);
    } else if (l.kind === 'unified') {
      patchOriginal(l.view, a);
      patchDoc(l.view, b);
    } else patchDoc(l.view, side === 'b' ? b : a);
  }, [a, b]); // eslint-disable-line react-hooks/exhaustive-deps

  const build = (el: HTMLElement, language: Extension): (() => void) => {
    const { a, b } = text.current;
    const ext = (): Extension[] => [
      lineNumbers(),
      highlightSpecialChars(),
      drawSelection(),
      highlightSelectionMatches(),
      syntaxHighlighting(highlight),
      // Alt+F5 / Shift+Alt+F5: next / previous change, as in VS Code
      keymap.of([{ key: 'Alt-F5', run: goToNextChunk }, { key: 'Shift-Alt-F5', run: goToPreviousChunk }, ...defaultKeymap, ...searchKeymap]),
      EditorState.readOnly.of(true),
      theme,
      mergeTheme,
      phrases(),
      wrap ? EditorView.lineWrapping : [],
      language,
    ];
    const collapseUnchanged = collapse ? COLLAPSE : undefined;
    if (single) {
      const v = new EditorView({ parent: el, state: EditorState.create({ doc: norm(a) === '' ? b : a, extensions: ext() }) });
      live.current = { kind: 'single', view: v };
      return () => v.destroy();
    }
    let views: EditorView[];
    let destroy: () => void;
    if (layout === 'split') {
      const mv = new MergeView({
        a: { doc: a, extensions: ext() },
        b: { doc: b, extensions: ext() },
        parent: el,
        gutter: true,
        highlightChanges: true,
        collapseUnchanged,
        diffConfig: DIFF_CONFIG,
      });
      views = [mv.a, mv.b];
      live.current = { kind: 'split', a: mv.a, b: mv.b };
      const unsync = syncScroll(mv.a, mv.b);
      destroy = () => (unsync(), mv.destroy());
    } else {
      const v = new EditorView({
        parent: el,
        state: EditorState.create({
          doc: b,
          extensions: [
            ...ext(),
            unifiedMergeView({ original: a, mergeControls: false, gutter: true, highlightChanges: true, syntaxHighlightDeletions: true, collapseUnchanged, diffConfig: DIFF_CONFIG }),
          ],
        }),
      });
      views = [v];
      live.current = { kind: 'unified', view: v };
      destroy = () => v.destroy();
    }
    const target = views[views.length - 1];
    nav.current = target;
    // Open at the first change, like VS Code
    const raf = requestAnimationFrame(() => {
      const first = getChunks(target.state)?.chunks[0];
      if (first) target.dispatch({ effects: EditorView.scrollIntoView(first.fromB, { y: 'start', yMargin: 40 }) });
    });
    return () => {
      cancelAnimationFrame(raf);
      destroy();
    };
  };

  const box = useRef<HTMLDivElement>(null);
  const drag = (e: React.PointerEvent<HTMLDivElement>) => {
    const r = box.current?.getBoundingClientRect();
    if (!r || e.button !== 0) return;
    e.preventDefault();
    const sash = e.currentTarget;
    sash.setPointerCapture(e.pointerId);
    document.body.classList.add('resizing');
    const lo = Math.min(0.5, MIN_PANE / r.width);
    const move = (ev: PointerEvent) => setRatio(Math.min(1 - lo, Math.max(lo, (ev.clientX - r.left) / r.width)));
    const up = () => {
      sash.removeEventListener('pointermove', move);
      sash.removeEventListener('pointerup', up);
      sash.removeEventListener('pointercancel', up);
      document.body.classList.remove('resizing');
    };
    sash.addEventListener('pointermove', move);
    sash.addEventListener('pointerup', up);
    sash.addEventListener('pointercancel', up);
  };

  if (same) return <div className="pd-empty muted small">{t('bothSidesAreTheSame')}</div>;
  return (
    <div
      ref={box}
      className={`wp-merge wp-merge-${split ? 'split' : 'unified'}`}
      style={split ? ({ '--split': ratio } as React.CSSProperties) : undefined}
    >
      <div className="wp-merge-host" ref={host} />
      {split && <div className="wp-merge-sash" title={t('dragToResizeDoubleClick')} onPointerDown={drag} onDoubleClick={() => setRatio(0.5)} />}
    </div>
  );
});
