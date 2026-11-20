import { useEffect, useRef, useState } from 'react';
import { getT, isEn, useLang } from '../i18n';
import { Compartment, EditorState } from '@codemirror/state';
import {
  crosshairCursor,
  drawSelection,
  EditorView,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  keymap,
  lineNumbers,
  rectangularSelection,
} from '@codemirror/view';
import { closeBrackets, closeBracketsKeymap } from '@codemirror/autocomplete';
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { bracketMatching, foldAll, foldGutter, foldKeymap, HighlightStyle, indentOnInput, indentUnit, LanguageDescription, syntaxHighlighting, unfoldAll } from '@codemirror/language';
import { highlightSelectionMatches, openSearchPanel, searchPanelOpen } from '@codemirror/search';
import { languages } from '@codemirror/language-data';
import { tags as t } from '@lezer/highlight';
import { detectEol, detectIndent } from '../textFormat';
import { openMenu, type MenuItem } from './Overlay';
import { readClipboard, writeClipboard } from '../clipboard';
import { editorSearchExtension, editorSearchKeymap } from './EditorSearchPanel';
import { Icon } from './Icon';

const tx = getT('filePanel');

/** What the right-click menu of the editor can act on (built by the caller, see FilePanel's editorMenu) */
export interface EditorMenuCtx {
  /** Selected text (all ranges, joined by line breaks); empty when nothing is selected */
  selection: string;
  /** Full lines covered by the main selection (preserving leading indentation on line 1) */
  selectedLines: string;
  readOnly: boolean;
  /** Lines the main selection covers (1-based); equal when it's a cursor or within one line */
  fromLine: number;
  toLine: number;
  cut: () => void;
  copy: () => void;
  paste: () => void;
  selectAll: () => void;
  find: () => void;
  foldAll: () => void;
  unfoldAll: () => void;
  /** Set by the diff view: adds the selection as a diff snippet (deleted lines included), like its floating button */
  addSelection?: () => void;
  /** Set by the diff view for the old (left) side: line numbers are the old file's, not the file on disk */
  oldSide?: boolean;
}

export interface GotoTarget {
  line: number;
  endLine?: number;
  col?: number;
  len?: number;
  n: number;
}

/** Extract the 1-based line span and full-line text of the main non-empty selection */
export function getSelectionSpan(st: EditorState): { fromLine: number; toLine: number; content: string } | null {
  const main = st.selection.main;
  if (main.empty) return null;
  const fromLine = st.doc.lineAt(main.from).number;
  const end = st.doc.lineAt(main.to);
  const toLine = Math.max(fromLine, main.to > main.from && main.to === end.from ? end.number - 1 : end.number);
  const content = st.doc.sliceString(st.doc.line(fromLine).from, st.doc.line(toLine).to);
  return content ? { fromLine, toLine, content } : null;
}

/** Position a floating action chip above (or below near line 1) the main selection */
export function measureSelectionChip(v: EditorView, container: HTMLElement): { top: number; left: number } | null {
  const main = v.state.selection.main;
  if (main.empty) return null;
  const endPos = main.to > main.from && v.state.doc.lineAt(main.to).from === main.to ? main.to - 1 : main.to;
  const cFrom = v.coordsAtPos(main.from);
  const cTo = v.coordsAtPos(Math.max(main.from, endPos));
  if (!cFrom && !cTo) return null;
  const r = container.getBoundingClientRect();
  const sr = v.scrollDOM.getBoundingClientRect();
  const rawTop = cFrom?.top ?? cTo!.top;
  const rawBot = cTo?.bottom ?? cFrom!.bottom;
  if (rawBot < sr.top + 8 || rawTop > sr.bottom - 8) return null;
  const topRel = rawTop - r.top;
  const botRel = rawBot - r.top;
  const top = topRel >= 34 ? topRel - 30 : Math.min(Math.max(4, r.height - 32), botRel + 4);
  const minLeft = Math.max(8, sr.left - r.left + 44);
  const maxLeft = Math.max(minLeft, r.width - 180);
  const left = Math.min(maxLeft, Math.max(minLeft, (cFrom?.left ?? sr.left + 52) - r.left));
  return { top, left };
}

