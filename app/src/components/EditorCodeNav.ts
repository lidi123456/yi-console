import { syntaxTree } from '@codemirror/language';
import { getChunks, getOriginalDoc } from '@codemirror/merge';
import { EditorState, StateEffect, StateField, type Extension } from '@codemirror/state';
import { Decoration, type DecorationSet, EditorView, keymap, ViewPlugin, type ViewUpdate } from '@codemirror/view';

export interface NavToken {
  kind: 'symbol' | 'path' | 'url';
  text: string;
  from: number;
  to: number;
  line: number;
  col: number;
  container?: string;
}

export interface NavCoords {
  left: number;
  top: number;
  bottom: number;
}

export interface NavTrigger {
  mode: 'click' | 'definition' | 'references';
  token: NavToken;
  coords: NavCoords;
  side?: 'a' | 'b' | 'del-file';
  content: string;
}

export const IS_MAC = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform);

export function isNavMod(e: { ctrlKey: boolean; metaKey: boolean; altKey: boolean; shiftKey: boolean }): boolean {
  return IS_MAC ? e.metaKey && !e.ctrlKey && !e.altKey && !e.shiftKey : e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey;
}

const NON_NAV_KEYWORDS = new Set([
  'abstract', 'as', 'async', 'await', 'break', 'case', 'catch', 'class', 'const', 'continue', 'crate', 'debugger',
  'default', 'def', 'defer', 'del', 'delete', 'do', 'dyn', 'elif', 'else', 'enum', 'except', 'export', 'extends',
  'extern', 'false', 'finally', 'fn', 'for', 'from', 'func', 'function', 'global', 'go', 'goto', 'if', 'impl',
  'implements', 'import', 'in', 'instanceof', 'interface', 'is', 'lambda', 'let', 'loop', 'match', 'mod', 'module',
  'mut', 'namespace', 'new', 'nil', 'nonlocal', 'null', 'of', 'or', 'override', 'package', 'pass', 'private',
  'protected', 'pub', 'public', 'raise', 'readonly', 'ref', 'return', 'select', 'static', 'struct', 'super', 'switch',
  'trait', 'true', 'try', 'type', 'typeof', 'undefined', 'unsafe', 'use', 'var', 'void', 'where', 'while', 'with',
  'yield', 'and', 'not', 'sizeof', 'typedef', 'const_cast', 'static_cast',
]);

