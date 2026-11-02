import { useEffect, useRef } from 'react';
import { isEn, useLang } from '../i18n';
import { Compartment, EditorState } from '@codemirror/state';
import {
  drawSelection,
  EditorView,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  keymap,
  lineNumbers,
} from '@codemirror/view';
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { bracketMatching, HighlightStyle, indentOnInput, LanguageDescription, syntaxHighlighting } from '@codemirror/language';
import { highlightSelectionMatches, searchKeymap } from '@codemirror/search';
import { languages } from '@codemirror/language-data';
import { tags as t } from '@lezer/highlight';

// Code editor in the file panel (CodeMirror 6): syntax highlighting, line numbers, current line, find/replace, Ctrl+S to save.
// Language packs are lazy-loaded by file name and stay out of the main bundle

/** Colors use CSS variables; dark mode swaps them in styles.css */
const highlight = HighlightStyle.define([
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

const theme = EditorView.theme({
  '&': { height: '100%', fontSize: '12px', backgroundColor: 'var(--bg)', color: 'var(--text)' },
  '&.cm-focused': { outline: 'none' },
  '.cm-scroller': { fontFamily: 'var(--mono)', lineHeight: '20px' },
  '.cm-content': { padding: '4px 0', caretColor: 'var(--text)' },
  '.cm-gutters': { backgroundColor: 'var(--bg)', color: 'var(--text-3)', border: 'none' },
  '.cm-lineNumbers .cm-gutterElement': { padding: '0 12px 0 16px', minWidth: '44px' },
  '.cm-activeLine': { backgroundColor: 'var(--cm-active)' },
  '.cm-activeLineGutter': { backgroundColor: 'transparent', color: 'var(--text)' },
  '&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection': { backgroundColor: 'var(--cm-sel) !important' },
  '.cm-selectionMatch': { backgroundColor: 'var(--cm-match)' },
  '&.cm-focused .cm-matchingBracket': { backgroundColor: 'var(--cm-match)', outline: '1px solid var(--border-2)' },
  '.cm-searchMatch': { backgroundColor: 'var(--cm-find)' },
  '.cm-searchMatch.cm-searchMatch-selected': { backgroundColor: 'var(--cm-find-cur)' },
  '.cm-panels': { backgroundColor: 'var(--bg-2)', color: 'var(--text)', borderColor: 'var(--border)' },
  '.cm-panels.cm-panels-top': { borderBottom: '1px solid var(--border)' },
  // The global input { width: 100% } stretches the search box and checkboxes to a full row (pushing checkboxes away from their labels); size them to content here
  '.cm-panel.cm-search': { padding: '6px 30px 6px 10px', fontSize: '12px', lineHeight: '28px' },
  '.cm-panel.cm-search input, .cm-panel.cm-search button': { fontSize: '12px' },
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
  '.cm-panel.cm-search label': {
    display: 'inline-flex',
    alignItems: 'center',
    gap: '4px',
    margin: '2px 10px 2px 0',
    fontSize: '12px',
    color: 'var(--text-2)',
    whiteSpace: 'nowrap',
    verticalAlign: 'middle',
    cursor: 'pointer',
  },
  '.cm-panel.cm-search input[type=checkbox]': { width: 'auto', height: 'auto', margin: 0, padding: 0, boxShadow: 'none' },
  '.cm-panel.cm-search [name=close]': { top: '6px', right: '8px', fontSize: '16px', color: 'var(--text-3)', cursor: 'pointer' },
});

function searchPhrases() {
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
}: {
  /** Changes when switching files (or re-reading from disk); the editor is rebuilt from scratch */
  docKey: string;
  value: string;
  filename: string;
  readOnly: boolean;
  wrap: boolean;
  onChange: (v: string) => void;
  onSave: () => void;
  /** Jump to a line (col and len are character positions in the line, used to select the matched word); jumps again whenever n changes */
  goto?: { line: number; col?: number; len?: number; n: number } | null;
  /** Reports the top line while scrolling (frac is how much of that line is visible, 0–1); the Markdown split preview follows it */
  onTopLine?: (line: number, frac: number) => void;
  /** Scroll a line to the top (without changing the selection); scrolls again whenever n changes */
  scrollLine?: { line: number; frac: number; n: number } | null;
}) {
  const host = useRef<HTMLDivElement>(null);
  const [, lang] = useLang();
  const view = useRef<EditorView | null>(null);
  const comp = useRef({ lang: new Compartment(), ro: new Compartment(), wrap: new Compartment(), phrases: new Compartment() });
  // Callbacks go through a ref so the editor doesn't need rebuilding
  const cb = useRef({ onChange, onSave, onTopLine });
  cb.current = { onChange, onSave, onTopLine };

  useEffect(() => {
    const c = comp.current;
    const v = new EditorView({
      parent: host.current!,
      state: EditorState.create({
        doc: value,
        extensions: [
          lineNumbers(),
          highlightActiveLineGutter(),
          highlightSpecialChars(),
          history(),
          drawSelection(),
          indentOnInput(),
          bracketMatching(),
          highlightActiveLine(),
          highlightSelectionMatches(),
          syntaxHighlighting(highlight),
          keymap.of([
            { key: 'Mod-s', preventDefault: true, run: () => (cb.current.onSave(), true) },
            ...defaultKeymap,
            ...historyKeymap,
            ...searchKeymap,
            indentWithTab,
          ]),
          EditorState.tabSize.of(4),
          c.phrases.of(searchPhrases()),
          theme,
          c.lang.of([]),
          c.ro.of([EditorState.readOnly.of(readOnly), EditorView.editable.of(!readOnly)]),
          c.wrap.of(wrap ? EditorView.lineWrapping : []),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) cb.current.onChange(u.state.doc.toString());
          }),
        ],
      }),
    });
    view.current = v;
    let alive = true;
    const onScroll = () => {
      const f = cb.current.onTopLine;
      if (!f) return;
      const blk = v.lineBlockAtHeight(v.scrollDOM.scrollTop);
      f(v.state.doc.lineAt(blk.from).number, blk.height ? Math.max(0, Math.min(1, (v.scrollDOM.scrollTop - blk.top) / blk.height)) : 0);
    };
    v.scrollDOM.addEventListener('scroll', onScroll, { passive: true });
    const desc = LanguageDescription.matchFilename(languages, filename);
    desc
      ?.load()
      .then((lang) => alive && v.dispatch({ effects: c.lang.reconfigure(lang) }))
      .catch((e) => console.warn('加载语法高亮失败', filename, e));
    return () => {
      alive = false;
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
    const doc = v.state.doc;
    const ln = doc.line(Math.max(1, Math.min(doc.lines, goto.line)));
    const from = Math.min(ln.to, ln.from + (goto.col ?? 0));
    const to = Math.min(ln.to, from + (goto.len ?? 0));
    v.dispatch({
      selection: { anchor: from, head: to },
      effects: EditorView.scrollIntoView(from, { y: 'center' }),
    });
    v.focus();
  }, [goto?.n, docKey]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    const v = view.current;
    if (!v || !scrollLine) return;
    const doc = v.state.doc;
    const blk = v.lineBlockAt(doc.line(Math.max(1, Math.min(doc.lines, scrollLine.line))).from);
    v.scrollDOM.scrollTop = blk.top + scrollLine.frac * blk.height;
  }, [scrollLine?.n]); // eslint-disable-line react-hooks/exhaustive-deps

  return <div className="wp-code" ref={host} />;
}