/** Build the right-click context menu state and dispatchers for a CodeMirror view */
export function openEditorContextMenu(e: React.MouseEvent, v: EditorView, menu: (ctx: EditorMenuCtx) => (MenuItem | '-')[]) {
  // Like VS Code: right-clicking outside the selection moves the cursor there first, inside it keeps the selection
  const pos = v.posAtCoords({ x: e.clientX, y: e.clientY });
  if (pos != null && !v.state.selection.ranges.some((r) => r.from <= pos && pos <= r.to)) v.dispatch({ selection: { anchor: pos } });
  const st = v.state;
  const main = st.selection.main;
  const selection = st.selection.ranges.filter((r) => !r.empty).map((r) => st.sliceDoc(r.from, r.to)).join(st.lineBreak);
  const fromLine = st.doc.lineAt(main.from).number;
  // A selection ending at the start of a line doesn't include that line (VS Code does the same)
  const end = st.doc.lineAt(main.to);
  const toLine = Math.max(fromLine, main.to > main.from && main.to === end.from ? end.number - 1 : end.number);
  const selectedLines = selection ? st.doc.sliceString(st.doc.line(fromLine).from, st.doc.line(toLine).to) : '';
  const ro = st.readOnly;
  const ctx: EditorMenuCtx = {
    selection,
    selectedLines,
    readOnly: ro,
    fromLine,
    toLine,
    copy: () => void writeClipboard(selection),
    cut: () => {
      writeClipboard(selection).then((ok) => ok && v.dispatch(v.state.replaceSelection(''), { userEvent: 'delete.cut' }));
      v.focus();
    },
    paste: () => {
      readClipboard(false).then((text) => {
        if (text) v.dispatch(v.state.replaceSelection(text), { userEvent: 'input.paste', scrollIntoView: true });
        v.focus();
      });
    },
    selectAll: () => {
      v.dispatch({ selection: { anchor: 0, head: v.state.doc.length } });
      v.focus();
    },
    find: () => openSearchPanel(v),
    foldAll: () => foldAll(v),
    unfoldAll: () => unfoldAll(v),
  };
  openMenu(e, menu(ctx));
}

export function applyGoto(v: EditorView, goto: GotoTarget) {
  const doc = v.state.doc;
  if (!doc.lines) return;
  const ln = doc.line(Math.max(1, Math.min(doc.lines, goto.line)));
  let from: number;
  let to: number;
  if (goto.endLine != null && goto.col == null) {
    from = ln.from;
    to = doc.line(Math.max(ln.number, Math.min(doc.lines, goto.endLine))).to;
  } else {
    from = Math.min(ln.to, ln.from + (goto.col ?? 0));
    to = Math.min(ln.to, from + (goto.len ?? 0));
  }
  v.dispatch({
    selection: { anchor: from, head: to },
    effects: EditorView.scrollIntoView(from, { y: 'center' }),
  });
  v.focus();
}

// Code editor in the file panel (CodeMirror 6): syntax highlighting, line numbers, current line, find/replace, Ctrl+S to save,
// folding, auto-closed brackets and multiple cursors (VS Code's bindings: Alt+click, Shift+Alt+drag for a column, Ctrl+Alt+Up/Down, Ctrl+D).
// Language packs are lazy-loaded by file name and stay out of the main bundle

/** Colors use CSS variables; dark mode swaps them in styles.css */
export const highlight = HighlightStyle.define([
  { tag: [t.keyword, t.controlKeyword, t.moduleKeyword, t.operatorKeyword, t.modifier], color: 'var(--hl-keyword)' },
  { tag: [t.string, t.special(t.string), t.regexp, t.character], color: 'var(--hl-string)' },
  { tag: [t.number, t.bool, t.null, t.atom, t.unit], color: 'var(--hl-number)' },
  { tag: [t.comment, t.lineComment, t.blockComment, t.docComment], color: 'var(--hl-comment)', fontStyle: 'italic' },
  { tag: [t.function(t.variableName), t.function(t.propertyName), t.macroName], color: 'var(--hl-func)' },
  { tag: [t.typeName, t.className, t.namespace, t.tagName, t.standard(t.tagName)], color: 'var(--hl-type)' },
  { tag: [t.propertyName, t.attributeName, t.labelName], color: 'var(--hl-prop)' },
  { tag: [t.special(t.variableName), t.self, t.definitionKeyword], color: 'var(--hl-keyword)' },
  { tag: [t.variableName, t.definition(t.variableName)], color: 'var(--hl-var)' },
  { tag: [t.heading], color: 'var(--hl-type)', fontWeight: '600' },
  { tag: [t.link, t.url], color: 'var(--hl-string)', textDecoration: 'underline' },
  { tag: [t.emphasis], fontStyle: 'italic' },
  { tag: [t.strong], fontWeight: '600' },
  { tag: [t.meta, t.processingInstruction], color: 'var(--hl-comment)' },
  { tag: [t.invalid], color: 'var(--err)' },
]);