const URL_RE = /https?:\/\/[^\s"'`<>()[\]{}]+/g;
const PATH_IN_COMMENT_RE =
  /(?:\.{1,2}\/[A-Za-z0-9_.@-]+(?:\/[A-Za-z0-9_.@-]+)*|[A-Za-z0-9_.@-]+(?:\/[A-Za-z0-9_.-]+)+|[A-Za-z0-9_.-]+\.(?:tsx?|jsx?|mts|cts|mjs|cjs|rs|py|go|java|kt|c|h|cc|cpp|hpp|css|scss|less|html?|md|mdx|json|toml|ya?ml|sh))(?::\d+(?::\d+)?)?/g;
const IMPORT_LINE_RE = /^\s*(?:import\s|export\b.*\bfrom\s|.*require\s*\(|#include\s)/;
const PATH_IN_STRING_RE =
  /^(?:\.{1,2}(?:\/[A-Za-z0-9_.@-]+)*|[A-Za-z0-9_@~.-]+\/[A-Za-z0-9_.@/-]+|[A-Za-z0-9_.-]+\.(?:tsx?|jsx?|mts|cts|mjs|cjs|rs|py|go|java|kt|c|h|cc|cpp|hpp|css|scss|less|html?|md|mdx|json|toml|ya?ml|sh))(?::\d+(?::\d+)?)?$/;
const RUST_MOD_RE = /^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)\s*;/;
const ID_START_RE = /[A-Za-z_$]/;
const ID_CHAR_RE = /[A-Za-z0-9_$]/;

function isWordAt(ch: string): boolean {
  return ID_CHAR_RE.test(ch);
}

function findQuotedSpan(lineText: string, offset: number): { innerStart: number; innerEnd: number; text: string } | null {
  let activeQuote: string | null = null;
  let quoteStart = -1;
  for (let i = 0; i < lineText.length; i++) {
    const ch = lineText[i];
    if (activeQuote) {
      if (ch === '\\') {
        i++;
        continue;
      }
      if (ch === activeQuote) {
        if (offset >= quoteStart + 1 && offset <= i) {
          return { innerStart: quoteStart + 1, innerEnd: i, text: lineText.slice(quoteStart + 1, i) };
        }
        activeQuote = null;
        quoteStart = -1;
      }
    } else if (ch === '\'' || ch === '"' || ch === '`' || ch === '<') {
      if (ch === '<' && !/^\s*#include\s+</.test(lineText.slice(0, i + 1))) continue;
      activeQuote = ch === '<' ? '>' : ch;
      quoteStart = i;
    }
  }
  return null;
}

function extractContainerBefore(lineText: string, identStart: number): string | undefined {
  let i = identStart - 1;
  while (i >= 0 && /\s/.test(lineText[i])) i--;
  if (i < 0) return undefined;
  let sepLen = 0;
  if (lineText[i] === '.') {
    sepLen = i > 0 && lineText[i - 1] === '?' ? 2 : 1;
  } else if (i > 0 && lineText[i] === ':' && lineText[i - 1] === ':') {
    sepLen = 2;
  } else if (i > 0 && lineText[i] === '>' && lineText[i - 1] === '-') {
    sepLen = 2;
  }
  if (!sepLen) return undefined;
  let end = i - sepLen;
  while (end >= 0 && /\s/.test(lineText[end])) end--;
  if (end < 0 || !isWordAt(lineText[end])) return undefined;
  let start = end;
  while (start > 0 && isWordAt(lineText[start - 1])) start--;
  const word = lineText.slice(start, end + 1);
  return ID_START_RE.test(word[0] ?? '') ? word : undefined;
}

function extractTokenFromLine(
  lineText: string,
  lineNum: number,
  lineFrom: number,
  offset: number,
  filename: string,
  nodeNames: string[],
): NavToken | null {
  if (offset < 0 || offset > lineText.length) return null;

  // 1. External http(s) URLs anywhere on the line
  URL_RE.lastIndex = 0;
  let um: RegExpExecArray | null;
  while ((um = URL_RE.exec(lineText)) !== null) {
    const clean = um[0].replace(/[.,;:!?)\]}'"]+$/, '');
    if (offset >= um.index && offset <= um.index + clean.length) {
      return {
        kind: 'url',
        text: clean,
        from: lineFrom + um.index,
        to: lineFrom + um.index + clean.length,
        line: lineNum,
        col: um.index,
      };
    }
  }

  const isComment = nodeNames.some((n) => /comment/i.test(n));
  const isString = nodeNames.some((n) => /string|template|heredoc/i.test(n));
  const isNumber = nodeNames.some((n) => /number|integer|float/i.test(n));
  const isKeyword = nodeNames.some((n) => /keyword|boolean|null|undefined/i.test(n));

  // 2. Import / quoted path strings (also works before Lezer grammar finishes lazy-loading)
  const quoted = findQuotedSpan(lineText, offset);
  if (quoted || isString) {
    if (quoted) {
      const inner = quoted.text.trim();
      const looksLikeImport = IMPORT_LINE_RE.test(lineText);
      const isPkgPath =
        looksLikeImport &&
        inner.length > 0 &&
        !/\s/.test(inner) &&
        (inner.startsWith('.') || inner.startsWith('@') || inner.startsWith('~') || inner.startsWith('/') || inner.includes('/'));
      if (isPkgPath || PATH_IN_STRING_RE.test(inner)) {
        return {
          kind: 'path',
          text: inner,
          from: lineFrom + quoted.innerStart,
          to: lineFrom + quoted.innerEnd,
          line: lineNum,
          col: quoted.innerStart,
        };
      }
    }
    if (isString) return null;
  }

  // 3. Paths / file:line references inside comments
  if (isComment) {
    PATH_IN_COMMENT_RE.lastIndex = 0;
    let pm: RegExpExecArray | null;
    while ((pm = PATH_IN_COMMENT_RE.exec(lineText)) !== null) {
      if (offset >= pm.index && offset <= pm.index + pm[0].length) {
        return {
          kind: 'path',
          text: pm[0],
          from: lineFrom + pm.index,
          to: lineFrom + pm.index + pm[0].length,
          line: lineNum,
          col: pm.index,
        };
      }
    }
    return null;
  }

  // 4. Rust `mod foo;` declaration
  if (!filename || /\.rs$/i.test(filename)) {
    const rm = RUST_MOD_RE.exec(lineText);
    if (rm) {
      const modName = rm[1];
      const idx = lineText.indexOf(modName, rm[0].indexOf('mod'));
      if (idx >= 0 && offset >= idx && offset <= idx + modName.length) {
        return {
          kind: 'path',
          text: `mod:${modName}`,
          from: lineFrom + idx,
          to: lineFrom + idx + modName.length,
          line: lineNum,
          col: idx,
        };
      }
    }
  }

  if (isNumber || isKeyword) return null;

  // 5. CSS custom properties (`--var`)
  if (/\.(?:css|scss|sass|less)$/i.test(filename)) {
    let s = offset;
    let e = offset;
    const cssCh = (c: string) => /[A-Za-z0-9_-]/.test(c);
    while (s > 0 && cssCh(lineText[s - 1])) s--;
    while (e < lineText.length && cssCh(lineText[e])) e++;
    const cssWord = lineText.slice(s, e);
    if (cssWord.startsWith('--') && cssWord.length > 2) {
      return { kind: 'symbol', text: cssWord, from: lineFrom + s, to: lineFrom + e, line: lineNum, col: s };
    }
  }

  // 6. Standard identifier
  let anchor = offset;
  if (anchor === lineText.length || !isWordAt(lineText[anchor])) {
    if (anchor > 0 && isWordAt(lineText[anchor - 1])) anchor--;
    else return null;
  }
  let start = anchor;
  let end = anchor + 1;
  while (start > 0 && isWordAt(lineText[start - 1])) start--;
  while (end < lineText.length && isWordAt(lineText[end])) end++;
  const word = lineText.slice(start, end);
  if (!word || !ID_START_RE.test(word[0])) return null;

  const container = extractContainerBefore(lineText, start);
  const dotAccess = !!container || (start > 0 && (lineText[start - 1] === '.' || lineText.slice(Math.max(0, start - 2), start) === '::'));
  if (!dotAccess && NON_NAV_KEYWORDS.has(word)) return null;
  if (word === 'this' || word === 'self') return null;

  return {
    kind: 'symbol',
    text: word,
    from: lineFrom + start,
    to: lineFrom + end,
    line: lineNum,
    col: start,
    container,
  };
}

export function extractNavToken(state: EditorState, pos: number, filename = ''): NavToken | null {
  const clamped = Math.max(0, Math.min(state.doc.length, pos));
  const line = state.doc.lineAt(clamped);
  const tree = syntaxTree(state);
  const n1 = tree.resolveInner(clamped, -1);
  const n2 = clamped < state.doc.length ? tree.resolveInner(clamped, 1) : n1;
  const names = [n1.name, n2.name];
  return extractTokenFromLine(line.text, line.number, line.from, clamped - line.from, filename, names);
}

const setNavHoverEffect = StateEffect.define<{ from: number; to: number } | null>();

const navHoverField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(setNavHoverEffect)) {
        if (!e.value || e.value.from >= e.value.to) return Decoration.none;
        return Decoration.set([Decoration.mark({ class: 'cm-nav-link' }).range(e.value.from, e.value.to)]);
      }
    }
    return tr.docChanged ? value.map(tr.changes) : value;
  },
  provide: (f) => EditorView.decorations.from(f),
});

