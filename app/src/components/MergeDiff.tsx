import { forwardRef, type ReactNode, useEffect, useImperativeHandle, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useBack } from '../back';
import { ChangeSet, Compartment, EditorState, StateEffect, StateField, Text, type Extension } from '@codemirror/state';
import {
  crosshairCursor,
  Decoration,
  type DecorationSet,
  drawSelection,
  EditorView,
  gutter,
  GutterMarker,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  keymap,
  lineNumbers,
  rectangularSelection,
  WidgetType,
} from '@codemirror/view';
import { closeBrackets, closeBracketsKeymap } from '@codemirror/autocomplete';
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { bracketMatching, foldGutter, foldKeymap, indentOnInput, indentUnit, LanguageDescription, syntaxHighlighting } from '@codemirror/language';
import { highlightSelectionMatches, searchPanelOpen } from '@codemirror/search';
import { languages } from '@codemirror/language-data';
import { Chunk, getChunks, getOriginalDoc, goToNextChunk, goToPreviousChunk, MergeView, originalDocChangeEffect, unifiedMergeView } from '@codemirror/merge';
import { getT, isEn, useLang } from '../i18n';
import { detectEol, detectIndent } from '../textFormat';
import {
  applyGoto,
  foldMarker,
   highlight,
  measureSelectionChip,
  openEditorContextMenu,
  searchPhrases,
  theme,
  type EditorMenuCtx,
  type GotoTarget,
} from './CodeEditor';
import { editorSearchExtension, editorSearchKeymap } from './EditorSearchPanel';
import { editorCodeNavExtension, isNavMod, type NavTrigger } from './EditorCodeNav';
import { Icon } from './Icon';
import type { MenuItem } from './Overlay';

const t = getT('diffView');
const tx = getT('filePanel');

// Full-file diff in the file panel:
// - Default layout is single-file inline ("unified"): one CodeMirror editor showing the full file with deleted lines in red
//   above added lines in green, syntax highlighting on both, and direct editing + Ctrl+S saving when viewing working-tree changes.
// - Side-by-side ("split") aligns two CodeMirror editors with synced scrolling and a draggable divider.
// - Hovering any diff hunk reveals a floating "Add to chat / Copy" bar; selecting lines reveals a floating "+ Add to chat (Ctrl+L)" button.

const DIFF_CONFIG = { scanLimit: 500, timeout: 300 };
const COLLAPSE = { margin: 3, minSize: 4 };
const CTX_LINES = 3;

export interface InlineReviewPlacement {
  id: string;
  line: number;
}

class ReviewSlotWidget extends WidgetType {
  readonly id: string;
  readonly line: number;
  private readonly mount: (id: string, dom: HTMLElement) => void;
  private readonly unmount: (id: string, dom: HTMLElement) => void;

  constructor(
    id: string,
    line: number,
    mount: (id: string, dom: HTMLElement) => void,
    unmount: (id: string, dom: HTMLElement) => void,
  ) {
    super();
    this.id = id;
    this.line = line;
    this.mount = mount;
    this.unmount = unmount;
  }

  eq(other: ReviewSlotWidget) {
    return other.id === this.id && other.line === this.line;
  }

  toDOM(view: EditorView) {
    const dom = document.createElement('div');
    dom.className = 'cm-review-widget-slot';
    dom.dataset.reviewId = this.id;
    const ro = new ResizeObserver(() => {
      try {
        view.requestMeasure();
      } catch {
        /* ignore after view destroy */
      }
    });
    ro.observe(dom);
    (dom as HTMLElement & { __yiRevRo?: ResizeObserver }).__yiRevRo = ro;
    this.mount(this.id, dom);
    return dom;
  }

  destroy(dom: HTMLElement) {
    const ro = (dom as HTMLElement & { __yiRevRo?: ResizeObserver }).__yiRevRo;
    ro?.disconnect();
    this.unmount(this.id, dom);
  }

  ignoreEvent() {
    return true;
  }
}

interface ReviewWidgetEffectSpec {
  items: InlineReviewPlacement[];
  mount: (id: string, dom: HTMLElement) => void;
  unmount: (id: string, dom: HTMLElement) => void;
}

const setInlineReviewsEffect = StateEffect.define<ReviewWidgetEffectSpec>();

function buildReviewDecos(doc: Text, spec: ReviewWidgetEffectSpec): DecorationSet {
  if (!spec.items.length || doc.lines < 1) return Decoration.none;
  const sorted = [...spec.items].sort((x, y) => x.line - y.line || x.id.localeCompare(y.id));
  const ranges = sorted.map((it) => {
    const ln = Math.max(1, Math.min(doc.lines, Math.floor(it.line)));
    const pos = doc.line(ln).to;
    return Decoration.widget({
      widget: new ReviewSlotWidget(it.id, ln, spec.mount, spec.unmount),
      block: true,
      side: 1,
    }).range(pos);
  });
  return Decoration.set(ranges, true);
}

const inlineReviewsField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(setInlineReviewsEffect)) {
        return buildReviewDecos(tr.state.doc, e.value);
      }
    }
    return tr.docChanged ? value.map(tr.changes) : value;
  },
  provide: (f) => EditorView.decorations.from(f),
});

class SignMarker extends GutterMarker {
  sign: string;
  cls: string;
  constructor(sign: string, cls: string) {
    super();
    this.sign = sign;
    this.cls = cls;
  }
  toDOM() {
    const span = document.createElement('span');
    span.className = this.cls;
    span.textContent = this.sign;
    return span;
  }
}
// Only the bar in the gutter, no sign, as in Cursor
const delSignMarker = new SignMarker('', 'cm-all-del-sign');

const allDeletedGutter = gutter({
  class: 'cm-changeGutter cm-all-del-gutter',
  lineMarker: () => delSignMarker,
});

const mergeTheme = EditorView.theme({
  // Line-level diff look matching chat DiffView: faint line tints, a 2px bar in the gutter, no +/- signs and no
  // word-level boxes; removed lines are dimmed
  '&.cm-merge-a .cm-changedLine, .cm-deletedChunk': { backgroundColor: 'var(--df-del-line)' },
  '&.cm-merge-b .cm-changedLine, .cm-inlineChangedLine': { backgroundColor: 'var(--df-add-line)' },
  '& .cm-deletedLine, & .cm-deletedLine span': { color: 'var(--text-3)' },
  '.cm-deletedChunk': { paddingLeft: '6px' },
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
  '.cm-changeGutter': { width: '6px', paddingLeft: '0' },
  '&.cm-merge-a .cm-changedLineGutter, .cm-deletedLineGutter': {
    background: 'transparent',
    borderLeft: '2px solid var(--df-del-mark)',
  },
  '&.cm-merge-b .cm-changedLineGutter': {
    background: 'transparent',
    borderLeft: '2px solid var(--df-add-mark)',
  },
});

function phrases() {
  const p = isEn() ? { '$ unchanged lines': '$ unchanged lines' } : { '$ unchanged lines': '$ 行未改动' };
  return [searchPhrases(), EditorState.phrases.of(p)];
}