/** Fold gutter arrow: a chevron pointing down when open, right when folded */
export function foldMarker(open: boolean) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 16 16');
  svg.setAttribute('class', open ? 'cm-fold-mk' : 'cm-fold-mk closed');
  svg.innerHTML = '<path d="M4 6l4 4 4-4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>';
  return svg as unknown as HTMLElement;
}

export const theme = EditorView.theme({
  // Editor font size and line height are per-device preferences (fontPrefs.ts)
  '&': { height: '100%', fontSize: 'var(--fs-editor)', backgroundColor: 'var(--bg)', color: 'var(--text)', overflow: 'hidden' },
  '&.cm-focused': { outline: 'none' },
  '.cm-scroller': { fontFamily: 'var(--font-editor)', lineHeight: 'var(--lh-editor)', flex: '1 1 0%', minHeight: 0, height: 'auto' },
  '.cm-content': { padding: '4px 0', caretColor: 'var(--text)' },
  '.cm-gutters': { backgroundColor: 'var(--bg)', color: 'var(--text-3)', border: 'none' },
  '.cm-lineNumbers .cm-gutterElement': { padding: '0 4px 0 16px', minWidth: '36px' },
  // Fold arrows: hidden until the gutter is hovered (as in VS Code); folded ranges always show theirs
  '.cm-foldGutter .cm-gutterElement': { width: '16px', display: 'flex', alignItems: 'center', justifyContent: 'center', cursor: 'pointer' },
  '.cm-fold-mk': { width: '10px', height: '10px', color: 'var(--text-3)', opacity: 0, transition: 'opacity .15s' },
  '.cm-fold-mk.closed, .cm-gutters:hover .cm-fold-mk': { opacity: 1 },
  '.cm-fold-mk.closed': { transform: 'rotate(-90deg)' },
  '.cm-foldPlaceholder': { backgroundColor: 'var(--cm-match)', border: 'none', color: 'var(--text-2)', padding: '0 4px', margin: '0 2px' },
  '.cm-activeLine': { backgroundColor: 'var(--cm-active)' },
  '.cm-activeLineGutter': { backgroundColor: 'transparent', color: 'var(--text)' },
  '&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection': { backgroundColor: 'var(--cm-sel) !important' },
  '.cm-selectionMatch': { backgroundColor: 'var(--cm-match)' },
  '&.cm-focused .cm-matchingBracket': { backgroundColor: 'var(--cm-match)', outline: '1px solid var(--border-2)' },
  '.cm-searchMatch': { backgroundColor: 'var(--cm-find)', borderRadius: '2px' },
  '.cm-searchMatch.cm-searchMatch-selected': { backgroundColor: 'var(--cm-find-cur)', boxShadow: '0 0 0 1px var(--cm-find-ring)' },
  // Keep CodeMirror panels/tooltips inside the editor's stacking context (baseTheme sets z-index 300/500, which otherwise escapes above .modal-bg at 50)
  '.cm-panels': { backgroundColor: 'var(--bg-2)', color: 'var(--text)', borderColor: 'var(--border)', zIndex: 10, flexShrink: 0 },
  '.cm-panels.cm-panels-top': { borderBottom: '1px solid var(--border)' },
  '.cm-panels.cm-panels-bottom': { borderTop: '1px solid var(--border)' },
  '.cm-tooltip': { zIndex: 20 },
  // Compact inputs/buttons for CodeMirror built-in dialogs such as Go to line (Mod-Alt-g)
  '.cm-textfield': {
    width: 'min(220px, 100%)',
    height: '26px',
    margin: '2px 6px 2px 0',
    padding: '0 8px',
    borderRadius: '6px',
    border: '1px solid var(--border-2)',
    verticalAlign: 'middle',
  },
  '.cm-button': {
    height: '26px',
    margin: '2px 4px 2px 0',
    padding: '0 10px',
    backgroundImage: 'none',
    backgroundColor: 'var(--bg)',
    border: '1px solid var(--border-2)',
    borderRadius: '6px',
    verticalAlign: 'middle',
    lineHeight: '24px',
  },
  '.cm-button:hover': { backgroundColor: 'var(--bg-3)' },
});