function measureTokenCoords(view: EditorView, from: number, to: number, fallback?: { x: number; y: number }): NavCoords {
  const cFrom = view.coordsAtPos(from);
  const cTo = view.coordsAtPos(to);
  if (cFrom) {
    return { left: cFrom.left, top: cFrom.top, bottom: cTo?.bottom ?? cFrom.bottom };
  }
  if (fallback) {
    return { left: fallback.x, top: fallback.y - 10, bottom: fallback.y + 10 };
  }
  const r = view.dom.getBoundingClientRect();
  return { left: r.left + 60, top: r.top + 40, bottom: r.top + 60 };
}

function caretAtPoint(x: number, y: number): { node: Node; offset: number } | null {
  const doc = document as Document & {
    caretPositionFromPoint?: (x: number, y: number) => { offsetNode: Node; offset: number } | null;
    caretRangeFromPoint?: (x: number, y: number) => Range | null;
  };
  if (typeof doc.caretPositionFromPoint === 'function') {
    const p = doc.caretPositionFromPoint(x, y);
    if (p) return { node: p.offsetNode, offset: p.offset };
  }
  if (typeof doc.caretRangeFromPoint === 'function') {
    const r = doc.caretRangeFromPoint(x, y);
    if (r) return { node: r.startContainer, offset: r.startOffset };
  }
  return null;
}

