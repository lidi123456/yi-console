import { forwardRef, useEffect, useImperativeHandle, useRef } from 'react';
import { EditorState, type Extension } from '@codemirror/state';
import { drawSelection, EditorView, highlightSpecialChars, keymap, lineNumbers } from '@codemirror/view';
import { defaultKeymap } from '@codemirror/commands';
import { LanguageDescription, syntaxHighlighting } from '@codemirror/language';
import { languages } from '@codemirror/language-data';
import { highlightSelectionMatches, searchKeymap } from '@codemirror/search';
import { getChunks, goToNextChunk, goToPreviousChunk, MergeView, unifiedMergeView } from '@codemirror/merge';
import { getT, isEn, useLang } from '../i18n';
import { highlight, searchPhrases, theme } from './CodeEditor';

const t = getT('diffView');

// Full-file diff in the file panel, modelled on VS Code's diff editor: both whole files with syntax highlighting,
// changed lines plus character-level marks inside them, and unchanged stretches collapsed.
// - Side by side is two CodeMirror editors (MergeView) aligned line by line; stacked is one editor with the deleted
//   lines shown above the new ones (unifiedMergeView), like VS Code's inline view.
// - Colors are VS Code's diffEditor.* defaults (variables in styles.css).
// - Performance: CodeMirror only renders the visible lines of either side; the diff runs on the main thread, so it
//   gets a 1 s budget (VS Code runs it in a worker with 5 s) and falls back to a coarser diff past that.
//   The caller limits file size (git.blob and fs.read stop at 1 MB) and uses the hunk-only PaneDiff beyond it.
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

  const same = a.replace(/\r\n/g, '\n') === b.replace(/\r\n/g, '\n');

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
      destroy();
    };
  }, [a, b, filename, layout, collapse, wrap, lang, same]);

  const build = (el: HTMLElement, language: Extension): (() => void) => {
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
      destroy = () => mv.destroy();
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

  if (same) return <div className="pd-empty muted small">{t('bothSidesAreTheSame')}</div>;
  return <div className={`wp-merge wp-merge-${layout}`} ref={host} />;
});