export function searchPhrases() {
  if (isEn()) return [];
  return EditorState.phrases.of({
    Find: '查找',
    Replace: '替换',
    next: '下一个',
    previous: '上一个',
    all: '全部',
    'match case': '区分大小写',
    regexp: '正则',
    'by word': '全词',
    replace: '替换',
    'replace all': '全部替换',
    close: '关闭',
    'Go to line': '跳转到行',
    go: '跳转',
    'current match': '当前匹配',
    'on line': '行',
  });
}

export function CodeEditor({
  docKey,
  value,
  filename,
  readOnly,
  wrap,
  onChange,
  onSave,
  goto,
  onTopLine,
  scrollLine,
  menu,
  onToggleWrap,
  onAddSelection,
}: {
  /** Changes when switching files (or re-reading from disk); the editor is rebuilt from scratch */
  docKey: string;
  value: string;
  filename: string;
  readOnly: boolean;
  wrap: boolean;
  onChange: (v: string) => void;
  onSave: () => void;
  /** Jump to a line or line range; jumps again whenever n changes */
  goto?: GotoTarget | null;
  /** Reports the top line while scrolling (frac is how much of that line is visible, 0–1); the Markdown split preview follows it */
  onTopLine?: (line: number, frac: number) => void;
  /** Scroll a line to the top (without changing the selection); scrolls again whenever n changes */
  scrollLine?: { line: number; frac: number; n: number } | null;
  /** Right-click menu items; without it the global edit menu is used */
  menu?: (ctx: EditorMenuCtx) => (MenuItem | '-')[];
  /** Alt+Z, VS Code's toggle for word wrap */
  onToggleWrap?: () => void;
  /** Add the selected code lines as context in the session composer (Ctrl/Cmd+L or floating button) */
  onAddSelection?: (sel: { fromLine: number; toLine: number; content: string }) => void;
}) {
  const host = useRef<HTMLDivElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const [, lang] = useLang();
  const view = useRef<EditorView | null>(null);
  const dragging = useRef(false);
  const [selChip, setSelChip] = useState<{ top: number; left: number } | null>(null);
  const comp = useRef({ lang: new Compartment(), ro: new Compartment(), wrap: new Compartment(), phrases: new Compartment() });
  // Callbacks go through a ref so the editor doesn't need rebuilding
  const cb = useRef({ onChange, onSave, onTopLine, onToggleWrap, onAddSelection });
  cb.current = { onChange, onSave, onTopLine, onToggleWrap, onAddSelection };

  const syncChip = () => {
    const v = view.current;
    const box = wrapRef.current;
    if (!v || !box || dragging.current || !cb.current.onAddSelection) {
      setSelChip(null);
      return;
    }
    // Keep the floating "+ Add to chat" chip out of the way while stepping through search matches
    if (searchPanelOpen(v.state) && document.activeElement?.closest('.ed-find')) {
      setSelChip(null);
      return;
    }
    setSelChip(measureSelectionChip(v, box));
  };

  useEffect(() => {
    const c = comp.current;
    const eol = detectEol(value);
    const ind = detectIndent(value);
    setSelChip(null);
    const v = new EditorView({
      parent: host.current!,
      state: EditorState.create({
        doc: value,
        extensions: [
          lineNumbers(),
          foldGutter({ markerDOM: foldMarker }),
          highlightActiveLineGutter(),
          highlightSpecialChars(),
          history(),
          drawSelection(),
          EditorState.allowMultipleSelections.of(true),
          // VS Code: Alt+click adds a cursor, Shift+Alt+drag selects a column
          EditorView.clickAddsSelectionRange.of((e) => e.altKey),
          rectangularSelection({ eventFilter: (e) => e.altKey && e.shiftKey }),
          crosshairCursor({ key: 'Shift' }),
          indentOnInput(),
          bracketMatching(),
          closeBrackets(),
          highlightActiveLine(),
          highlightSelectionMatches(),
          syntaxHighlighting(highlight),
          keymap.of([
            { key: 'Mod-s', preventDefault: true, run: () => (cb.current.onSave(), true) },
            { key: 'Alt-z', preventDefault: true, run: () => (cb.current.onToggleWrap?.(), !!cb.current.onToggleWrap) },
            {
              key: 'Mod-l',
              preventDefault: true,
              run: (ev) => {
                if (!cb.current.onAddSelection) return false;
                const s = getSelectionSpan(ev.state);
                if (!s) return false;
                cb.current.onAddSelection(s);
                return true;
              },
            },
            ...closeBracketsKeymap,
            ...defaultKeymap,
            ...historyKeymap,
            ...editorSearchKeymap,
            ...foldKeymap,
            indentWithTab,
          ]),
          editorSearchExtension(),
          EditorState.tabSize.of(ind.tabSize),
          indentUnit.of(ind.unit),
          c.phrases.of(searchPhrases()),
          theme,
          c.lang.of([]),
          c.ro.of([EditorState.readOnly.of(readOnly), EditorView.editable.of(!readOnly)]),
          c.wrap.of(wrap ? EditorView.lineWrapping : []),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) cb.current.onChange(u.state.doc.sliceString(0, u.state.doc.length, eol));
            if (u.transactions.some((tr) => tr.isUserEvent('select.search'))) {
              setSelChip(null);
              return;
            }
            if (u.selectionSet || u.docChanged || u.geometryChanged) requestAnimationFrame(syncChip);
          }),
        ],
      }),
    });
    view.current = v;
    let alive = true;
    const onScroll = () => {
      syncChip();
      const f = cb.current.onTopLine;
      if (!f) return;
      const blk = v.lineBlockAtHeight(v.scrollDOM.scrollTop);
      f(v.state.doc.lineAt(blk.from).number, blk.height ? Math.max(0, Math.min(1, (v.scrollDOM.scrollTop - blk.top) / blk.height)) : 0);
    };
    const onUp = () => {
      if (!dragging.current) return;
      dragging.current = false;
      requestAnimationFrame(syncChip);
    };
    v.scrollDOM.addEventListener('scroll', onScroll, { passive: true });
    window.addEventListener('pointerup', onUp);
    window.addEventListener('pointercancel', onUp);
    const desc = LanguageDescription.matchFilename(languages, filename);
    desc
      ?.load()
      .then((lang) => alive && v.dispatch({ effects: c.lang.reconfigure(lang) }))
      .catch((e) => console.warn('加载语法高亮失败', filename, e));
    return () => {
      alive = false;
      window.removeEventListener('pointerup', onUp);
      window.removeEventListener('pointercancel', onUp);
      v.destroy();
      view.current = null;
    };
    // Only rebuild when the document changes; after that the editor maintains value itself
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [docKey]);

  useEffect(() => {
    view.current?.dispatch({
      effects: comp.current.ro.reconfigure([EditorState.readOnly.of(readOnly), EditorView.editable.of(!readOnly)]),
    });
  }, [readOnly]);

  useEffect(() => {
    view.current?.dispatch({ effects: comp.current.wrap.reconfigure(wrap ? EditorView.lineWrapping : []) });
  }, [wrap]);

  useEffect(() => {
    view.current?.dispatch({ effects: comp.current.phrases.reconfigure(searchPhrases()) });
  }, [lang]);

  // Jumping: must also jump right after the editor is created, hence the docKey dependency
  useEffect(() => {
    const v = view.current;
    if (!v || !goto) return;
    applyGoto(v, goto);
  }, [goto?.n, docKey]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    const v = view.current;
    if (!v || !scrollLine) return;
    const doc = v.state.doc;
    const blk = v.lineBlockAt(doc.line(Math.max(1, Math.min(doc.lines, scrollLine.line))).from);
    v.scrollDOM.scrollTop = blk.top + scrollLine.frac * blk.height;
  }, [scrollLine?.n]); // eslint-disable-line react-hooks/exhaustive-deps

  const mod = typeof navigator !== 'undefined' && navigator.platform.includes('Mac') ? '⌘L' : 'Ctrl+L';

  return (
    <div
      className="wp-code-wrap"
      ref={wrapRef}
      onPointerDown={(e) => {
        if (e.button === 0 && !(e.target as Element | null)?.closest('.cm-sel-act, .ed-find')) {
          dragging.current = true;
          setSelChip(null);
        }
      }}
      onContextMenu={(e) =>
        view.current && menu && !(e.target as Element | null)?.closest('.cm-panels') && openEditorContextMenu(e, view.current, menu)
      }
    >
      <div className="wp-code" ref={host} />
      {selChip && onAddSelection && (
        <button
          type="button"
          className="cm-sel-act"
          style={{ top: selChip.top, left: selChip.left }}
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => {
            const v = view.current;
            const s = v ? getSelectionSpan(v.state) : null;
            if (s) {
              onAddSelection(s);
              v?.dispatch({ selection: { anchor: v.state.selection.main.to } });
              setSelChip(null);
            }
          }}
        >
          <Icon name="plus" size={12} />
          <span>{tx('addToChat')}</span>
          <kbd>{mod}</kbd>
        </button>
      )}
    </div>
  );
}