function tryDeletedChunkNav(
  view: EditorView,
  e: MouseEvent,
  delEl: HTMLElement,
  filename: string,
): NavTrigger | null {
  const caret = caretAtPoint(e.clientX, e.clientY);
  const lineEl = (e.target as Element | null)?.closest?.<HTMLElement>('.cm-deletedLine');
  if (!caret || caret.node.nodeType !== Node.TEXT_NODE || !lineEl || !delEl.contains(lineEl)) return null;
  const lineText = lineEl.textContent ?? '';
  let prefixLen = 0;
  const walker = document.createTreeWalker(lineEl, NodeFilter.SHOW_TEXT);
  let cur: Node | null;
  while ((cur = walker.nextNode())) {
    if (cur === caret.node) break;
    prefixLen += (cur.textContent ?? '').length;
  }
  const offset = prefixLen + caret.offset;
  let lineNum = 1;
  try {
    const anchorB = view.posAtDOM(delEl);
    const chunks = getChunks(view.state)?.chunks ?? [];
    const ch = chunks.find((c) => c.fromB === anchorB);
    const docA = getOriginalDoc(view.state);
    if (ch && docA.length > 0 && ch.fromA < ch.toA) {
      const startLineA = docA.lineAt(Math.min(ch.fromA, docA.length)).number;
      const delLines = Array.from(delEl.querySelectorAll('.cm-deletedLine'));
      const idx = delLines.indexOf(lineEl);
      if (idx >= 0) lineNum = Math.min(docA.lines, startLineA + idx);
    }
  } catch {
    /* ignore DOM mapping errors */
  }
  const tok = extractTokenFromLine(lineText, lineNum, 0, offset, filename, []);
  if (!tok) return null;
  const docA = getOriginalDoc(view.state);
  return {
    mode: 'click',
    token: tok,
    coords: { left: e.clientX, top: e.clientY - 10, bottom: e.clientY + 10 },
    side: 'a',
    content: docA.toString(),
  };
}

export function triggerNavAtCursor(
  view: EditorView,
  mode: 'definition' | 'references',
  filename: string,
  onNav: (t: NavTrigger) => void,
  side?: 'a' | 'b' | 'del-file',
): boolean {
  const pos = view.state.selection.main.head;
  const tok = extractNavToken(view.state, pos, filename);
  if (!tok) return false;
  if (mode === 'references' && tok.kind !== 'symbol') return false;
  onNav({
    mode,
    token: tok,
    coords: measureTokenCoords(view, tok.from, tok.to),
    side,
    content: view.state.doc.toString(),
  });
  return true;
}