/** Side by side, each editor scrolls itself and the two follow each other both ways */
function syncScroll(a: EditorView, b: EditorView): () => void {
  let applying: HTMLElement | null = null;
  const follow = (from: HTMLElement, to: HTMLElement) => () => {
    if (applying === from) {
      applying = null;
      return;
    }
    const top = to.scrollTop;
    const left = to.scrollLeft;
    to.scrollTop = from.scrollTop;
    to.scrollLeft = from.scrollLeft;
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

/** Keeps block review widgets framed inside the visible scrollport next to the gutter during horizontal code scrolling. */
function observeReviewViewport(v: EditorView): () => void {
  const sync = () => {
    const gutter = v.dom.querySelector('.cm-gutters');
    const gw = gutter ? Math.ceil(gutter.getBoundingClientRect().width) : 0;
    const vw = Math.max(200, Math.floor(v.scrollDOM.clientWidth - gw - 2));
    v.scrollDOM.style.setProperty('--cm-gutter-w', `${gw}px`);
    v.scrollDOM.style.setProperty('--cm-rev-w', `${vw}px`);
  };
  sync();
  const ro = new ResizeObserver(sync);
  ro.observe(v.scrollDOM);
  const gutter = v.dom.querySelector('.cm-gutters');
  if (gutter) ro.observe(gutter);
  const raf = requestAnimationFrame(sync);
  return () => {
    cancelAnimationFrame(raf);
    ro.disconnect();
  };
}

const norm = (s: string) => s.replace(/\r\n/g, '\n');

function spanChange(old: string, text: string): { from: number; to: number; insert: string } | null {
  if (old === text) return null;
  const max = Math.min(old.length, text.length);
  let start = 0;
  while (start < max && old.charCodeAt(start) === text.charCodeAt(start)) start++;
  let end = 0;
  while (end < max - start && old.charCodeAt(old.length - 1 - end) === text.charCodeAt(text.length - 1 - end)) end++;
  return { from: start, to: old.length - end, insert: text.slice(start, text.length - end) };
}

function patchDoc(view: EditorView, text: string) {
  const c = spanChange(view.state.doc.toString(), norm(text));
  if (c) view.dispatch({ changes: c });
}

function patchOriginal(view: EditorView, text: string) {
  const old = getOriginalDoc(view.state);
  const c = spanChange(old.toString(), norm(text));
  if (c) view.dispatch({ effects: originalDocChangeEffect(view.state, ChangeSet.of(c, old.length)) });
}

export interface DiffSnippet {
  kind: 'diff' | 'code';
  fromLine: number;
  toLine: number;
  added?: number;
  deleted?: number;
  content: string;
}

/** Build a unified diff snippet (with up to CTX_LINES unchanged lines around it) for chunk `idx` */
function extractChunkSnippet(docA: Text, docB: Text, chunks: readonly Chunk[], idx: number): DiffSnippet | null {
  const ch = chunks[idx];
  if (!ch) return null;
  const delLines: string[] = [];
  let delStart = 1;
  let insAfterA = 0;
  if (ch.fromA < ch.toA && docA.length > 0) {
    delStart = docA.lineAt(Math.min(ch.fromA, docA.length)).number;
    const delEnd = docA.lineAt(Math.min(ch.endA, docA.length)).number;
    for (let l = delStart; l <= delEnd; l++) delLines.push(docA.line(l).text);
  } else if (docA.length > 0) {
    insAfterA = ch.fromA > docA.length ? docA.lines : docA.lineAt(ch.fromA).number - 1;
  }

  const addLines: string[] = [];
  let addStart = 1;
  let addEnd = 1;
  let anchorB = 1;
  let insAfterB = 0;
  if (ch.fromB < ch.toB && docB.length > 0) {
    addStart = docB.lineAt(Math.min(ch.fromB, docB.length)).number;
    addEnd = docB.lineAt(Math.min(ch.endB, docB.length)).number;
    for (let l = addStart; l <= addEnd; l++) addLines.push(docB.line(l).text);
  } else if (docB.length > 0) {
    if (ch.fromB > docB.length) {
      anchorB = docB.lines;
      insAfterB = docB.lines;
    } else {
      anchorB = docB.lineAt(ch.fromB).number;
      insAfterB = anchorB - 1;
    }
  }

  if (!delLines.length && !addLines.length) return null;

  const prev = idx > 0 ? chunks[idx - 1] : null;
  const minCtxB =
    docB.length === 0
      ? 1
      : prev
        ? prev.fromB < prev.toB
          ? docB.lineAt(Math.min(prev.endB, docB.length)).number + 1
          : prev.fromB > docB.length
            ? docB.lines + 1
            : docB.lineAt(prev.fromB).number
        : 1;
  const next = idx + 1 < chunks.length ? chunks[idx + 1] : null;
  const maxCtxB =
    docB.length === 0
      ? 0
      : next
        ? next.fromB > docB.length
          ? docB.lines
          : docB.lineAt(next.fromB).number - 1
        : docB.lines;

  const firstChangedB = addLines.length ? addStart : insAfterB + 1;
  const lastChangedB = addLines.length ? addEnd : insAfterB;
  const beforeCtx: string[] = [];
  if (docB.length > 0) {
    for (let l = Math.max(minCtxB, firstChangedB - CTX_LINES); l <= firstChangedB - 1; l++) beforeCtx.push(docB.line(l).text);
  }
  const afterCtx: string[] = [];
  if (docB.length > 0) {
    for (let l = lastChangedB + 1; l <= Math.min(maxCtxB, lastChangedB + CTX_LINES); l++) afterCtx.push(docB.line(l).text);
  }

  const oldStart = Math.max(0, (delLines.length ? delStart : insAfterA + 1) - beforeCtx.length);
  const oldCount = beforeCtx.length + delLines.length + afterCtx.length;
  const newStart = Math.max(0, firstChangedB - beforeCtx.length);
  const newCount = beforeCtx.length + addLines.length + afterCtx.length;
  const header = `@@ -${oldStart},${oldCount} +${newStart},${newCount} @@`;
  const out = [
    header,
    ...beforeCtx.map((l) => ` ${l}`),
    ...delLines.map((l) => `-${l}`),
    ...addLines.map((l) => `+${l}`),
    ...afterCtx.map((l) => ` ${l}`),
  ].join('\n');

  return {
    kind: 'diff',
    fromLine: addLines.length ? addStart : anchorB,
    toLine: addLines.length ? addEnd : anchorB,
    added: addLines.length || undefined,
    deleted: delLines.length || undefined,
    content: out,
  };
}

/** Build a snippet for the user's line selection inside a diff editor */
function extractSelectionSnippet(
  view: EditorView,
  docA: Text,
  docB: Text,
  chunks: readonly Chunk[],
  side: 'a' | 'b' | 'del-file',
): DiffSnippet | null {
  const st = view.state;
  const main = st.selection.main;
  if (main.empty) return null;
  const fromLine = st.doc.lineAt(main.from).number;
  const end = st.doc.lineAt(main.to);
  const toLine = Math.max(fromLine, main.to > main.from && main.to === end.from ? end.number - 1 : end.number);

  if (side === 'del-file') {
    const lines: string[] = [];
    for (let l = fromLine; l <= toLine; l++) lines.push(`-${st.doc.line(l).text}`);
    const count = toLine - fromLine + 1;
    return {
      kind: 'diff',
      fromLine,
      toLine,
      deleted: count,
      content: [`@@ -${fromLine},${count} +0,0 @@`, ...lines].join('\n'),
    };
  }

  if (side === 'a') {
    let deleted = 0;
    const lines: string[] = [];
    for (let l = fromLine; l <= toLine; l++) {
      const pos = docA.line(l).from;
      const inDel = chunks.some((ch) => ch.fromA < ch.toA && pos >= ch.fromA && pos <= ch.endA);
      if (inDel) {
        deleted++;
        lines.push(`-${docA.line(l).text}`);
      } else lines.push(` ${docA.line(l).text}`);
    }
    if (!deleted) {
      return { kind: 'code', fromLine, toLine, content: docA.sliceString(docA.line(fromLine).from, docA.line(toLine).to) };
    }
    return {
      kind: 'diff',
      fromLine,
      toLine,
      deleted,
      content: [`@@ -${fromLine},${toLine - fromLine + 1} @@`, ...lines].join('\n'),
    };
  }

  // Selection in B (unified view or right pane of split view)
  let added = 0;
  let deleted = 0;
  const lines: string[] = [];
  const emittedChunks = new Set<Chunk>();
  const emitChunkDeletions = (ch: Chunk) => {
    if (emittedChunks.has(ch) || ch.fromA >= ch.toA || docA.length === 0) return;
    emittedChunks.add(ch);
    const ds = docA.lineAt(Math.min(ch.fromA, docA.length)).number;
    const de = docA.lineAt(Math.min(ch.endA, docA.length)).number;
    for (let d = ds; d <= de; d++) {
      deleted++;
      lines.push(`-${docA.line(d).text}`);
    }
  };

  for (let l = fromLine; l <= toLine; l++) {
    const ln = docB.line(l);
    for (const ch of chunks) {
      if (ch.fromB <= docB.length && docB.lineAt(ch.fromB).number === l) emitChunkDeletions(ch);
      else if (l === fromLine && ch.fromB < ch.toB && ln.from >= ch.fromB && ln.from <= ch.endB) emitChunkDeletions(ch);
    }
    const isAdd = chunks.some((ch) => ch.fromB < ch.toB && ln.from >= ch.fromB && ln.from <= ch.endB);
    if (isAdd) {
      added++;
      lines.push(`+${ln.text}`);
    } else {
      lines.push(` ${ln.text}`);
    }
  }
  if (toLine === docB.lines) {
    for (const ch of chunks) if (ch.fromB > docB.length) emitChunkDeletions(ch);
  }

  if (!added && !deleted) {
    return { kind: 'code', fromLine, toLine, content: docB.sliceString(docB.line(fromLine).from, docB.line(toLine).to) };
  }
  return {
    kind: 'diff',
    fromLine,
    toLine,
    added: added || undefined,
    deleted: deleted || undefined,
    content: [`@@ +${fromLine},${toLine - fromLine + 1} @@`, ...lines].join('\n'),
  };
}

type Live =
  | { kind: 'split'; a: EditorView; b: EditorView }
  | { kind: 'unified'; view: EditorView }
  | { kind: 'del-file'; view: EditorView };

/** Purely deleted file (old side non-empty, new side empty) has no layout toggle in read-only mode */
export const oneSided = (a: string, b: string) => a.length > 0 && b.length === 0;
const MIN_PANE = 120;

export interface MergeDiffHandle {
  next(): void;
  prev(): void;
  flush(): string | undefined;
  getValue(): string;
  isDirty(): boolean;
}

export const MergeDiff = forwardRef<
  MergeDiffHandle,
  {
    /** Identifies the opened file/comparison; changing it rebuilds the editor state */
    docKey?: string;
    /** Old text (left / deleted side) */
    a: string;
    /** New text (right / current side) */
    b: string;
    /** Picks the syntax highlighting */
    filename: string;
    layout: 'split' | 'unified';
    collapse: boolean;
    wrap: boolean;
    /** Editable working-tree diff when false (default true) */
    readOnly?: boolean;
    onChange?: (v: string) => void;
    onSave?: () => void;
    onToggleWrap?: () => void;
    goto?: GotoTarget | null;
    menu?: (ctx: EditorMenuCtx) => (MenuItem | '-')[];
    onAddContext?: (ref: DiffSnippet) => void;
    onAddReview?: (ref: DiffSnippet, comment: string) => void;
    onCopyHunk?: (content: string) => void;
    inlineReviews?: InlineReviewPlacement[];
    renderInlineReview?: (id: string) => ReactNode;
    sideAFilename?: string;
    sideBFilename?: string;
    onNav?: (t: NavTrigger) => void;
    onCursorPos?: (line: number, col: number, side: 'a' | 'b' | 'del-file') => void;
  }
>(function MergeDiff(
  {
    docKey = '',
    a,
    b,
    filename,
    layout,
    collapse,
    wrap,
    readOnly = true,
    onChange,
    onSave,
    onToggleWrap,
    goto,
    menu,
    onAddContext,
    onAddReview,
    onCopyHunk,
    inlineReviews,
    renderInlineReview,
    sideAFilename,
    sideBFilename,
    onNav,
    onCursorPos,
  },
  ref,
) {
  const host = useRef<HTMLDivElement>(null);
  const box = useRef<HTMLDivElement>(null);
  const nav = useRef<EditorView | null>(null);
  const [, lang] = useLang();
  const mountedSlots = useRef(new Map<string, HTMLElement>());
  const slotSyncRaf = useRef<number | null>(null);
  const [slotNodes, setSlotNodes] = useState<[string, HTMLElement][]>([]);
  const scheduleSlotSync = () => {
    if (slotSyncRaf.current != null) return;
    slotSyncRaf.current = requestAnimationFrame(() => {
      slotSyncRaf.current = null;
      setSlotNodes(Array.from(mountedSlots.current.entries()));
    });
  };
  const mountSlot = (id: string, dom: HTMLElement) => {
    mountedSlots.current.set(id, dom);
    scheduleSlotSync();
  };
  const unmountSlot = (id: string, dom: HTMLElement) => {
    if (mountedSlots.current.get(id) === dom) {
      mountedSlots.current.delete(id);
      scheduleSlotSync();
    }
  };
  const inlineItems = useMemo(() => inlineReviews ?? [], [inlineReviews]);
  const inlineKey = useMemo(() => inlineItems.map((x) => `${x.id}@${x.line}`).join('|'), [inlineItems]);
  const inlineItemsRef = useRef<InlineReviewPlacement[]>(inlineItems);
  inlineItemsRef.current = inlineItems;

  const allViews = (l: Live | null): EditorView[] => (!l ? [] : l.kind === 'split' ? [l.a, l.b] : [l.view]);

  const flush = (): string | undefined => {
    if (changeTimer.current !== null) {
      window.clearTimeout(changeTimer.current);
      changeTimer.current = null;
    }
    const l = live.current;
    const owner = committedRef.current;
    if (!l || l.kind === 'del-file' || !dirtyRef.current || owner.readOnly || !owner.onChange) return undefined;
    dirtyRef.current = false;
    const v = l.kind === 'unified' ? l.view : l.b;
    const nextVal = v.state.doc.sliceString(0, v.state.doc.length, eolRef.current);
    lastEmittedBRef.current = nextVal;
    pendingLocalBRef.current = { docKey: owner.docKey, value: nextVal };
    owner.onChange(nextVal);
    return nextVal;
  };

  useImperativeHandle(
    ref,
    () => ({
      next: () => nav.current && goToNextChunk(nav.current),
      prev: () => nav.current && goToPreviousChunk(nav.current),
      flush,
      getValue: () => {
        const l = live.current;
        if (!l || l.kind === 'del-file') return text.current.b;
        const v = l.kind === 'unified' ? l.view : l.b;
        return v.state.doc.sliceString(0, v.state.doc.length, eolRef.current);
      },
      isDirty: () => dirtyRef.current && !committedRef.current.readOnly && !!committedRef.current.onChange,
    }),
    [],
  );

  const same = useMemo(() => a === b || norm(a) === norm(b), [a, b]);
  const emptyReadOnly = readOnly && same;
  const deletedFile = readOnly && a.length > 0 && b.length === 0;
  const split = layout === 'split' && !deletedFile;
  const [ratio, setRatio] = useState(0.5);
  const [hoveredHunk, setHoveredHunk] = useState<{ index: number; top: number } | null>(null);
  const [selChip, setSelChip] = useState<{ top: number; left: number; side: 'a' | 'b' | 'del-file' } | null>(null);
  const [commentDraft, setCommentDraft] = useState<{ top: number; snippet: DiffSnippet; text: string } | null>(null);
  const commentRef = useRef<HTMLTextAreaElement>(null);
  const dragging = useRef(false);
  const activeSelSide = useRef<'a' | 'b' | 'del-file'>('b');
  useBack(!!commentDraft, () => setCommentDraft(null));
  useEffect(() => {
    setCommentDraft(null);
  }, [docKey]);
  useEffect(() => {
    if (commentDraft) requestAnimationFrame(() => commentRef.current?.focus());
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [!!commentDraft]);

  const text = useRef({ a, b });
  text.current = { a, b };
  const live = useRef<Live | null>(null);
  const roComp = useRef(new Compartment());
  const langComp = useRef(new Compartment());
  const wrapComp = useRef(new Compartment());
  const phrasesComp = useRef(new Compartment());
  const eolRef = useRef<'\r\n' | '\n'>('\n');
  const dirtyRef = useRef(false);
  const changeTimer = useRef<number | null>(null);
  const lastEmittedBRef = useRef<string | null>(null);
  const pendingLocalBRef = useRef<{ docKey: string; value: string } | null>(null);
  const builtTextRef = useRef<{ a: string; b: string } | null>(null);
  const patchingRef = useRef(false);
  const docKeyRef = useRef(docKey);
  docKeyRef.current = docKey;
  const committedRef = useRef({ docKey, onChange, readOnly });
  if (committedRef.current.docKey === docKey) {
    committedRef.current.onChange = onChange;
    committedRef.current.readOnly = readOnly;
  }
  const selRaf = useRef(0);
  const moveRaf = useRef(0);
  const pendingMove = useRef<{ clientX: number; clientY: number; target: EventTarget | null } | null>(null);
  const builtFilenameRef = useRef(filename);
  const wrapRef = useRef(wrap);
  wrapRef.current = wrap;
  const readOnlyRef = useRef(readOnly);
  readOnlyRef.current = readOnly;
  const filenameRef = useRef(filename);
  filenameRef.current = filename;
  const sideAFilenameRef = useRef(sideAFilename ?? filename);
  sideAFilenameRef.current = sideAFilename ?? filename;
  const sideBFilenameRef = useRef(sideBFilename ?? filename);
  sideBFilenameRef.current = sideBFilename ?? filename;
  const cb = useRef({ onChange, onSave, onToggleWrap, onAddContext, onAddReview, onCopyHunk, menu, onNav, onCursorPos });
  cb.current = { onChange, onSave, onToggleWrap, onAddContext, onAddReview, onCopyHunk, menu, onNav, onCursorPos };

  const applyMergeGoto = (l: Live, g: GotoTarget) => {
    if (l.kind === 'del-file') {
      applyGoto(l.view, g);
      return;
    }
    if (l.kind === 'split') {
      applyGoto(g.side === 'a' ? l.a : l.b, g);
      return;
    }
    if (g.side === 'a') {
      const v = l.view;
      const docA = getOriginalDoc(v.state);
      const docB = v.state.doc;
      const chunks = getChunks(v.state)?.chunks ?? [];
      let deltaLines = 0;
      for (const ch of chunks) {
        const hasA = ch.fromA < ch.toA && docA.length > 0;
        const startA = hasA ? docA.lineAt(Math.min(ch.fromA, docA.length)).number : docA.length === 0 ? 1 : ch.fromA > docA.length ? docA.lines + 1 : docA.lineAt(ch.fromA).number;
        const endA = hasA ? docA.lineAt(Math.min(ch.endA, docA.length)).number : startA - 1;
        const lenA = hasA ? endA - startA + 1 : 0;
        const hasB = ch.fromB < ch.toB && docB.length > 0;
        const startB = hasB ? docB.lineAt(Math.min(ch.fromB, docB.length)).number : 0;
        const endB = hasB ? docB.lineAt(Math.min(ch.endB, docB.length)).number : -1;
        const lenB = hasB ? endB - startB + 1 : 0;
        if (hasA && g.line >= startA && g.line <= endA) {
          const anchorPos = Math.min(ch.fromB, docB.length);
          v.dispatch({
            selection: { anchor: anchorPos },
            effects: EditorView.scrollIntoView(anchorPos, { y: 'center' }),
          });
          v.focus();
          return;
        }
        if (g.line < startA) {
          applyGoto(v, { ...g, line: Math.max(1, g.line + deltaLines) });
          return;
        }
        deltaLines += lenB - lenA;
      }
      applyGoto(v, { ...g, line: Math.max(1, g.line + deltaLines) });
      return;
    }
    applyGoto(l.view, g);
  };

  const getActiveSelectionView = (): { view: EditorView; side: 'a' | 'b' | 'del-file' } | null => {
    const l = live.current;
    if (!l) return null;
    if (l.kind === 'del-file') return !l.view.state.selection.main.empty ? { view: l.view, side: 'del-file' } : null;
    if (l.kind === 'unified') return !l.view.state.selection.main.empty ? { view: l.view, side: 'b' } : null;
    const pref = activeSelSide.current === 'a' ? [l.a, l.b] : [l.b, l.a];
    for (const v of pref) {
      if (!v.state.selection.main.empty) return { view: v, side: v === l.a ? 'a' : 'b' };
    }
    return null;
  };

  const syncSelChip = () => {
    const el = box.current;
    if (!el || dragging.current || !cb.current.onAddContext) {
      setSelChip(null);
      return;
    }
    const cur = getActiveSelectionView();
    if (!cur) {
      setSelChip(null);
      return;
    }
    if (searchPanelOpen(cur.view.state) && document.activeElement?.closest('.ed-find')) {
      setSelChip(null);
      return;
    }
    const pos = measureSelectionChip(cur.view, el);
    setSelChip(pos ? { ...pos, side: cur.side } : null);
  };

  const getSelectionSnippet = (at?: { view: EditorView; side: 'a' | 'b' | 'del-file' }): { cur: { view: EditorView; side: 'a' | 'b' | 'del-file' }; snip: DiffSnippet } | null => {
    const l = live.current;
    const cur = at ?? getActiveSelectionView();
    if (!l || !cur) return null;
    let snip: DiffSnippet | null = null;
    if (l.kind === 'del-file') {
      snip = extractSelectionSnippet(l.view, l.view.state.doc, Text.empty, [], 'del-file');
    } else if (l.kind === 'unified') {
      const chunks = getChunks(l.view.state)?.chunks ?? [];
      snip = extractSelectionSnippet(l.view, getOriginalDoc(l.view.state), l.view.state.doc, chunks, 'b');
    } else {
      const chunks = getChunks(l.b.state)?.chunks ?? [];
      snip = extractSelectionSnippet(cur.view, l.a.state.doc, l.b.state.doc, chunks, cur.side);
    }
    return snip ? { cur, snip } : null;
  };

  /** at: the view and side to take the selection from (right-click); default the one with a selection */
  const triggerSelectionAdd = (at?: { view: EditorView; side: 'a' | 'b' | 'del-file' }): boolean => {
    if (!cb.current.onAddContext) return false;
    const got = getSelectionSnippet(at);
    if (!got) return false;
    cb.current.onAddContext(got.snip);
    got.cur.view.dispatch({ selection: { anchor: got.cur.view.state.selection.main.to } });
    setSelChip(null);
    return true;
  };

  const openCommentPopover = (top: number, snippet: DiffSnippet) => {
    const h = box.current?.clientHeight ?? 400;
    const clampedTop = Math.max(8, Math.min(top, Math.max(8, h - 160)));
    setSelChip(null);
    setCommentDraft({ top: clampedTop, snippet, text: '' });
  };

  const submitComment = () => {
    if (!commentDraft || !cb.current.onAddReview) return;
    const body = commentDraft.text.trim();
    if (!body) return;
    cb.current.onAddReview(commentDraft.snippet, body);
    setCommentDraft(null);
  };

  const currentHunkSnippet = (idx: number): DiffSnippet | null => {
    const l = live.current;
    if (!l) return null;
    if (l.kind === 'del-file') {
      const doc = l.view.state.doc;
      if (!doc.length) return null;
      const lines: string[] = [];
      for (let i = 1; i <= doc.lines; i++) lines.push(`-${doc.line(i).text}`);
      return {
        kind: 'diff',
        fromLine: 1,
        toLine: doc.lines,
        deleted: doc.lines,
        content: [`@@ -1,${doc.lines} +0,0 @@`, ...lines].join('\n'),
      };
    }
    if (l.kind === 'unified') {
      const chunks = getChunks(l.view.state)?.chunks ?? [];
      return extractChunkSnippet(getOriginalDoc(l.view.state), l.view.state.doc, chunks, idx);
    }
    const chunks = getChunks(l.b.state)?.chunks ?? [];
    return extractChunkSnippet(l.a.state.doc, l.b.state.doc, chunks, idx);
  };

  const scheduleSyncSelChip = () => {
    if (selRaf.current) return;
    selRaf.current = requestAnimationFrame(() => {
      selRaf.current = 0;
      syncSelChip();
    });
  };

  useEffect(() => {
    const el = host.current;
    if (emptyReadOnly || !el) return;
    let alive = true;
    setHoveredHunk(null);
    setSelChip(null);
    committedRef.current = { docKey, onChange, readOnly };
    builtFilenameRef.current = filenameRef.current;
    const destroy = build(el);
    const fname = filenameRef.current;
    const desc = LanguageDescription.matchFilename(languages, fname);
    if (desc) {
      desc
        .load()
        .then((ext) => {
          if (!alive) return;
          const eff = langComp.current.reconfigure(ext);
          for (const v of allViews(live.current)) v.dispatch({ effects: eff });
        })
        .catch((e) => console.warn('加载语法高亮失败', fname, e));
    }
    return () => {
      alive = false;
      flush();
      if (moveRaf.current) {
        cancelAnimationFrame(moveRaf.current);
        moveRaf.current = 0;
      }
      if (selRaf.current) {
        cancelAnimationFrame(selRaf.current);
        selRaf.current = 0;
      }
      pendingMove.current = null;
      nav.current = null;
      live.current = null;
      destroy();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [docKey, layout, collapse, emptyReadOnly, deletedFile]);

  useEffect(() => {
    if (filename === builtFilenameRef.current) return;
    builtFilenameRef.current = filename;
    let alive = true;
    const desc = LanguageDescription.matchFilename(languages, filename);
    (desc ? desc.load().catch((e) => (console.warn('加载语法高亮失败', filename, e), [] as Extension)) : Promise.resolve([] as Extension)).then((ext) => {
      if (!alive) return;
      const eff = langComp.current.reconfigure(ext);
      for (const v of allViews(live.current)) v.dispatch({ effects: eff });
    });
    return () => {
      alive = false;
    };
  }, [filename]);

  useEffect(() => {
    const eff = wrapComp.current.reconfigure(wrap ? EditorView.lineWrapping : []);
    for (const v of allViews(live.current)) v.dispatch({ effects: eff });
  }, [wrap]);

  useEffect(() => {
    const eff = phrasesComp.current.reconfigure(phrases());
    for (const v of allViews(live.current)) v.dispatch({ effects: eff });
  }, [lang]);

  useEffect(() => {
    const l = live.current;
    if (!l || l.kind === 'del-file') return;
    const target = l.kind === 'unified' ? l.view : l.b;
    target.dispatch({
      effects: roComp.current.reconfigure([EditorState.readOnly.of(readOnly), EditorView.editable.of(!readOnly)]),
    });
  }, [readOnly]);

  // Text changed with the same layout: patch the open editors in place (no-op when the edit came from the editor itself)
  useEffect(() => {
    const l = live.current;
    if (!l) return;
    const prev = builtTextRef.current;
    builtTextRef.current = { a, b };
    if (prev && prev.a === a && prev.b === b) return;
    const ownEdit = b === lastEmittedBRef.current;
    if (ownEdit) lastEmittedBRef.current = null;
    const aChanged = !prev || prev.a !== a;
    const bChanged = !prev || prev.b !== b;
    patchingRef.current = true;
    try {
      if (l.kind === 'split') {
        if (aChanged) patchDoc(l.a, a);
        if (bChanged && !ownEdit) patchDoc(l.b, b);
      } else if (l.kind === 'unified') {
        if (aChanged) patchOriginal(l.view, a);
        if (bChanged && !ownEdit) patchDoc(l.view, b);
      } else {
        if (aChanged) patchDoc(l.view, a);
      }
    } finally {
      patchingRef.current = false;
    }
  }, [a, b]);

  useEffect(() => {
    const l = live.current;
    if (!l) return;
    const reviewView = l.kind === 'split' ? l.b : l.view;
    reviewView.dispatch({
      effects: setInlineReviewsEffect.of({
        items: inlineItemsRef.current,
        mount: mountSlot,
        unmount: unmountSlot,
      }),
    });
  }, [inlineKey, docKey, layout, emptyReadOnly, deletedFile]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    const l = live.current;
    if (!l || !goto) return;
    applyMergeGoto(l, goto);
  }, [goto?.n, docKey, layout]); // eslint-disable-line react-hooks/exhaustive-deps

  const build = (el: HTMLElement): (() => void) => {
    const { a, b } = text.current;
    builtTextRef.current = { a, b };
    const curDocKey = docKeyRef.current;
    const pending = pendingLocalBRef.current;
    pendingLocalBRef.current = null;
    const initB = pending && pending.docKey === curDocKey ? pending.value : b;
    const eol = detectEol(initB || a);
    eolRef.current = eol;
    dirtyRef.current = false;
    lastEmittedBRef.current = pending && pending.docKey === curDocKey ? initB : null;
    if (changeTimer.current !== null) {
      window.clearTimeout(changeTimer.current);
      changeTimer.current = null;
    }
    const ind = detectIndent(initB || a);

    const commonKeys = [
      { key: 'Alt-F5', run: goToNextChunk },
      { key: 'Shift-Alt-F5', run: goToPreviousChunk },
      { key: 'Mod-s', preventDefault: true, run: () => (flush(), cb.current.onSave?.(), !!cb.current.onSave) },
      { key: 'Alt-z', preventDefault: true, run: () => (cb.current.onToggleWrap?.(), !!cb.current.onToggleWrap) },
      { key: 'Mod-l', preventDefault: true, run: () => triggerSelectionAdd() },
    ];

    const makeExt = (editableSide: boolean, selSide: 'a' | 'b' | 'del-file'): Extension[] => [
      lineNumbers(),
      ...(editableSide ? [foldGutter({ markerDOM: foldMarker }), highlightActiveLineGutter(), highlightActiveLine()] : []),
      highlightSpecialChars(),
      ...(editableSide
        ? [
            history(),
            EditorState.allowMultipleSelections.of(true),
            EditorView.clickAddsSelectionRange.of((e) => e.altKey),
            rectangularSelection({ eventFilter: (e) => e.altKey && e.shiftKey }),
            crosshairCursor({ key: 'Shift' }),
            indentOnInput(),
            bracketMatching(),
            closeBrackets(),
          ]
        : []),
      drawSelection(),
      highlightSelectionMatches(),
      syntaxHighlighting(highlight),
      keymap.of([
        ...commonKeys,
        ...(editableSide ? [...closeBracketsKeymap, ...defaultKeymap, ...historyKeymap, ...foldKeymap, indentWithTab] : defaultKeymap),
        ...editorSearchKeymap,
      ]),
      editorSearchExtension(),
      editorCodeNavExtension({
        filename: () => (selSide === 'a' || selSide === 'del-file' ? sideAFilenameRef.current : sideBFilenameRef.current),
        deletedChunkFilename: () => sideAFilenameRef.current,
        side: selSide,
        onNav: (t) => cb.current.onNav?.(t),
        onCursor: (l, c) => cb.current.onCursorPos?.(l, c, selSide),
      }),
      EditorState.tabSize.of(ind.tabSize),
      indentUnit.of(ind.unit),
      editableSide
        ? roComp.current.of([EditorState.readOnly.of(readOnlyRef.current), EditorView.editable.of(!readOnlyRef.current)])
        : EditorState.readOnly.of(true),
      theme,
      mergeTheme,
      phrasesComp.current.of(phrases()),
      wrapComp.current.of(wrapRef.current ? EditorView.lineWrapping : []),
      langComp.current.of([]),
      ...(selSide !== 'a' ? [inlineReviewsField] : []),
      EditorView.updateListener.of((u) => {
        if (editableSide && !patchingRef.current && u.docChanged && !committedRef.current.readOnly && committedRef.current.onChange) {
          dirtyRef.current = true;
          if (changeTimer.current !== null) window.clearTimeout(changeTimer.current);
          changeTimer.current = window.setTimeout(() => {
            changeTimer.current = null;
            flush();
          }, 200);
        }
        if (u.transactions.some((tr) => tr.isUserEvent('select.search'))) {
          setSelChip(null);
          return;
        }
        if (u.selectionSet && !u.state.selection.main.empty) activeSelSide.current = selSide;
        if (u.selectionSet || u.docChanged || u.geometryChanged) scheduleSyncSelChip();
      }),
    ];

    const onUp = () => {
      if (!dragging.current) return;
      dragging.current = false;
      scheduleSyncSelChip();
    };
    window.addEventListener('pointerup', onUp);
    window.addEventListener('pointercancel', onUp);

    const collapseUnchanged = collapse ? COLLAPSE : undefined;
    if (deletedFile) {
      const v = new EditorView({
        parent: el,
        state: EditorState.create({ doc: norm(a), extensions: [...makeExt(false, 'del-file'), allDeletedGutter] }),
      });
      live.current = { kind: 'del-file', view: v };
      if (inlineItemsRef.current.length) {
        v.dispatch({
          effects: setInlineReviewsEffect.of({ items: inlineItemsRef.current, mount: mountSlot, unmount: unmountSlot }),
        });
      }
      const onSc = () => scheduleSyncSelChip();
      const stopObserve = observeReviewViewport(v);
      v.scrollDOM.addEventListener('scroll', onSc, { passive: true });
      const delRaf = goto ? requestAnimationFrame(() => applyMergeGoto({ kind: 'del-file', view: v }, goto)) : null;
      return () => {
        if (changeTimer.current !== null) {
          window.clearTimeout(changeTimer.current);
          changeTimer.current = null;
        }
        if (delRaf != null) cancelAnimationFrame(delRaf);
        if (slotSyncRaf.current != null) {
          cancelAnimationFrame(slotSyncRaf.current);
          slotSyncRaf.current = null;
        }
        mountedSlots.current.clear();
        setSlotNodes([]);
        window.removeEventListener('pointerup', onUp);
        window.removeEventListener('pointercancel', onUp);
        v.scrollDOM.removeEventListener('scroll', onSc);
        stopObserve();
        v.destroy();
      };
    }

    let views: EditorView[];
    let destroy: () => void;
    if (layout === 'split') {
      const mv = new MergeView({
        a: { doc: norm(a), extensions: makeExt(false, 'a') },
        b: { doc: norm(initB), extensions: makeExt(true, 'b') },
        parent: el,
        gutter: true,
        highlightChanges: false,
        collapseUnchanged,
        diffConfig: DIFF_CONFIG,
      });
      views = [mv.a, mv.b];
      live.current = { kind: 'split', a: mv.a, b: mv.b };
      if (inlineItemsRef.current.length) {
        mv.b.dispatch({
          effects: setInlineReviewsEffect.of({ items: inlineItemsRef.current, mount: mountSlot, unmount: unmountSlot }),
        });
      }
      const unsync = syncScroll(mv.a, mv.b);
      const stopObserve = observeReviewViewport(mv.b);
      const onSc = () => {
        scheduleSyncSelChip();
        setHoveredHunk(null);
      };
      mv.a.scrollDOM.addEventListener('scroll', onSc, { passive: true });
      mv.b.scrollDOM.addEventListener('scroll', onSc, { passive: true });
      destroy = () => {
        if (slotSyncRaf.current != null) {
          cancelAnimationFrame(slotSyncRaf.current);
          slotSyncRaf.current = null;
        }
        mountedSlots.current.clear();
        setSlotNodes([]);
        mv.a.scrollDOM.removeEventListener('scroll', onSc);
        mv.b.scrollDOM.removeEventListener('scroll', onSc);
        stopObserve();
        unsync();
        mv.destroy();
      };
    } else {
      const v = new EditorView({
        parent: el,
        state: EditorState.create({
          doc: norm(initB),
          extensions: [
            ...makeExt(true, 'b'),
            unifiedMergeView({
              original: norm(a),
              mergeControls: false,
              gutter: true,
              highlightChanges: false,
              syntaxHighlightDeletions: true,
              syntaxHighlightDeletionsMaxLength: 20000,
              collapseUnchanged,
              diffConfig: DIFF_CONFIG,
            }),
          ],
        }),
      });
      views = [v];
      live.current = { kind: 'unified', view: v };
      if (inlineItemsRef.current.length) {
        v.dispatch({
          effects: setInlineReviewsEffect.of({ items: inlineItemsRef.current, mount: mountSlot, unmount: unmountSlot }),
        });
      }
      const stopObserve = observeReviewViewport(v);
      const onSc = () => {
        scheduleSyncSelChip();
        setHoveredHunk(null);
      };
      v.scrollDOM.addEventListener('scroll', onSc, { passive: true });
      destroy = () => {
        if (slotSyncRaf.current != null) {
          cancelAnimationFrame(slotSyncRaf.current);
          slotSyncRaf.current = null;
        }
        mountedSlots.current.clear();
        setSlotNodes([]);
        v.scrollDOM.removeEventListener('scroll', onSc);
        stopObserve();
        v.destroy();
      };
    }

    const target = views[views.length - 1];
    nav.current = target;
    const raf = requestAnimationFrame(() => {
      if (goto && live.current) {
        applyMergeGoto(live.current, goto);
        return;
      }
      const first = getChunks(target.state)?.chunks[0];
      if (first) target.dispatch({ effects: EditorView.scrollIntoView(Math.min(first.fromB, target.state.doc.length), { y: 'start', yMargin: 48 }) });
    });
    return () => {
      if (changeTimer.current !== null) {
        window.clearTimeout(changeTimer.current);
        changeTimer.current = null;
      }
      cancelAnimationFrame(raf);
      window.removeEventListener('pointerup', onUp);
      window.removeEventListener('pointercancel', onUp);
      destroy();
    };
  };

  /** Locate the diff chunk under the pointer and place the hunk action bar at its top-right */
  const processPointerMove = (clientX: number, clientY: number, target: EventTarget | null) => {
    if (!onAddContext && !onAddReview && !onCopyHunk) return;
    if ((target as Element | null)?.closest('.wp-hunk-bar, .cm-sel-act, .cm-sel-group, .wp-comment-pop, .wp-merge-sash, .ed-find, .cm-review-widget-slot')) return;
    const el = box.current;
    const l = live.current;
    if (!el || !l) return;
    const boxRect = el.getBoundingClientRect();

    if (l.kind === 'del-file') {
      if (l.view.state.doc.length > 0) setHoveredHunk((cur) => (cur?.index === 0 && cur.top === 6 ? cur : { index: 0, top: 6 }));
      return;
    }

    if (l.kind === 'unified') {
      const v = l.view;
      const chunks = getChunks(v.state)?.chunks ?? [];
      if (!chunks.length) {
        if (hoveredHunk) setHoveredHunk(null);
        return;
      }
      const sr = v.scrollDOM.getBoundingClientRect();
      const delEl = (target as Element | null)?.closest?.<HTMLElement>('.cm-deletedChunk');
      let idx = -1;
      let directDelNode: HTMLElement | undefined;
      if (delEl && v.contentDOM.contains(delEl)) {
        const pos = v.posAtDOM(delEl);
        idx = chunks.findIndex((ch) => ch.fromB === pos);
        if (idx >= 0) directDelNode = delEl;
      }
      if (idx < 0) {
        const docY = clientY - sr.top + v.scrollDOM.scrollTop;
        const blk = v.lineBlockAtHeight(docY);
        idx = chunks.findIndex((ch) =>
          ch.fromB < ch.toB ? blk.from <= ch.endB && blk.to >= ch.fromB : Math.abs(blk.from - ch.fromB) <= 1 && Math.abs(clientY - (blk.top - v.scrollDOM.scrollTop + sr.top)) < 24,
        );
      }
      if (idx < 0) {
        if (hoveredHunk) setHoveredHunk(null);
        return;
      }
      const ch = chunks[idx];
      const delNode =
        directDelNode ??
        [...v.contentDOM.querySelectorAll<HTMLElement>('.cm-deletedChunk')].find(
          (d) => v.posAtDOM(d) === ch.fromB && d.getClientRects().length > 0,
        );
      const delRect = delNode?.getBoundingClientRect();
      const docLen = v.state.doc.length;
      const bTop = v.lineBlockAt(Math.min(ch.fromB, docLen)).top - v.scrollDOM.scrollTop + (sr.top - boxRect.top);
      const bBot = v.lineBlockAt(Math.min(ch.endB, docLen)).bottom - v.scrollDOM.scrollTop + (sr.top - boxRect.top);
      const rawTop = delRect ? delRect.top - boxRect.top : bTop;
      const rawBot = ch.fromB < ch.toB ? bBot : delRect ? delRect.bottom - boxRect.top : bTop + 22;
      const top = Math.round(Math.max(6, Math.min(rawTop + 2, Math.max(6, rawBot - 28))));
      setHoveredHunk((cur) => (cur?.index === idx && Math.abs(cur.top - top) < 2 ? cur : { index: idx, top }));
      return;
    }

    // Split layout
    const chunks = getChunks(l.b.state)?.chunks ?? [];
    if (!chunks.length) {
      if (hoveredHunk) setHoveredHunk(null);
      return;
    }
    const leftRect = l.a.scrollDOM.getBoundingClientRect();
    const isLeft = clientX < leftRect.right;
    const sv = isLeft ? l.a : l.b;
    const sr = sv.scrollDOM.getBoundingClientRect();
    const docY = clientY - sr.top + sv.scrollDOM.scrollTop;
    const blk = sv.lineBlockAtHeight(docY);
    const idx = chunks.findIndex((ch) =>
      isLeft
        ? ch.fromA < ch.toA && blk.from <= ch.endA && blk.to >= ch.fromA
        : ch.fromB < ch.toB && blk.from <= ch.endB && blk.to >= ch.fromB,
    );
    if (idx < 0) {
      if (hoveredHunk) setHoveredHunk(null);
      return;
    }
    const ch = chunks[idx];
    const bSr = l.b.scrollDOM.getBoundingClientRect();
    const docLenB = l.b.state.doc.length;
    const rawTop =
      ch.fromB < ch.toB || docLenB > 0
        ? l.b.lineBlockAt(Math.min(ch.fromB, docLenB)).top - l.b.scrollDOM.scrollTop + (bSr.top - boxRect.top)
        : l.a.lineBlockAt(Math.min(ch.fromA, l.a.state.doc.length)).top - l.a.scrollDOM.scrollTop + (leftRect.top - boxRect.top);
    const top = Math.round(Math.max(6, rawTop + 2));
    setHoveredHunk((cur) => (cur?.index === idx && Math.abs(cur.top - top) < 2 ? cur : { index: idx, top }));
  };

  const onPointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    if (!onAddContext && !onAddReview && !onCopyHunk) return;
    pendingMove.current = { clientX: e.clientX, clientY: e.clientY, target: e.target };
    if (moveRaf.current) return;
    moveRaf.current = requestAnimationFrame(() => {
      moveRaf.current = 0;
      const m = pendingMove.current;
      pendingMove.current = null;
      if (m) processPointerMove(m.clientX, m.clientY, m.target);
    });
  };

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

  const onContextMenu = (e: React.MouseEvent) => {
    const l = live.current;
    if (!l || !menu) return;
    if ((e.target as Element | null)?.closest('.cm-panels')) return;
    const v =
      l.kind === 'split'
        ? e.clientX < l.a.scrollDOM.getBoundingClientRect().right
          ? l.a
          : l.b
        : l.view;
    const side: 'a' | 'b' | 'del-file' = l.kind === 'del-file' ? 'del-file' : l.kind === 'split' && v === l.a ? 'a' : 'b';
    const sideFilename = side === 'a' || side === 'del-file' ? sideAFilenameRef.current : sideBFilenameRef.current;
    // "Add selection to chat" sends the same diff snippet as the floating button, deleted lines included
    openEditorContextMenu(
      e,
      v,
      (ctx) =>
        menu({
          ...ctx,
          oldSide: side === 'a' || side === 'del-file',
          addSelection: cb.current.onAddContext && ctx.selection ? () => void triggerSelectionAdd({ view: v, side }) : undefined,
        }),
      cb.current.onNav ? { filename: sideFilename, side, onNav: (t) => cb.current.onNav?.(t) } : undefined,
    );
  };

  if (emptyReadOnly) return <div className="pd-empty muted small">{t('bothSidesAreTheSame')}</div>;
  const mod = typeof navigator !== 'undefined' && navigator.platform.includes('Mac') ? '⌘L' : 'Ctrl+L';

  return (
    <div
      ref={box}
      className={`wp-merge wp-merge-${split ? 'split' : 'unified'} ${deletedFile ? 'wp-merge-all-del' : ''}`}
      style={split ? ({ '--split': ratio } as React.CSSProperties) : undefined}
      onPointerMove={onPointerMove}
      onPointerLeave={() => {
        if (moveRaf.current) {
          cancelAnimationFrame(moveRaf.current);
          moveRaf.current = 0;
        }
        pendingMove.current = null;
        setHoveredHunk(null);
      }}
      onPointerDown={(e) => {
        if (e.button === 0 && !isNavMod(e.nativeEvent) && !(e.target as Element | null)?.closest('.cm-sel-act, .cm-sel-group, .wp-comment-pop, .wp-hunk-bar, .wp-merge-sash, .ed-find, .cm-review-widget-slot')) {
          dragging.current = true;
          setSelChip(null);
        }
      }}
      onContextMenu={onContextMenu}
    >
      <div className="wp-merge-host" ref={host} />
      {renderInlineReview &&
        slotNodes.map(([id, slotEl]) => createPortal(renderInlineReview(id), slotEl, `rev-slot-${id}`))}
      {split && <div className="wp-merge-sash" title={t('dragToResizeDoubleClick')} onPointerDown={drag} onDoubleClick={() => setRatio(0.5)} />}
      {hoveredHunk && !selChip && !commentDraft && (onAddContext || onAddReview || onCopyHunk) && (
        <div className="wp-hunk-bar" style={{ top: hoveredHunk.top }} onMouseDown={(e) => e.preventDefault()}>
          {onAddReview && (
            <button
              type="button"
              className="wp-hunk-btn review"
              title={tx('addReviewComment')}
              onClick={() => {
                const snip = currentHunkSnippet(hoveredHunk.index);
                if (snip) openCommentPopover(hoveredHunk.top + 30, snip);
              }}
            >
              <Icon name="comment" size={12} />
              <span>{tx('comment')}</span>
            </button>
          )}
          {onAddContext && (
            <button
              type="button"
              className="wp-hunk-btn primary"
              title={tx('addDiffHunkToChat')}
              onClick={() => {
                const snip = currentHunkSnippet(hoveredHunk.index);
                if (snip) onAddContext(snip);
              }}
            >
              <Icon name="chat" size={12} />
              <span>{tx('addToChat')}</span>
            </button>
          )}
          {onCopyHunk && (
            <button
              type="button"
              className="wp-hunk-btn"
              title={tx('copyDiffHunk')}
              onClick={() => {
                const snip = currentHunkSnippet(hoveredHunk.index);
                if (snip) onCopyHunk(snip.content);
              }}
            >
              <Icon name="copy" size={12} />
              <span>{tx('copy')}</span>
            </button>
          )}
        </div>
      )}
      {selChip && !commentDraft && (onAddContext || onAddReview) && (
        <div className="cm-sel-group" style={{ top: selChip.top, left: selChip.left }} onMouseDown={(e) => e.preventDefault()}>
          {onAddReview && (
            <button
              type="button"
              className="cm-sel-act review"
              onClick={() => {
                const got = getSelectionSnippet();
                if (got) openCommentPopover(selChip.top + 30, got.snip);
              }}
            >
              <Icon name="comment" size={12} />
              <span>{tx('comment')}</span>
            </button>
          )}
          {onAddContext && (
            <button type="button" className="cm-sel-act" onClick={() => triggerSelectionAdd()}>
              <Icon name="plus" size={12} />
              <span>{tx('addToChat')}</span>
              <kbd>{mod}</kbd>
            </button>
          )}
        </div>
      )}
      {commentDraft && onAddReview && (
        <div className="wp-comment-pop" style={{ top: commentDraft.top }} onKeyDown={(e) => e.stopPropagation()}>
          <div className="wp-comment-head">
            <span className="wp-comment-loc mono">
              <Icon name="comment" size={12} />
              {`${filename}:${commentDraft.snippet.fromLine === commentDraft.snippet.toLine ? commentDraft.snippet.fromLine : `${commentDraft.snippet.fromLine}-${commentDraft.snippet.toLine}`}`}
            </span>
            <button type="button" className="icon-btn" title={tx('close')} onClick={() => setCommentDraft(null)}>
              <Icon name="x" size={12} />
            </button>
          </div>
          <textarea
            ref={commentRef}
            className="wp-comment-input"
            rows={3}
            value={commentDraft.text}
            placeholder={tx('reviewCommentPlaceholder')}
            onChange={(e) => setCommentDraft((c) => c && { ...c, text: e.target.value })}
            onKeyDown={(e) => {
              if (e.key === 'Escape' && !e.nativeEvent.isComposing) {
                e.preventDefault();
                setCommentDraft(null);
              } else if ((e.metaKey || e.ctrlKey) && e.key === 'Enter' && !e.nativeEvent.isComposing) {
                e.preventDefault();
                submitComment();
              }
            }}
          />
          <div className="wp-comment-foot">
            <span className="muted small">{tx('reviewCommentHint')}</span>
            <div className="row">
              <button type="button" className="btn small" onClick={() => setCommentDraft(null)}>
                {tx('cancel')}
              </button>
              <button type="button" className="btn small primary" disabled={!commentDraft.text.trim()} onClick={submitComment}>
                {tx('addReviewCommentBtn')}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
});