export function editorCodeNavExtension(opts: {
  filename: () => string;
  side?: 'a' | 'b' | 'del-file';
  onNav: (t: NavTrigger) => void;
  onCursor?: (line: number, col: number) => void;
}): Extension {
  const plugin = ViewPlugin.fromClass(
    class {
      view: EditorView;
      lastPtr: { x: number; y: number } | null = null;
      hoverSpan: { from: number; to: number } | null = null;

      constructor(view: EditorView) {
        this.view = view;
        view.dom.addEventListener('mousemove', this.onMove);
        view.dom.addEventListener('mouseleave', this.onLeave);
        view.dom.addEventListener('mousedown', this.onMouseDown, true);
        window.addEventListener('keydown', this.onKeyChange);
        window.addEventListener('keyup', this.onKeyChange);
        window.addEventListener('blur', this.clearHover);
      }

      destroy() {
        this.view.dom.removeEventListener('mousemove', this.onMove);
        this.view.dom.removeEventListener('mouseleave', this.onLeave);
        this.view.dom.removeEventListener('mousedown', this.onMouseDown, true);
        window.removeEventListener('keydown', this.onKeyChange);
        window.removeEventListener('keyup', this.onKeyChange);
        window.removeEventListener('blur', this.clearHover);
        this.view.dom.classList.remove('cm-nav-active');
      }

      setHover(span: { from: number; to: number } | null) {
        const same =
          (!span && !this.hoverSpan) || (span && this.hoverSpan && span.from === this.hoverSpan.from && span.to === this.hoverSpan.to);
        if (same) return;
        this.hoverSpan = span;
        this.view.dom.classList.toggle('cm-nav-active', !!span);
        this.view.dispatch({ effects: setNavHoverEffect.of(span) });
      }

      clearHover = () => {
        this.setHover(null);
      };

      onLeave = () => {
        this.lastPtr = null;
        this.clearHover();
      };

      evaluateAt(x: number, y: number, modActive: boolean) {
        if (!modActive) {
          this.clearHover();
          return;
        }
        const pos = this.view.posAtCoords({ x, y });
        if (pos == null) {
          this.clearHover();
          return;
        }
        const tok = extractNavToken(this.view.state, pos, opts.filename());
        this.setHover(tok ? { from: tok.from, to: tok.to } : null);
      }

      onMove = (e: MouseEvent) => {
        this.lastPtr = { x: e.clientX, y: e.clientY };
        if ((e.target as Element | null)?.closest?.('.cm-panels, .cm-review-widget-slot, .cm-sel-act, .cm-sel-group, .ed-find')) {
          this.clearHover();
          return;
        }
        this.evaluateAt(e.clientX, e.clientY, isNavMod(e));
      };

      onKeyChange = (e: KeyboardEvent) => {
        if (!this.lastPtr) return;
        this.evaluateAt(this.lastPtr.x, this.lastPtr.y, isNavMod(e));
      };

      onMouseDown = (e: MouseEvent) => {
        if (e.button !== 0 || !isNavMod(e)) return;
        const targetEl = e.target as Element | null;
        if (targetEl?.closest?.('.cm-panels, .cm-review-widget-slot, .cm-sel-act, .cm-sel-group, .ed-find')) return;

        const delEl = targetEl?.closest?.<HTMLElement>('.cm-deletedChunk');
        if (delEl && this.view.contentDOM.contains(delEl)) {
          const trig = tryDeletedChunkNav(this.view, e, delEl, opts.filename());
          if (trig) {
            e.preventDefault();
            e.stopPropagation();
            this.clearHover();
            opts.onNav(trig);
          }
          return;
        }

        const pos = this.view.posAtCoords({ x: e.clientX, y: e.clientY });
        if (pos == null) return;
        const tok = extractNavToken(this.view.state, pos, opts.filename());
        if (!tok) return;
        e.preventDefault();
        e.stopPropagation();
        this.clearHover();
        // Move cursor onto the clicked token cleanly (matching VS Code behavior)
        this.view.dispatch({ selection: { anchor: pos } });
        this.view.focus();
        opts.onNav({
          mode: 'click',
          token: tok,
          coords: measureTokenCoords(this.view, tok.from, tok.to, { x: e.clientX, y: e.clientY }),
          side: opts.side,
          content: this.view.state.doc.toString(),
        });
      };

      update(_u: ViewUpdate) {}
    },
  );

  const navKeymap = keymap.of([
    {
      key: 'F12',
      preventDefault: true,
      run: (view) => triggerNavAtCursor(view, 'definition', opts.filename(), opts.onNav, opts.side),
    },
    {
      key: 'Shift-F12',
      preventDefault: true,
      run: (view) => triggerNavAtCursor(view, 'references', opts.filename(), opts.onNav, opts.side),
    },
  ]);

  const cursorListener = EditorView.updateListener.of((u) => {
    if (!opts.onCursor || !u.selectionSet) return;
    const head = u.state.selection.main.head;
    const ln = u.state.doc.lineAt(head);
    opts.onCursor(ln.number, head - ln.from);
  });

  return [navHoverField, plugin, navKeymap, cursorListener];
}
