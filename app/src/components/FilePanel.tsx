import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useBack } from '../back';
import { getT, trErr, useLang } from '../i18n';
import { errorToast, getState, hostStatus, loadSession, openNewSession, skey, toast, useSessionData, useStore } from '../store';
import { isTauri, rpc } from '../transport';
import { TOOL_LABEL, type DirEntry, type FileDiff, type HubEvent, type QuestionAnswer } from '../types';
import { fmtSize, lsGet, lsSet } from '../util';
import { writeClipboard } from '../clipboard';
import { addContextRefToChat } from '../contextRefs';
import {
  computeFileIssueBadges,
  extractFindingThreads,
  extractReviewSnapshot,
  getActiveFindingStream,
  getActiveReviewId,
  getAllDispositions,
  getFindingDisposition,
  normalizeReviewPath,
  resolveReviewMeta,
  setActiveReviewId,
  subscribeFocusFinding,
  useReviewStoreVersion,
  type FileIssueBadge,
  type ReviewIssue,
} from '../reviewState';
import { Icon, ToolBadge } from './Icon';
import { askConfirm, askText, openMenu, type MenuItem } from './Overlay';
import { CodeEditor, type EditorMenuCtx, type GotoTarget } from './CodeEditor';
import { PaneDiff } from './DiffView';
import { MergeDiff, oneSided, type MergeDiffHandle } from './MergeDiff';
import { CsvView, ImageView, MdPreview, PdfView } from './FilePreview';
import { GrepPanel } from './GrepPanel';
import { QuickOpen, type QuickMode } from './QuickOpen';
import { OPEN_CHANGES, OPEN_FILE, takeOpenChanges, takeOpenFile, takeOpenTurn } from './SessionActions';
import { BrowserPane, isWebTab, WEB_PREFIX, webTabFile, webTabTitle } from './Browser';
import { AI_PREFIX, aiTabSession, AiBrowserPane, isAiTab, useAiBrowserBusy } from './AiBrowser';
import { registerWebTabs } from './NativeWeb';
import { canUseBrowser } from '../browserUse';
import { CommitDetails, commitRangeTab, commitTab, GitHistory, isCommitTab, parseCommitTab } from './GitHistory';
import { BranchWorktreePopover, CommitDialog, type BranchesResult, type WorktreeEntry, type WorktreesResult } from './GitOpsDialogs';
import {
  adoptReviewFindings,
  deleteReviewMeta,
  getReviewMeta,
  isReviewTab,
  ReviewLaunchDialog,
  type ReviewLaunchPreset,
  ReviewPane,
  reviewTabSession,
} from './ReviewTab';
import { ReviewInlineWidget } from './ReviewInlineWidget';
import {
  isSideTab,
  sideChatHasMessages,
  SideChatPane,
  sideChatTitle,
  sideChatTools,
  sideTabPath,
  sideTabSession,
  startSideChat,
} from './SideChatTab';

const tx = getT('filePanel');
const aiT = getT('browserUse');

// The "workspace files" panel on the right of the session page, modeled on the VS Code / Cursor explorer:
// - on desktop the panel always shows the preview area on the left and the tree on the right (collapsible), with a placeholder until a file is opened;
//   on mobile, with no file open it's just the file tree
// - the tree marks git changes (M / U / D); the "Changes" view lists only files changed in this workspace, and opening a changed file shows its diff
// - above the editor are tabs for opened files, then a clickable path; clicking a directory pops up that directory's tree
// - three sidebar views: all files / changes / search (file contents); Ctrl+P quick open, @ for symbols, : to go to a line
// - under Changes, a collapsible commit history box; a file in a commit opens as a read-only diff tab (git:<hash>:<path>)
// - files can be dragged into the input box (inserts @path), and system files dropped on the tree are uploaded to that directory
// - another file can be opened side by side on the right, or two files compared; diffs can be side by side or unified

interface OpenFile {
  path: string;
  content: string;
  /** Content being edited */
  draft: string;
  binary: boolean;
  truncated: boolean;
  /** Not valid UTF-8 (GBK and the like): shown lossily, so read-only to keep saving from corrupting it */
  notUtf8?: boolean;
  size: number;
  modified: number;
  /** Image: data URL */
  image?: string;
  /** Incremented on every read from disk; the editor rebuilds when it changes */
  rev: number;
  /** File deleted in git: only the diff can be viewed */
  deleted?: boolean;
  /** PDF: base64 */
  pdf?: string;
  /** Browser tab (path is web:url) */
  web?: boolean;
  /** A file in a commit or commit range (path is git:<hash>:<rel> or git:<base>..<end>:<rel>): read-only diff */
  commit?: { hash: string; baseHash?: string; rel: string; subject: string; message: string; author: string; time: number; status: string; diff: string };
  /** Untitled tab (path is untitled:Untitled-N): not on disk yet; saving asks for a path */
  untitled?: boolean;
  /** Cross-tool read-only code review tab (path is review:<sessionId>) */
  review?: boolean;
  /** Hidden side chat tab (path is side:<sessionId>) */
  side?: boolean;
}

/** Dragging a file from the file panel to the input box: the value is "@relative-path " */
export const DRAG_REF = 'application/x-yi-console-ref';
/** Per-file size limit for uploads dropped onto the file tree (same as yi-agentd) */
const UPLOAD_MAX = 50 * 1024 * 1024;
const TREE_DEFAULT = 300;
const TREE_MIN = 200;
const TREE_MAX = 600;
const MAIN_MIN = 220;

/** Pointer-capture horizontal resize helper for right-docked split panes (dragging left widens, right narrows). */
export function beginWidthDrag(
  e: React.PointerEvent<HTMLDivElement>,
  {
    initialWidth,
    min,
    max,
    onMove,
    onEnd,
    cleanupRef,
  }: {
    initialWidth: number;
    min: number;
    max: number;
    onMove: (width: number) => void;
    onEnd: (width: number) => void;
    cleanupRef: React.MutableRefObject<(() => void) | null>;
  },
) {
  if (e.button !== 0) return;
  e.preventDefault();
  cleanupRef.current?.();

  const handle = e.currentTarget;
  const pointerId = e.pointerId;
  const x0 = e.clientX;
  const w0 = initialWidth;
  let w = Math.round(Math.max(min, Math.min(max, w0)));
  let captured = false;
  try {
    handle.setPointerCapture(pointerId);
    captured = typeof handle.hasPointerCapture === 'function' ? handle.hasPointerCapture(pointerId) : true;
  } catch {
    captured = false;
  }

  const target: EventTarget = captured ? handle : window;
  let done = false;

  const move = (ev: Event) => {
    const pe = ev as PointerEvent;
    w = Math.round(Math.max(min, Math.min(max, w0 - (pe.clientX - x0))));
    onMove(w);
  };

  const finish = () => {
    if (done) return;
    done = true;
    target.removeEventListener('pointermove', move);
    target.removeEventListener('pointerup', finish);
    target.removeEventListener('pointercancel', finish);
    if (captured) {
      handle.removeEventListener('lostpointercapture', finish);
      try {
        if (handle.hasPointerCapture(pointerId)) handle.releasePointerCapture(pointerId);
      } catch {
        /* ignore */
      }
    }
    handle.classList.remove('dragging');
    document.body.classList.remove('resizing');
    if (cleanupRef.current === finish) cleanupRef.current = null;
    onEnd(w);
  };

  cleanupRef.current = finish;
  handle.classList.add('dragging');
  document.body.classList.add('resizing');
  target.addEventListener('pointermove', move);
  target.addEventListener('pointerup', finish);
  target.addEventListener('pointercancel', finish);
  if (captured) handle.addEventListener('lostpointercapture', finish);
}

/**
 * Files "copied" in the file panel: put the relative path on the clipboard (pasting elsewhere gives the path) and remember it;
 * when exactly that text is pasted into the input box, turn it into an @reference (same as dragging into the input box)
 */
export let copiedRef: { plain: string; ref: string } | null = null;

export function toB64(buf: ArrayBuffer): string {
  const u = new Uint8Array(buf);
  let bin = '';
  for (let i = 0; i < u.length; i += 0x8000) bin += String.fromCharCode(...u.subarray(i, i + 0x8000));
  return btoa(bin);
}

const isCsv = (p: string) => ['csv', 'tsv'].includes(extOf(p));
const isHtml = (p: string) => ['html', 'htm'].includes(extOf(p));

/** git changes: keyed by normalized absolute path */
/** Untitled tabs, as in VS Code (double-click the empty tab bar): key untitled:Untitled-N; the text stays in memory until saved */
const UNTITLED = 'untitled:';
const isUntitled = (t: string) => t.startsWith(UNTITLED);
/** Untitled text per panel (keyed like the remembered tabs), so it survives the panel remounting; it isn't kept across app restarts */
const untitledCache = new Map<string, Map<string, string>>();
const untitledStore = (key: string) => {
  let m = untitledCache.get(key);
  if (!m) untitledCache.set(key, (m = new Map()));
  return m;
};
/** VS Code's label for an untitled file: its first line (whitespace collapsed, at most 40 characters, needs a letter or digit), else Untitled-N */
const untitledLabel = (t: string, text: string) => {
  const first = text.split(/\r?\n/, 1)[0].slice(0, 400).trim().replace(/\s+/g, ' ');
  return /[\p{L}\p{N}]/u.test(first) ? [...first].slice(0, 40).join('') : t.slice(UNTITLED.length);
};
const isAbsPath = (p: string) => /^([a-zA-Z]:)?[\\/]/.test(p);

/** A file's content at a revision (git.blob) */
type GitBlob = { exists: boolean; text?: string; binary: boolean; tooLarge: boolean; notUtf8: boolean };
const blobKey = (rev: string, rel: string) => `${rev}\n${rel}`;

/** added / deleted: lines added/removed relative to HEAD; null when not computable (binary etc.); older yi-agentd doesn't send these fields */
type GitFile = { status: string; diff: string; path: string; rel: string; added?: number | null; deleted?: number | null };

/** "+12 −3": shown only in the Changes view */
export function LineStat({ a, d }: { a?: number | null; d?: number | null }) {
  if (a == null && d == null) return null;
  return (
    <span className="wp-stat">
      {!!a && <span className="add">+{a}</span>}
      {!!d && <span className="del">−{d}</span>}
    </span>
  );
}

interface GitInfo {
  repo: string;
  branch: string;
  /** Current HEAD commit (older yi-agentd doesn't send it) */
  head?: string | null;
  /** Keyed by norm(absolute path); rel is the path relative to the repo root (used for git operations) */
  files: Map<string, GitFile>;
  /** Directories containing changes */
  dirs: Set<string>;
  /** Ignored file paths (norm(absolute path)) from git status --ignored=matching */
  ignoredFiles: Set<string>;
  /** Ignored directory paths (norm(absolute path)), plus `<repo>/.git` */
  ignoredDirs: Set<string>;
  /** Total changed files when yi-agentd returned only the first 10000 */
  total?: number | null;
}

/** Whether `absPath` (file or directory) is ignored by git or sits inside an ignored directory */
function isGitIgnored(git: GitInfo | null | undefined, absPath: string): boolean {
  if (!git || !absPath) return false;
  const k = norm(absPath);
  if (git.files.has(k)) return false;
  if (git.ignoredFiles.has(k) || git.ignoredDirs.has(k)) return true;
  let i = k.lastIndexOf('/');
  while (i > 0) {
    const d = k.slice(0, i);
    if (git.ignoredDirs.has(d)) return true;
    i = d.lastIndexOf('/');
  }
  return false;
}

/** Older yi-agentd on Windows returned child paths with a \\?\ prefix (and they got stored in tab records); strip it everywhere */
const unverbatim = (p: string) => (/^\\\\\?\\[a-z]:/i.test(p) ? p.slice(4) : p);

/** For comparing paths: use / everywhere; Windows drive paths are case-insensitive */
const norm = (p: string) => {
  const s = p.replace(/\\/g, '/').replace(/\/+$/, '');
  return /^[a-z]:/i.test(s) ? s.toLowerCase() : s;
};

/** porcelain status code -> letter and color to display */
export function gitMark(status: string): [string, string] {
  if (status === 'new' || status === '??') return ['U', 'g-new'];
  if (status.includes('U')) return ['!', 'g-del'];
  if (status.includes('D')) return ['D', 'g-del'];
  if (status.includes('A')) return ['A', 'g-new'];
  if (status.includes('R')) return ['R', 'g-ren'];
  return ['M', 'g-mod'];
}

export function gitText(code: string): string {
  if (code === 'U') return tx('untracked');
  if (code === 'A') return tx('added');
  if (code === 'D') return tx('deleted');
  if (code === 'R') return tx('renamed');
  if (code === 'M') return tx('modified');
  if (code === '!') return tx('conflict');
  return code;
}

type SaveState = 'saving' | 'conflict' | 'error' | null;

function saveStateText(state: string): string {
  if (state === 'dirty') return tx('modified');
  if (state === 'saving') return tx('saving');
  if (state === 'conflict') return tx('changedOnDisk');
  if (state === 'error') return tx('saveFailed');
  return state;
}

/** Insert text into the session input box (Composer listens for it) */
export const INSERT_COMPOSER = 'yi-console:insert-composer';

const sepOf = (p: string) => (p.includes('\\') && !p.includes('/') ? '\\' : '/');
const join = (dir: string, name: string) => {
  const sep = sepOf(dir);
  return dir.endsWith(sep) ? dir + name : dir + sep + name;
};
const parentOf = (p: string) => {
  const i = Math.max(p.lastIndexOf('/'), p.lastIndexOf('\\'));
  return i > 0 ? p.slice(0, i) : p;
};
const baseName = (p: string) => p.slice(Math.max(p.lastIndexOf('/'), p.lastIndexOf('\\')) + 1);
const extOf = (name: string) => (name.includes('.') ? name.slice(name.lastIndexOf('.') + 1).toLowerCase() : '');
/** Title of a commit or commit-range file tab: "name @abc1234" or "name @abc1234..def5678"; null if t isn't one */
const commitTabLabel = (t: string) => {
  const c = parseCommitTab(t);
  if (!c) return null;
  return c.baseHash ? `${baseName(c.rel)} @${c.baseHash.slice(0, 7)}..${c.hash.slice(0, 7)}` : `${baseName(c.rel)} @${c.hash.slice(0, 7)}`;
};

const IMAGE_MIME: Record<string, string> = {
  png: 'image/png',
  jpg: 'image/jpeg',
  jpeg: 'image/jpeg',
  gif: 'image/gif',
  webp: 'image/webp',
  bmp: 'image/bmp',
  ico: 'image/x-icon',
  svg: 'image/svg+xml',
  avif: 'image/avif',
};
const isMarkdown = (p: string) => ['md', 'markdown', 'mdx'].includes(extOf(p));

// ---------- File icons: shape and color by extension ----------

type Glyph = 'doc' | 'code' | 'term' | 'json' | 'image' | 'md' | 'gear' | 'table' | 'pdf';
const GLYPH: Record<Glyph, string> = {
  doc: 'M4 1.5h5.5L13 5v9.5H4zM9.5 1.5V5H13',
  code: 'M6 4.5L2.5 8 6 11.5M10 4.5L13.5 8 10 11.5',
  term: 'M2 3h12v10H2zM4.5 6.5L6.5 8l-2 1.5M8 10h3',
  json: 'M6 2.5C4.5 2.5 4.5 3.5 4.5 5S4 8 3 8c1 0 1.5 1.5 1.5 3s0 2.5 1.5 2.5M10 2.5c1.5 0 1.5 1 1.5 2.5S12 8 13 8c-1 0-1.5 1.5-1.5 3s0 2.5-1.5 2.5',
  image: 'M2 3h12v10H2zM2 11l3.5-3.5 3 3 2-2L14 12M10.5 6h.01',
  md: 'M1.5 4h13v8h-13zM3.5 10V6l2 2 2-2v4M11 6v4M9.5 8.5L11 10l1.5-1.5',
  table: 'M2 3h12v10H2zM2 6.5h12M2 10h12M6 3v10',
  pdf: 'M4 1.5h5.5L13 5v9.5H4zM9.5 1.5V5H13M6 11.5c1.5-1 2.5-3 2.5-5 0 2 1 4 2.5 4.5',
  gear: 'M8 10a2 2 0 1 0 0-4 2 2 0 0 0 0 4zM8 1.5v2M8 12.5v2M1.5 8h2M12.5 8h2M3.4 3.4l1.4 1.4M11.2 11.2l1.4 1.4M3.4 12.6l1.4-1.4M11.2 4.8l1.4-1.4',
};
const KINDS: [RegExp, Glyph, string][] = [
  [/^(ts|tsx|mts|cts)$/, 'code', '#3178c6'],
  [/^(js|jsx|mjs|cjs)$/, 'code', '#c9a400'],
  [/^rs$/, 'code', '#ce7a3d'],
  [/^(py|pyi)$/, 'code', '#3572a5'],
  [/^go$/, 'code', '#00add8'],
  [/^(java|kt|kts|scala|groovy)$/, 'code', '#b07219'],
  [/^(c|h|cc|cpp|hpp|cxx|m|mm)$/, 'code', '#6866b8'],
  [/^(cs|fs|vb)$/, 'code', '#178600'],
  [/^(html|htm|xml|xaml|vue|svelte)$/, 'code', '#e34c26'],
  [/^(css|scss|sass|less)$/, 'code', '#7a55c7'],
  [/^(rb|php|swift|dart|lua|r|pl|sql)$/, 'code', '#cc5a3d'],
  [/^(sh|bash|zsh|fish|ps1|psm1|bat|cmd)$/, 'term', '#2e9d5b'],
  [/^(json|jsonc|json5)$/, 'json', '#c9a400'],
  [/^(md|markdown|mdx)$/, 'md', '#519aba'],
  [/^(yml|yaml|toml|ini|cfg|conf|env|properties|lock)$/, 'gear', '#8b929d'],
  [/^(csv|tsv|xlsx|xls)$/, 'table', '#1f9d55'],
  [/^pdf$/, 'pdf', '#d1453b'],
  [/^(png|jpg|jpeg|gif|webp|bmp|ico|svg|avif)$/, 'image', '#a074c4'],
];
function fileKind(name: string): [Glyph, string] {
  const lower = name.toLowerCase();
  if (lower === 'dockerfile' || lower === 'makefile') return ['gear', '#2496ed'];
  if (lower.startsWith('.env') || lower.startsWith('.git')) return ['gear', '#8b929d'];
  const ext = extOf(name);
  for (const [re, g, c] of KINDS) if (re.test(ext)) return [g, c];
  return ['doc', 'var(--text-3)'];
}

export function FileIcon({ name, size = 14 }: { name: string; size?: number }) {
  const [d, color] = fileGlyph(name);
  return (
    <svg
      className="wp-ficon"
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke={color}
      strokeWidth="1.4"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d={d} />
    </svg>
  );
}

/** Icon shape (16×16 path) and color for a file name; also used for file links in chat (chatFiles.ts) */
export function fileGlyph(name: string): [string, string] {
  const [g, color] = fileKind(name);
  return [GLYPH[g], color];
}

// ---------- One row of the file tree ----------

interface TreeRow {
  key: string;
  /** Absolute path */
  path: string;
  name: string;
  depth: number;
  isDir: boolean;
  expanded?: boolean;
  loading?: boolean;
}

/** Lists longer than this render only the rows in view (plus OVERSCAN above and below), like VS Code's list view:
 *  a repo with 10,000 changed files would otherwise put ~150,000 elements on the page */
const VIRTUAL_MIN = 150;
const OVERSCAN = 20;

function Rows({
  rows,
  scroller,
  selected,
  focused,
  git,
  onToggle,
  onOpen,
  onMenu,
  onDragRow,
  dropDir,
  onDropOver,
  stats,
  reviewBadges,
  reviewRelOf,
  reviewCleanCheck,
}: {
  rows: TreeRow[];
  /** The scrolling element the rows sit in; given, long lists are virtualized (rows all share one height) */
  scroller?: React.RefObject<HTMLElement | null>;
  /** Changes view: show lines added/removed to the right of file rows */
  stats?: boolean;
  selected?: string;
  /** Keyboard cursor (row key), separate from the opened file as in VS Code */
  focused?: string | null;
  git?: GitInfo | null;
  /** Start dragging a row (dropping into the input box inserts a reference) */
  onDragRow?: (e: React.DragEvent, r: TreeRow) => void;
  /** Directory currently being uploaded to (highlighted) */
  dropDir?: string | null;
  /** System files dragged over a row: a directory row means that directory, a file row means its parent directory */
  onDropOver?: (dir: string) => void;
  onToggle: (r: TreeRow) => void;
  onOpen: (r: TreeRow) => void;
  onMenu?: (e: React.MouseEvent, r: TreeRow) => void;
  reviewBadges?: Map<string, FileIssueBadge>;
  reviewRelOf?: (absPath: string) => string;
  reviewCleanCheck?: (relPath: string) => boolean;
}) {
  const box = useRef<HTMLDivElement>(null);
  const virtual = !!scroller && rows.length > VIRTUAL_MIN;
  // Row height comes from CSS (taller on phones), measured from a rendered row
  const [rowH, setRowH] = useState(26);
  const [win, setWin] = useState<[number, number]>([0, 2 * OVERSCAN]);
  /** Where the rows start inside the scroller (notices and totals may sit above them) */
  const offsetIn = (el: HTMLElement) => box.current!.getBoundingClientRect().top - el.getBoundingClientRect().top + el.scrollTop;
  useLayoutEffect(() => {
    const el = scroller?.current;
    if (!virtual || !el || !box.current) return;
    const update = () => {
      if (!box.current) return;
      const h = box.current.querySelector<HTMLElement>('.wp-row')?.offsetHeight || rowH;
      if (h !== rowH) return setRowH(h);
      const y = el.scrollTop - offsetIn(el);
      const from = Math.max(0, Math.floor(y / h) - OVERSCAN);
      const to = Math.min(rows.length, Math.ceil((y + el.clientHeight) / h) + OVERSCAN);
      setWin((w) => (w[0] === from && w[1] === to ? w : [from, to]));
    };
    update();
    el.addEventListener('scroll', update, { passive: true });
    const ro = new ResizeObserver(update);
    ro.observe(el);
    return () => {
      el.removeEventListener('scroll', update);
      ro.disconnect();
    };
  }, [virtual, rows.length, rowH, scroller]); // eslint-disable-line react-hooks/exhaustive-deps
  // The keyboard cursor may move to a row that isn't rendered: scroll it into view by position
  useEffect(() => {
    const el = scroller?.current;
    if (!virtual || !el || !box.current || focused == null) return;
    const i = rows.findIndex((r) => r.key === focused);
    if (i < 0) return;
    const top = offsetIn(el) + i * rowH;
    if (top < el.scrollTop) el.scrollTop = top;
    else if (top + rowH > el.scrollTop + el.clientHeight) el.scrollTop = top + rowH - el.clientHeight;
  }, [focused]); // eslint-disable-line react-hooks/exhaustive-deps
  const from = virtual ? Math.min(win[0], rows.length) : 0;
  const to = virtual ? Math.min(Math.max(win[1], from), rows.length) : rows.length;
  return (
    <div ref={box} style={virtual ? { paddingTop: from * rowH, paddingBottom: (rows.length - to) * rowH } : undefined}>
      {rows.slice(from, to).map((r) => {
        const g = git && !r.isDir ? git.files.get(norm(r.path)) : undefined;
        const mark = g ? gitMark(g.status) : null;
        const dirChanged = !!git && r.isDir && !r.expanded && git.dirs.has(norm(r.path));
        const ignored = !r.loading && !mark && !dirChanged && isGitIgnored(git, r.path);
        const revRel = !r.isDir && reviewRelOf ? reviewRelOf(r.path) : '';
        const revBadge = revRel && reviewBadges ? reviewBadges.get(revRel) : undefined;
        const revTopSev = revBadge ? (revBadge.high > 0 ? 'high' : revBadge.medium > 0 ? 'medium' : 'low') : null;
        const revClean = !!revRel && !revBadge && !!reviewCleanCheck?.(revRel);
        return (
        <div
          key={r.key}
          role="treeitem"
          aria-expanded={r.isDir ? !!r.expanded : undefined}
          aria-selected={selected === r.path}
          data-path={r.path}
          data-key={r.key}
          data-dir={r.isDir ? '1' : undefined}
          className={`wp-row ${selected === r.path ? 'sel' : ''} ${focused === r.key ? 'kfocus' : ''} ${mark ? mark[1] : ignored ? 'g-ign' : ''} ${dirChanged ? 'g-dir' : ''} ${
            dropDir && r.isDir && r.path === dropDir ? 'drop' : ''
          }`}
          draggable={!r.loading && !!onDragRow}
          onDragStart={onDragRow ? (e) => onDragRow(e, r) : undefined}
          onDragOver={
            onDropOver
              ? (e) => {
                  if (!e.dataTransfer.types.includes('Files')) return;
                  e.preventDefault();
                  e.stopPropagation();
                  onDropOver(r.isDir ? r.path : parentOf(r.path));
                }
              : undefined
          }
          style={{ paddingLeft: 8 + r.depth * 14 }}
          title={mark ? `${r.path}\n${gitText(mark[0])}` : ignored ? `${r.path}\n${tx('ignored')}` : r.path}
          onClick={() => (r.isDir ? onToggle(r) : onOpen(r))}
          onContextMenu={onMenu ? (e) => (e.stopPropagation(), onMenu(e, r)) : undefined}
        >
          {/* Same as VS Code's default icon theme: file icons sit in the chevron column, so directory and file names at the same level line up on the left */}
          <span className="wp-twist">
            {r.isDir ? (
              <span className={`chev ${r.expanded ? 'open' : ''}`}>
                <Icon name="chevron" size={13} />
              </span>
            ) : (
              <FileIcon name={r.name} />
            )}
          </span>
          <span className="truncate wp-name">{r.name}</span>
          {/* Touch screens only (see .wp-row-more); before the counts and letter so those stay right-aligned */}
          {onMenu && !r.loading && (
            <button
              className="wp-row-more"
              title={tx('moreActions')}
              onClick={(e) => {
                e.stopPropagation();
                const rect = e.currentTarget.getBoundingClientRect();
                onMenu({ clientX: rect.right - 180, clientY: rect.bottom + 2, preventDefault: () => {} } as unknown as React.MouseEvent, r);
              }}
            >
              <Icon name="more" size={13} />
            </button>
          )}
          {revBadge && revBadge.total > 0 && revTopSev ? (
            <span
              className={`wp-row-rev-badge sev-${revTopSev} ${revBadge.open === 0 ? 'done' : ''}`}
              title={tx('reviewFoundCount', { n: revBadge.total })}
            >
              {revBadge.total}
            </span>
          ) : revClean ? (
            <span className="wp-row-rev-clean" title={tx('reviewFileCleanTip')}>
              <Icon name="check" size={11} />
            </span>
          ) : null}
          {stats && g && <LineStat a={g.added} d={g.deleted} />}
          {mark && <span className="wp-git">{mark[0]}</span>}
          {dirChanged && <span className="wp-git-dot" />}
        </div>
        );
      })}
    </div>
  );
}

/** Flatten into rows following kids */
function flatten(dir: string, depth: number, kids: Record<string, DirEntry[]>, expanded: Set<string>, out: TreeRow[] = []) {
  for (const e of kids[dir] ?? []) {
    const open = e.isDir && expanded.has(e.path);
    out.push({ key: e.path, path: e.path, name: e.name, depth, isDir: e.isDir, expanded: open });
    if (open) {
      if (kids[e.path]) flatten(e.path, depth + 1, kids, expanded, out);
      else out.push({ key: e.path + '/…', path: '', name: tx('loading'), depth: depth + 1, isDir: false, loading: true });
    }
  }
  return out;
}

interface SearchNode {
  dirs: Map<string, SearchNode>;
  files: string[];
}

/** Arrange search results (relative paths) into a tree: directories with a single subdirectory are merged into a/b/c */
function searchRows(root: string, paths: string[], collapsed: Set<string>): TreeRow[] {
  const top: SearchNode = { dirs: new Map(), files: [] };
  for (const p of paths) {
    const parts = p.split('/');
    let n = top;
    for (const d of parts.slice(0, -1)) {
      if (!n.dirs.has(d)) n.dirs.set(d, { dirs: new Map(), files: [] });
      n = n.dirs.get(d)!;
    }
    n.files.push(parts[parts.length - 1]);
  }
  const out: TreeRow[] = [];
  const walk = (n: SearchNode, rel: string, depth: number) => {
    for (const [name0, child0] of [...n.dirs.entries()].sort((a, b) => a[0].localeCompare(b[0]))) {
      let name = name0;
      let child = child0;
      while (child.files.length === 0 && child.dirs.size === 1) {
        const [k, v] = [...child.dirs.entries()][0];
        name += '/' + k;
        child = v;
      }
      const r = rel ? `${rel}/${name}` : name;
      const open = !collapsed.has(r);
      out.push({ key: 'd:' + r, path: join(root, r), name, depth, isDir: true, expanded: open });
      if (open) walk(child, r, depth + 1);
    }
    for (const f of [...n.files].sort()) {
      const r = rel ? `${rel}/${f}` : f;
      out.push({ key: 'f:' + r, path: join(root, r), name: f, depth, isDir: false });
    }
  };
  walk(top, '', 0);
  return out;
}

const SEARCH_LIMIT = 200;

/** Last git status per host + directory (like VS Code's Source Control view): reopening the panel or switching back to a
 *  session shows it at once and refreshes in the background instead of starting from "Loading changes…".
 *  git: null = not a repo. In memory only */
const gitCache = new Map<string, { git: GitInfo | null; sig: string; ignSig: string }>();
const gitKey = (hostId: string, cwd: string) => `${hostId}\n${cwd}`;
/** Root path and top-level listing per host + directory + hidden-files setting. The Changes list and the tree are
 *  both laid out against the root, so without it a remounted panel stays blank until fs.list returns. In memory only */
const rootCache = new Map<string, { path: string; entries: DirEntry[] }>();
const rootKey = (hostId: string, cwd: string, hidden: boolean) => `${hostId}\n${cwd}\n${hidden ? 1 : 0}`;
/** Known worktree paths per (hostId + mainRepo) across panel remounts, so only genuinely newly created worktrees trigger the switch banner */
const knownWorktreesByRepo = new Map<string, Set<string>>();

interface BranchDiffInfo {
  repo: string;
  branch: string;
  base: string | null;
  mergeBase: string | null;
  total?: number | null;
  files: Map<string, GitFile>;
}

export function FilePanel({
  hostId,
  cwd,
  baseCwd,
  onSwitchCwd,
  sessionId,
  status,
  fullscreen,
  onFullscreen,
  onClose,
  onFileOpen,
  standalone,
}: {
  hostId: string;
  cwd: string;
  /** The session's original working directory (differs from cwd when browsing another git worktree) */
  baseCwd?: string;
  /** Switch the file panel to another git worktree directory of the same repository */
  onSwitchCwd?: (nextPath: string) => void;
  sessionId: string;
  /** Session status: refresh the tree and open files when a turn ends */
  status: string;
  fullscreen: boolean;
  onFullscreen: () => void;
  onClose: () => void;
  /** Notify the parent when files are opened / closed: the panel width differs (narrow with only the tree) */
  onFileOpen?: (open: boolean) => void;
  /** Files session: the panel is the whole page (no chat), can't be closed or made full screen, and tabs are kept per session */
  standalone?: boolean;
}) {
  const isOtherWorktree = !!baseCwd && norm(cwd) !== norm(baseCwd);
  const [, lang] = useLang();
  const [hidden, setHidden] = useState(() => lsGet('wp-hidden', true));
  const [root, setRoot] = useState<string | null>(() => rootCache.get(rootKey(hostId, cwd, hidden))?.path ?? null);
  const [kids, setKids] = useState<Record<string, DirEntry[]>>(() => {
    const c = rootCache.get(rootKey(hostId, cwd, hidden));
    return c ? { [c.path]: c.entries } : {};
  });
  // Expanded folders are remembered per host + directory (VS Code keeps them in workspace storage)
  const expKey = `wp-exp-${hostId}/${cwd}`;
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set(lsGet<string[]>(expKey, [])));
  const expandedRef = useRef(expanded);
  expandedRef.current = expanded;
  const expLoaded = useRef(expKey);
  const [file, setFile] = useState<OpenFile | null>(null);
  const [saveState, setSaveState] = useState<SaveState>(null);
  const [saveError, setSaveError] = useState('');
  const [wrap, setWrap] = useState(() => lsGet('wp-wrap', false));
  const [preview, setPreview] = useState(false);
  // Markdown split view: edit on the left, preview on the right, scrolling kept in sync; editorAt / previewAt is the line the other side should scroll to
  const [mdSplit, setMdSplit] = useState(false);
  const [editorAt, setEditorAt] = useState<{ line: number; frac: number; n: number } | null>(null);
  const [previewAt, setPreviewAt] = useState<{ line: number; frac: number; n: number } | null>(null);
  const [mdGoto, setMdGoto] = useState<{ line: number; n: number } | null>(null);
  // Which side drives the scrolling in split view: follow the side the mouse / finger is on, so the two don't trigger each other
  const syncFrom = useRef<'editor' | 'preview'>('editor');
  const sessionKey = skey(hostId, sessionId);
  const treeOpenKey = `wp-tree-${sessionKey}`;
  const viewKey = `wp-view-${sessionKey}`;
  const [treeOpen, setTreeOpen] = useState(() => lsGet(treeOpenKey, true));
  const setTree = useCallback(
    (open: boolean) => {
      lsSet(treeOpenKey, open);
      setTreeOpen(open);
    },
    [treeOpenKey],
  );
  const [treeW, setTreeW] = useState(() => lsGet('wp-tree-w', TREE_DEFAULT));
  const bodyRef = useRef<HTMLDivElement>(null);
  const sideRef = useRef<HTMLDivElement>(null);
  const dragCleanupRef = useRef<(() => void) | null>(null);
  useEffect(() => () => dragCleanupRef.current?.(), []);
  const [filter, setFilter] = useState('');
  const [search, setSearch] = useState<{ q: string; paths: string[]; truncated: boolean; error?: boolean } | null>(null);
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  // Opened files (tabs); the current file is file
  const [tabs, setTabs] = useState<string[]>([]);
  // Native webviews (NativeWeb.tsx) stay parked while their tab is open, so switching tabs doesn't reload them
  const panelId = useRef(`${Math.random()}`).current;
  useLayoutEffect(() => registerWebTabs(panelId, tabs.filter(isWebTab).map((t) => `${hostId}|${t}`)), [tabs, hostId]); // eslint-disable-line react-hooks/exhaustive-deps
  // Later than the webviews' own unmount, which parks them (closing the panel, switching sessions keeps them)
  useEffect(() => () => void setTimeout(() => registerWebTabs(panelId, null), 0), []); // eslint-disable-line react-hooks/exhaustive-deps
  const [git, setGit] = useState<GitInfo | null>(() => gitCache.get(gitKey(hostId, cwd))?.git ?? null);
  const [worktrees, setWorktrees] = useState<WorktreeEntry[]>([]);
  const [newWorktree, setNewWorktree] = useState<WorktreeEntry | null>(null);
  const rawHostSessions = useStore((st) => st.sessions[hostId]);
  const hostSessions = useMemo(() => rawHostSessions ?? [], [rawHostSessions]);
  const sessInfo = useMemo(() => hostSessions.find((x) => x.id === sessionId), [hostSessions, sessionId]);
  const canSideChat = !standalone && !!sessInfo && sessInfo.mode === 'structured' && sessInfo.tool !== 'shell' && sessInfo.tool !== 'files' && !sessInfo.sideChat;
  const turnCheckpoints = useMemo(
    () => (sessInfo?.checkpoints ?? []).filter((c) => c.filesChanged > 0 && !!c.endRef),
    [sessInfo?.checkpoints],
  );
  const [diffScope, setDiffScope] = useState<'uncommitted' | 'branch' | 'turn'>('uncommitted');
  const [selectedTurn, setSelectedTurn] = useState<number | null>(null);
  const activeTurnCkpt = useMemo(() => {
    if (!turnCheckpoints.length) return null;
    if (selectedTurn != null) {
      const exact = turnCheckpoints.find((c) => c.turn === selectedTurn);
      if (exact) return exact;
    }
    return turnCheckpoints[turnCheckpoints.length - 1];
  }, [turnCheckpoints, selectedTurn]);
  const [turnDiff, setTurnDiff] = useState<{ baseRef: string; endRef: string; total?: number | null; files: Map<string, GitFile> } | null>(null);
  const [turnDiffLoading, setTurnDiffLoading] = useState(false);
  const [branchBase, setBranchBase] = useState(() => sessInfo?.worktree?.baseBranch ?? '');
  useEffect(() => {
    setBranchBase(sessInfo?.worktree?.baseBranch ?? '');
  }, [sessInfo?.worktree?.branch, sessInfo?.worktree?.baseBranch]);
  const [branchDiff, setBranchDiff] = useState<BranchDiffInfo | null>(null);
  const [branchDiffLoading, setBranchDiffLoading] = useState(false);
  const [branchesInfo, setBranchesInfo] = useState<BranchesResult | null>(null);
  const [branchesLoading, setBranchesLoading] = useState(false);
  const [bwOpen, setBwOpen] = useState(false);
  const bwBtnRef = useRef<HTMLButtonElement>(null);
  const [commitOpen, setCommitOpen] = useState(false);
  const [reviewPreset, setReviewPreset] = useState<ReviewLaunchPreset | null>(null);
  const [pushing, setPushing] = useState(false);
  const [view, setView] = useState<'all' | 'changes' | 'search'>(() => lsGet(viewKey, 'all'));
  const [showDiff, setShowDiff] = useState(false);
  // Default to Cursor-style single-file inline diff with the full file expanded
  const [diffLayout, setDiffLayout] = useState<'split' | 'unified'>(() => lsGet('wp-diff-mode', 'unified'));
  const [diffCollapse, setDiffCollapse] = useState(() => lsGet('wp-diff-fold', false));
  const mergeRef = useRef<MergeDiffHandle>(null);
  // Make the editor jump to a line (search results, outline, : line number, context pills)
  const [goto, setGoto] = useState<GotoTarget | null>(null);
  const [quick, setQuick] = useState<QuickMode | null>(null);
  // Another file opened side by side on the right (read-only); compare diffs the current file against another
  const [second, setSecond] = useState<OpenFile | null>(null);
  const [compare, setCompare] = useState<{ path: string; text: string; replace?: boolean } | null>(null);
  // "Find in folder" from the tree: the search view fills "files to include" with it
  const [grepInclude, setGrepInclude] = useState<{ value: string; n: number } | null>(null);
  // Commit file tab: the title toggles the full commit details (there's no hover tooltip on phones)
  const [commitInfo, setCommitInfo] = useState(false);
  const [dropDir, setDropDir] = useState<string | null>(null);
  const [uploading, setUploading] = useState(0);
  const isLocal = useStore((st) => st.hosts.find((h) => h.config.id === hostId)?.config.kind === 'local');
  const connected = useStore((st) => st.hosts.find((h) => h.config.id === hostId)?.status.status === 'connected');
  useReviewStoreVersion();
  const [reviewOnlyFilter, setReviewOnlyFilter] = useState(false);
  const [showInlineReviews, setShowInlineReviews] = useState(true);
  const [highlightedFindingId, setHighlightedFindingId] = useState<string | null>(null);
  const [diffIssueCursor, setDiffIssueCursor] = useState(0);

  const reviewSessions = useMemo(
    () =>
      standalone
        ? []
        : hostSessions
            .filter((x) => x.parentId === sessionId && (!!x.review || !!getReviewMeta(x.id)))
            .sort((a, b) => (b.updatedAt ?? b.createdAt) - (a.updatedAt ?? a.createdAt)),
    [standalone, hostSessions, sessionId],
  );
  const storedActiveReviewId = getActiveReviewId(hostId, sessionId);
  const activeReviewInfo = useMemo(
    () => (storedActiveReviewId ? reviewSessions.find((s) => s.id === storedActiveReviewId) : undefined) ?? reviewSessions[0],
    [reviewSessions, storedActiveReviewId],
  );
  const activeReviewId = activeReviewInfo?.id ?? null;
  const activeReviewData = useSessionData(activeReviewId ? skey(hostId, activeReviewId) : '');
  const activeReviewMeta = resolveReviewMeta(activeReviewInfo, activeReviewId ? getReviewMeta(activeReviewId) : null);
  const activeReviewBusy = activeReviewInfo?.status === 'running' || activeReviewInfo?.status === 'starting';

  useEffect(() => {
    if (connected && activeReviewId && !activeReviewData.loaded) {
      loadSession(hostId, activeReviewId).catch(() => {});
    }
  }, [connected, hostId, activeReviewId, activeReviewData.loaded]);

  useEffect(() => {
    if (!activeReviewId) setReviewOnlyFilter(false);
  }, [activeReviewId]);

  useEffect(() => {
    let timer: number | undefined;
    const unsub = subscribeFocusFinding((p) => {
      if (p.hostId !== hostId || p.parentSessionId !== sessionId) return;
      setShowInlineReviews(true);
      setHighlightedFindingId(p.findingId);
      window.clearTimeout(timer);
      timer = window.setTimeout(() => setHighlightedFindingId((cur) => (cur === p.findingId ? null : cur)), 3200);
    });
    return () => {
      window.clearTimeout(timer);
      unsub();
    };
  }, [hostId, sessionId]);

  const sdata = useSessionData(skey(hostId, sessionId));
  // Files changed by tools in this session: norm(absolute path) -> each edit made to that file
  const touched = useMemo(() => {
    const m = new Map<string, FileDiff[]>();
    for (const e of sdata.events) {
      if (e.kind !== 'tool_call' || !e.diffs?.length) continue;
      for (const d of e.diffs) {
        const abs = /^([a-zA-Z]:)?[\\/]/.test(d.path) ? d.path : join(root ?? cwd, d.path);
        const k = norm(abs);
        const list = m.get(k);
        if (list) list.push(d);
        else m.set(k, [d]);
      }
    }
    return m;
  }, [sdata.events, sdata.events.length, root, cwd]);

  const dirty = !!file && file.draft !== file.content;
  const fileRef = useRef(file);
  fileRef.current = file;

  /** quiet: restoring remembered folders; one that no longer exists is just forgotten */
  const load = useCallback(
    async (dir: string, quiet = false) => {
      try {
        const r = await rpc<{ path: string; entries: DirEntry[] }>(hostId, 'fs.list', { path: dir, showHidden: hidden });
        setKids((k) => ({ ...k, [dir]: r.entries }));
        return r.path;
      } catch (e) {
        if (quiet) {
          setExpanded((x) => {
            if (!x.has(dir)) return x;
            const n = new Set(x);
            n.delete(dir);
            return n;
          });
        } else errorToast(e);
        return null;
      }
    },
    [hostId, hidden],
  );

  // Root directory: fs.list normalizes ~ and relative forms into absolute paths
  useEffect(() => {
    const key = rootKey(hostId, cwd, hidden);
    const c = rootCache.get(key);
    setKids(c ? { [c.path]: c.entries } : {});
    if (c) setRoot(c.path);
    // Switched to another directory without remounting: take that directory's remembered folders
    let want = expandedRef.current;
    if (expLoaded.current !== expKey) {
      expLoaded.current = expKey;
      want = new Set(lsGet<string[]>(expKey, []));
      setExpanded(want);
    }
    rpc<{ path: string; entries: DirEntry[] }>(hostId, 'fs.list', { path: cwd, showHidden: hidden })
      .then((r) => {
        rootCache.set(key, { path: r.path, entries: r.entries });
        setRoot(r.path);
        setKids((k) => ({ ...k, [r.path]: r.entries }));
        // Reload the remembered folders (also after toggling hidden files, which reset the listings above)
        const pre = norm(r.path) + '/';
        const stale = [...want].filter((d) => !norm(d).startsWith(pre));
        for (const d of want) if (!stale.includes(d)) load(d, true);
        if (stale.length) setExpanded((x) => new Set([...x].filter((d) => !stale.includes(d))));
      })
      .catch(errorToast);
  }, [hostId, cwd, hidden]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    if (expLoaded.current === expKey) lsSet(expKey, [...expanded].slice(-300));
  }, [expanded, expKey]);

  // Until git status comes back we don't know whether this is a repo, so the snippets recorded in the session can't be shown as a diff yet
  const [gitReady, setGitReady] = useState(() => gitCache.has(gitKey(hostId, cwd)));
  // Diff of a single file (git.changes only lists statuses; the diff is fetched when the file is opened), keyed by norm(absolute path)
  const [fileDiffs, setFileDiffs] = useState<Record<string, string | null>>({});
  // Whole files at a revision for the full-file diff (git.blob), keyed by `rev\nrel`; null = couldn't be read
  const [blobs, setBlobs] = useState<Record<string, GitBlob | null>>({});
  // Blobs from before the last status change, shown until their refetch lands: clearing them would swap the open diff
  // for the loading state, remounting MergeDiff (losing the divider position and scroll) every time the agent edits
  const staleBlobs = useRef<{ key: string; blobs: Record<string, GitBlob | null> }>({ key: '', blobs: {} });
  // Summary of the last git status: periodic refreshes leave the UI alone if nothing changed, so an open diff doesn't flicker
  const gitSig = useRef(gitCache.get(gitKey(hostId, cwd))?.sig ?? '');
  const gitIgnSig = useRef(gitCache.get(gitKey(hostId, cwd))?.ignSig ?? '');
  /** quiet: background refresh (timer / window refocus); if the status didn't change, just quietly refetch the current file's diff */
  const loadWorktrees = useCallback(() => {
    rpc<WorktreesResult>(hostId, 'git.worktrees', { cwd })
      .then((r) => {
        const list = (r.worktrees ?? []).filter((w) => !w.bare);
        setWorktrees(list);
        if (!r.repo) return;
        const rk = `${hostId}\n${norm(r.repo)}`;
        const known = knownWorktreesByRepo.get(rk);
        const curKeys = new Set(list.map((w) => norm(w.path)));
        if (!known) {
          knownWorktreesByRepo.set(rk, curKeys);
        } else {
          const added = list.find((w) => !w.isCurrent && !known.has(norm(w.path)));
          for (const k of curKeys) known.add(k);
          if (added && onSwitchCwd) setNewWorktree(added);
        }
      })
      .catch(() => {});
  }, [hostId, cwd, onSwitchCwd]);

  const loadBranchDiff = useCallback(
    (baseOverride?: string) => {
      const b = baseOverride !== undefined ? baseOverride : branchBase;
      setBranchDiffLoading(true);
      rpc<{
        repo: string | null;
        branch?: string;
        base?: string | null;
        mergeBase?: string | null;
        total?: number | null;
        files: { path: string; status: string; diff: string; added?: number | null; deleted?: number | null }[];
      }>(hostId, 'git.branchDiff', { cwd, base: b || undefined, diff: false })
        .then((r) => {
          setBranchDiffLoading(false);
          if (!r.repo) {
            setBranchDiff(null);
            return;
          }
          const files = new Map<string, GitFile>();
          const sep = sepOf(r.repo);
          for (const f of r.files) {
            const abs = join(r.repo, f.path.split('/').join(sep));
            files.set(norm(abs), { ...f, path: abs, rel: f.path });
          }
          setBranchDiff({
            repo: r.repo,
            branch: r.branch ?? '',
            base: r.base ?? null,
            mergeBase: r.mergeBase ?? null,
            total: r.total,
            files,
          });
        })
        .catch(() => setBranchDiffLoading(false));
    },
    [hostId, cwd, branchBase],
  );

  const loadTurnDiff = useCallback(
    (baseRef: string, endRef: string, paths?: string[]) => {
      setTurnDiffLoading(true);
      rpc<{
        repo: string | null;
        baseRef?: string;
        endRef?: string;
        total?: number | null;
        files: { path: string; status: string; diff: string; added?: number | null; deleted?: number | null }[];
      }>(hostId, 'git.refDiff', { cwd, baseRef, endRef, diff: false, ...(paths?.length ? { paths } : {}) })
        .then((r) => {
          setTurnDiffLoading(false);
          if (!r.repo) {
            setTurnDiff(null);
            return;
          }
          const files = new Map<string, GitFile>();
          const sep = sepOf(r.repo);
          for (const f of r.files) {
            const abs = join(r.repo, f.path.split('/').join(sep));
            files.set(norm(abs), { ...f, path: abs, rel: f.path });
          }
          setTurnDiff({ baseRef, endRef, total: r.total, files });
        })
        .catch(() => setTurnDiffLoading(false));
    },
    [hostId, cwd],
  );

  const loadBranches = useCallback(
    () =>
      rpc<BranchesResult>(hostId, 'git.branches', { cwd })
        .then((r) => {
          setBranchesInfo(r);
          return r;
        })
        .catch(() => null),
    [hostId, cwd],
  );

  const loadGit = useCallback((quiet = false) => {
    // Without diffs: in a big repo with many changed files, running git diff for each takes ages, so fetch only the file that gets opened
    rpc<{
      repo: string | null;
      branch?: string;
      head?: string | null;
      total?: number | null;
      files: { path: string; status: string; diff: string; added?: number | null; deleted?: number | null }[];
      ignored?: string[];
    }>(hostId, 'git.changes', { cwd, diff: false })
      .then((r) => {
        setGitReady(true);
        if (r.repo) loadWorktrees();
        const rawIgnored = r.ignored ?? [];
        const sig = `${r.repo}|${r.branch}|${r.head}|${r.files.map((f) => `${f.status} ${f.path} ${f.added ?? ''}/${f.deleted ?? ''}`).join('|')}`;
        const ignSig = rawIgnored.join('\0');
        const filesChanged = !quiet || sig !== gitSig.current;
        const ignChanged = !quiet || ignSig !== gitIgnSig.current;
        if (!filesChanged && !ignChanged) return refreshCurDiff.current();
        gitSig.current = sig;
        gitIgnSig.current = ignSig;
        if (filesChanged) {
          setFileDiffs({});
          setBlobs((m) => {
            const key = gitKey(hostId, cwd);
            staleBlobs.current = { key, blobs: staleBlobs.current.key === key ? { ...staleBlobs.current.blobs, ...m } : m };
            return {};
          });
        }
        if (!r.repo) {
          gitCache.set(gitKey(hostId, cwd), { git: null, sig, ignSig });
          setWorktrees([]);
          setBranchDiff(null);
          return setGit(null);
        }
        const files = new Map<string, GitFile>();
        const dirs = new Set<string>();
        for (const f of r.files) {
          const abs = join(r.repo, f.path.split('/').join(sepOf(r.repo)));
          const k = norm(abs);
          files.set(k, { ...f, path: abs, rel: f.path });
          for (let d = k.slice(0, k.lastIndexOf('/')); d.length > 1; d = d.slice(0, d.lastIndexOf('/'))) {
            if (dirs.has(d)) break;
            dirs.add(d);
          }
        }
        const ignoredFiles = new Set<string>();
        const ignoredDirs = new Set<string>([norm(join(r.repo, '.git'))]);
        const sep = sepOf(r.repo);
        for (const p of rawIgnored) {
          const isDir = p.endsWith('/');
          const relPath = isDir ? p.slice(0, -1) : p;
          const k = norm(join(r.repo, relPath.split('/').join(sep)));
          if (isDir) ignoredDirs.add(k);
          else ignoredFiles.add(k);
        }
        const info: GitInfo = { repo: r.repo, branch: r.branch ?? '', head: r.head, total: r.total, files, dirs, ignoredFiles, ignoredDirs };
        gitCache.set(gitKey(hostId, cwd), { git: info, sig, ignSig });
        setGit(info);
        if (!filesChanged) refreshCurDiff.current();
      })
      .catch(() => (setGitReady(true), quiet || setGit(null)));
  }, [hostId, cwd, loadWorktrees]);
  const refreshCurDiff = useRef(() => {});
  // With a cached status this is a background refresh: nothing changes on screen unless the status did
  useEffect(() => loadGit(gitCache.has(gitKey(hostId, cwd))), [loadGit]);
  useEffect(() => {
    if (git && (diffScope === 'branch' || reviewPreset !== null)) loadBranchDiff();
  }, [git, diffScope, branchBase, reviewPreset, loadBranchDiff]);
  useEffect(() => {
    if (diffScope === 'turn') {
      if (!activeTurnCkpt?.endRef) {
        setDiffScope('uncommitted');
        setTurnDiff(null);
      } else if (git) {
        loadTurnDiff(activeTurnCkpt.baseRef, activeTurnCkpt.endRef, activeTurnCkpt.paths);
      }
    }
  }, [git, diffScope, activeTurnCkpt?.baseRef, activeTurnCkpt?.endRef, activeTurnCkpt?.paths, loadTurnDiff]);
  useEffect(() => {
    if (git && (commitOpen || reviewPreset !== null || view === 'changes')) void loadBranches();
  }, [git?.branch, commitOpen, reviewPreset, view, loadBranches]); // eslint-disable-line react-hooks/exhaustive-deps
  // Commits made by the AI in a terminal or files changed by other programs aren't caught by the end-of-turn refresh: check again when the window regains focus and every 10 s (while visible)
  useEffect(() => {
    const tick = () => document.visibilityState === 'visible' && loadGit(true);
    const t = setInterval(tick, 10000);
    window.addEventListener('focus', tick);
    return () => {
      clearInterval(t);
      window.removeEventListener('focus', tick);
    };
  }, [loadGit]);

  const refreshTree = () => {
    if (!root) return;
    for (const d of [root, ...expanded]) load(d);
    loadGit();
  };

  // ---------- Filter: search the whole project 160 ms after typing stops ----------
  const q = filter.trim();
  useEffect(() => {
    if (!q || !root) {
      setSearch(null);
      return;
    }
    let alive = true;
    const t = setTimeout(() => {
      rpc<{ path: string; isDir: boolean }[]>(hostId, 'fs.search', { root, query: q, limit: SEARCH_LIMIT })
        .then((r) => {
          if (!alive) return;
          const files = r.filter((x) => !x.isDir).map((x) => x.path);
          // Prefer substring filtering; if nothing contains the text, fall back to fuzzy matching
          const ql = q.toLowerCase();
          const exact = files.filter((p) => p.toLowerCase().includes(ql));
          setSearch({ q, paths: exact.length ? exact : files, truncated: r.length >= SEARCH_LIMIT });
          setCollapsed(new Set());
        })
        .catch(() => alive && setSearch({ q, paths: [], truncated: false, error: true }));
    }, 160);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [q, root, hostId]);

  const toggle = (dir: string) => {
    const next = new Set(expanded);
    if (next.has(dir)) next.delete(dir);
    else {
      next.add(dir);
      if (!kids[dir]) load(dir);
    }
    setExpanded(next);
  };

  // ---------- Opening and saving files ----------

  const readFile = async (path: string, quiet = false): Promise<OpenFile | null> => {
    if (isSideTab(path)) return { path, content: '', draft: '', binary: true, truncated: false, size: 0, modified: 0, rev: Date.now(), side: true };
    if (isReviewTab(path)) return { path, content: '', draft: '', binary: true, truncated: false, size: 0, modified: 0, rev: Date.now(), review: true };
    if (isWebTab(path)) return { path, content: '', draft: '', binary: true, truncated: false, size: 0, modified: 0, rev: Date.now(), web: true };
    if (isUntitled(path))
      return { path, content: '', draft: untitled.get(path) ?? '', binary: false, truncated: false, size: 0, modified: 0, rev: Date.now(), untitled: true };
    const ct = parseCommitTab(path);
    if (ct) {
      try {
        if (ct.baseHash) {
          const r = await rpc<{ files: { path: string; status: string; diff: string }[] }>(hostId, 'git.reviewScope', {
            cwd,
            mode: 'range',
            range: `${ct.baseHash}..${ct.hash}`,
            diff: true,
            paths: [ct.rel],
          });
          const f = r.files.find((x) => x.path === ct.rel);
          const rangeLabel = `${ct.baseHash.slice(0, 7)}..${ct.hash.slice(0, 7)}`;
          const commit = { hash: ct.hash, baseHash: ct.baseHash, rel: ct.rel, subject: rangeLabel, message: '', author: '', time: 0, status: f?.status ?? '', diff: f?.diff ?? '' };
          return { path, content: '', draft: '', binary: true, truncated: false, size: 0, modified: 0, rev: Date.now(), commit };
        }
        const r = await rpc<{ hash: string; author: string; time: number; message: string; files: { path: string; status: string; diff: string }[] }>(hostId, 'git.show', {
          cwd,
          hash: ct.hash,
          paths: [ct.rel],
        });
        const f = r.files.find((x) => x.path === ct.rel);
        const commit = { hash: r.hash, rel: ct.rel, subject: r.message.split('\n')[0], message: r.message, author: r.author, time: r.time, status: f?.status ?? '', diff: f?.diff ?? '' };
        return { path, content: '', draft: '', binary: true, truncated: false, size: 0, modified: 0, rev: Date.now(), commit };
      } catch (e) {
        if (!quiet) errorToast(e);
        return null;
      }
    }
    const mime = IMAGE_MIME[extOf(path)];
    try {
      if (mime) {
        const r = await rpc<{ path: string; size: number; modified: number; base64: string }>(hostId, 'fs.read', { path, image: true });
        return { ...r, content: '', draft: '', binary: true, truncated: false, image: `data:${mime};base64,${r.base64}`, rev: Date.now() };
      }
      if (extOf(path) === 'pdf') {
        const r = await rpc<{ path: string; size: number; modified: number; base64: string }>(hostId, 'fs.read', { path, raw: true });
        return { ...r, content: '', draft: '', binary: true, truncated: false, pdf: r.base64, rev: Date.now() };
      }
      const r = await rpc<Omit<OpenFile, 'draft' | 'rev'>>(hostId, 'fs.read', { path });
      return { ...r, draft: r.content, rev: Date.now() };
    } catch (e) {
      if (!quiet) errorToast(e);
      return null;
    }
  };

  const confirmDiscard = async () =>
    !dirty ||
    (await askConfirm({
      title: tx('discardChangesTo', { v: nameOf(file!.path) }),
      message: tx('unsavedEditsWillBeDiscarded'),
      okText: tx('discard'),
      danger: true,
    }));

  const gitOf = (path: string) => git?.files.get(norm(path));
  /** Active diff map for the current Changes scope: uncommitted vs HEAD, branch vs merge-base, or per-turn checkpoint */
  const activeDiffFiles =
    diffScope === 'turn' && turnDiff
      ? turnDiff.files
      : diffScope === 'branch' && branchDiff
        ? branchDiff.files
        : git?.files;
  const diffGitOf = (path: string) => activeDiffFiles?.get(norm(path));
  const diffCacheKey = (path: string) =>
    `${
      diffScope === 'turn' && activeTurnCkpt?.endRef
        ? `${activeTurnCkpt.baseRef}..${activeTurnCkpt.endRef}`
        : diffScope === 'branch'
          ? branchDiff?.mergeBase ?? 'branch'
          : 'HEAD'
    }\n${norm(path)}`;
  /** Changes to this file: in a git repo, working tree vs HEAD, branch vs merge-base, or turn checkpoint; otherwise session tool edits */
  const diffOf = (path: string): FileDiff | null => {
    const g = diffGitOf(path);
    const d = g?.diff || fileDiffs[diffCacheKey(path)];
    if (g && d) return { path: g.rel, unified: d.slice(Math.max(0, d.indexOf('@@'))) };
    return null;
  };
  /** Outside a git repo, each edit tools made to this file in the session (each is a snippet, shown separately) */
  const sessionDiffsOf = (path: string): FileDiff[] => (gitReady && !git ? touched.get(norm(path)) ?? [] : []);
  /** Whether this file has changes to view (the diff may not have arrived yet) */
  const hasChange = (path: string) => !!diffGitOf(path) || sessionDiffsOf(path).length > 0;

  // Fetch the diff when a changed file is opened
  const curKey = file && !file.web && !file.commit && !file.review && !file.side ? norm(file.path) : '';
  const curDiffCacheKey = file && !file.web && !file.commit && !file.review && !file.side ? diffCacheKey(file.path) : '';
  const curGit = file ? diffGitOf(file.path) : undefined;
  useEffect(() => {
    if (!curKey || !curGit || curGit.diff || curDiffCacheKey in fileDiffs) return;
    const k = curDiffCacheKey;
    const req =
      diffScope === 'turn' && activeTurnCkpt?.endRef
        ? rpc<{ files: { path: string; diff: string }[] }>(hostId, 'git.refDiff', {
            cwd,
            baseRef: activeTurnCkpt.baseRef,
            endRef: activeTurnCkpt.endRef,
            paths: [curGit.rel],
          })
        : diffScope === 'branch'
          ? rpc<{ files: { path: string; diff: string }[] }>(hostId, 'git.branchDiff', {
              cwd,
              base: branchBase || undefined,
              paths: [curGit.rel],
            })
          : rpc<{ files: { path: string; diff: string }[] }>(hostId, 'git.changes', { cwd, paths: [curGit.rel] });
    req
      .then((r) => setFileDiffs((m) => ({ ...m, [k]: r.files.find((f) => f.path === curGit.rel)?.diff ?? '' })))
      .catch(() => setFileDiffs((m) => ({ ...m, [k]: '' })));
  }, [curKey, curDiffCacheKey, curGit, fileDiffs, diffScope, branchBase, activeTurnCkpt?.baseRef, activeTurnCkpt?.endRef]); // eslint-disable-line react-hooks/exhaustive-deps
  // Status unchanged but the content may have changed again: refetch the current file's diff in the background and swap it in when it arrives, without showing "loading"
  refreshCurDiff.current = () => {
    if (!curKey || !curGit || !(curDiffCacheKey in fileDiffs) || diffScope === 'turn') return;
    const k = curDiffCacheKey;
    const req =
      diffScope === 'branch'
        ? rpc<{ files: { path: string; diff: string }[] }>(hostId, 'git.branchDiff', {
            cwd,
            base: branchBase || undefined,
            paths: [curGit.rel],
          })
        : rpc<{ files: { path: string; diff: string }[] }>(hostId, 'git.changes', { cwd, paths: [curGit.rel] });
    req
      .then((r) => {
        const d = r.files.find((f) => f.path === curGit.rel)?.diff ?? '';
        setFileDiffs((m) => (m[k] === d ? m : { ...m, [k]: d }));
      })
      .catch(() => {});
  };
  // The file no longer has changes (e.g. just committed): if we're still on "Changes", switch back to the file
  useEffect(() => {
    if (showDiff && file && !file.deleted && !dirty && gitReady && !hasChange(file.path)) setShowDiff(false);
  }, [git, showDiff, file?.path, dirty]); // eslint-disable-line react-hooks/exhaustive-deps

  /** Whether to open the preview when opening a file: CSV defaults to the table; Markdown reuses the last choice of source / split / preview, and isn't preview-only when jumping to a line */
  const previewFor = (path: string, at?: unknown) => {
    if (isCsv(path)) return setPreview(!at), setMdSplit(false);
    if (isHtml(path)) return setPreview(false), setMdSplit(false);
    if (!isMarkdown(path)) return setPreview(false), setMdSplit(false);
    const m = lsGet<'source' | 'split' | 'preview'>('wp-md-mode', 'preview');
    setMdSplit(m === 'split' || (m === 'preview' && !!at && wide()));
    setPreview(m === 'preview' && !at);
  };
  const pickMdMode = (m: 'source' | 'split' | 'preview') => {
    lsSet('wp-md-mode', m);
    setMdSplit(m === 'split');
    setPreview(m === 'preview');
  };

  /** diff: files opened from the changes list show the diff by default; line: jump to this line after opening */
  const openFile = async (path0: string, diff = false, at?: { line: number; endLine?: number; col?: number; len?: number }) => {
    const path = unverbatim(path0);
    const jump = () => at && setGoto({ ...at, n: Date.now() });
    if (file?.path === path) {
      const canDiff = !!file.commit || !!file.deleted || hasChange(path);
      if (diff && canDiff) setShowDiff(true);
      if (at) {
        if (!diff || !canDiff) setShowDiff(false);
        if (preview) previewFor(path, at);
        setCompare(null);
        jump();
      }
      if (window.matchMedia('(max-width: 760px)').matches) setTreeOpen(false);
      return;
    }
    // An untitled tab keeps its text in memory when you switch away (it's written to untitled on every edit)
    if (!file?.untitled && !(await confirmDiscard())) return;
    const g = diffGitOf(path);
    const f: OpenFile | null =
      g && gitMark(g.status)[0] === 'D'
        ? { path, content: '', draft: '', binary: true, truncated: false, size: 0, modified: 0, rev: Date.now(), deleted: true }
        : (await readFile(path)) ??
          (diffScope === 'turn' && g
            ? { path, content: '', draft: '', binary: false, truncated: false, size: 0, modified: 0, rev: Date.now(), deleted: true }
            : null);
    if (!f) return;
    fileRef.current = f;
    setFile(f);
    setTabs((t) => (t.includes(path) ? t : [...t, path].slice(-30)));
    // The live picture / side chat needs the room: hide the tree for this session
    if (isAiTab(path) || isSideTab(path)) setTree(false);
    setSaveState(null);
    previewFor(path, at);
    setCompare(null);
    setGoto(at ? { ...at, n: Date.now() } : null);
    setShowDiff(!!f.deleted || (diff && hasChange(path)));
    if (window.matchMedia('(max-width: 760px)').matches) setTreeOpen(false);
  };
  // A file link or context pill in chat (SessionActions.openFileInPanel): taken on mount too, since a closed panel mounts after the event.
  // Through a ref, so the listener always calls the openFile of the latest render
  const openFileRef = useRef(openFile);
  openFileRef.current = openFile;
  /** Like VS Code's "Reveal in Explorer" for a folder: expand it and its parents in the tree, focus and scroll to its row.
   *  False when path isn't a folder (fs.list fails), so the caller opens it as a file */
  const revealDir = async (path: string): Promise<boolean> => {
    let r: { path: string; entries: DirEntry[] };
    try {
      r = await rpc<{ path: string; entries: DirEntry[] }>(hostId, 'fs.list', { path, showHidden: hidden });
    } catch {
      return false;
    }
    const dir = unverbatim(r.path);
    // A panel that just mounted may not have its root yet
    let top = root ?? rootCache.get(rootKey(hostId, cwd, hidden))?.path;
    if (!top) {
      try {
        top = (await rpc<{ path: string }>(hostId, 'fs.list', { path: cwd, showHidden: hidden })).path;
      } catch {
        return true;
      }
    }
    top = unverbatim(top);
    const nr = norm(top);
    const nd = norm(dir);
    if (nd !== nr && !nd.startsWith(nr + '/')) {
      toast(tx('folderIsOutsideTheWorkspace', { v: dir }));
      return true;
    }
    // The folder and its parents below the root, spelled the way fs.list spells tree paths (the root's separator)
    const chain: string[] = [];
    let cur = top;
    for (const part of dir.slice(top.length).split(/[\\/]/).filter(Boolean)) chain.push((cur = join(cur, part)));
    setKids((k) => ({ ...k, [chain[chain.length - 1] ?? top]: r.entries }));
    for (const d of chain.slice(0, -1)) if (!kids[d]) load(d, true);
    setExpanded((x) => new Set([...x, ...chain]));
    setFilter('');
    pickView('all');
    setTree(true);
    const target = chain[chain.length - 1];
    if (target) {
      setFocusKey(target);
      lastRow.current = { path: target, isDir: true };
      // Rows render after the parents load; the virtualized list scrolls by focus itself, short lists by element
      setTimeout(() => treeBox.current?.querySelector(`[data-key="${CSS.escape(target)}"]`)?.scrollIntoView({ block: 'center' }), 300);
    }
    return true;
  };
  const revealRef = useRef(revealDir);
  revealRef.current = revealDir;
  useEffect(() => {
    const take = async () => {
      const r = takeOpenFile(hostId, sessionId);
      if (!r) return;
      const isVirtual =
        isWebTab(r.path) || isAiTab(r.path) || isSideTab(r.path) || isReviewTab(r.path) || isUntitled(r.path) || isCommitTab(r.path);
      // A folder link: show it in the tree instead of trying to read it as a file
      if (!r.line && !r.diff && !isVirtual && (await revealRef.current(r.path))) return;
      openFileRef.current(r.path, !!r.diff, r.line ? { line: r.line, endLine: r.endLine } : undefined);
    };
    take();
    const on = (e: Event) => {
      const d = (e as CustomEvent<{ hostId: string; sessionId: string }>).detail;
      if (d.hostId === hostId && d.sessionId === sessionId) take();
    };
    window.addEventListener(OPEN_FILE, on);
    return () => window.removeEventListener(OPEN_FILE, on);
  }, [hostId, sessionId]);

  // ---------- When the tab bar overflows: scroll the current tab into view and add a dropdown on the right listing all tabs (like VS Code) ----------
  const tabsRef = useRef<HTMLDivElement>(null);
  const [tabsOverflow, setTabsOverflow] = useState(false);
  const [tabsMoreRight, setTabsMoreRight] = useState(false);
  const measureTabs = () => {
    const el = tabsRef.current;
    const ov = !!el && el.scrollWidth > el.clientWidth + 1;
    setTabsOverflow(ov);
    setTabsMoreRight(ov && el!.scrollLeft + el!.clientWidth < el!.scrollWidth - 1);
  };
  // Scroll the current tab into view: only change the tab bar's own scrollLeft (scrollIntoView would scroll outer containers too)
  const revealActiveTab = () => {
    const bar = tabsRef.current;
    const tab = bar?.querySelector<HTMLElement>('.wp-ftab.active');
    if (!bar || !tab) return;
    const b = bar.getBoundingClientRect();
    const r = tab.getBoundingClientRect();
    const left = r.left - b.left + bar.scrollLeft;
    const right = left + r.width;
    const pad = r.width + 16 <= bar.clientWidth ? 16 : 0;
    if (left < bar.scrollLeft) bar.scrollLeft = left;
    else if (right > bar.scrollLeft + bar.clientWidth - pad) {
      bar.scrollLeft = Math.min(bar.scrollWidth - bar.clientWidth, right - bar.clientWidth + pad);
    }
  };
  useEffect(() => {
    const el = tabsRef.current;
    if (!el) return;
    // When the panel narrows / is resized, keep the current file's tab visible
    const ro = new ResizeObserver(() => {
      revealActiveTab();
      measureTabs();
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [!!file]); // eslint-disable-line react-hooks/exhaustive-deps
  useLayoutEffect(() => {
    revealActiveTab();
    measureTabs();
  }, [file?.path, tabs.length]);
  const tabsMenu = (e: React.MouseEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    openMenu(
      { clientX: r.right - 260, clientY: r.bottom + 4 },
      [...tabs].reverse().map((t) => {
        const isRevSide = isSideTab(t) && !!(sideTabInfo(t)?.review || getReviewMeta(sideTabSession(t)));
        return {
          label: isSideTab(t) ? sideTabLabel(t) : isReviewTab(t) ? reviewTabLabel(t) : isWebTab(t) ? webTabTitle(t) : commitTabLabel(t) ?? nameOf(t),
          icon: isSideTab(t) ? (isRevSide ? 'sparkles' : 'sideChat') : isReviewTab(t) ? 'sparkles' : isWebTab(t) ? 'globe' : isCommitTab(t) ? 'diff' : 'file',
          hint: isSideTab(t) || isReviewTab(t) || isWebTab(t) || isCommitTab(t) || isUntitled(t) || rel(parentOf(t)) === '.' ? undefined : rel(parentOf(t)),
          active: t === file?.path,
          run: () => openFile(t),
        };
      }),
    );
  };

  /** Open a new browser tab; an empty url means a new tab page (lists ports on the host) */
  const openWeb = (url = '') => openFile(WEB_PREFIX + url);
  /** The browser tab changed its URL: replace the tab key in place */
  const navigateWeb = (from: string, to: string) => {
    setTabs((t) => {
      const rest = t.filter((x) => x !== to);
      return rest.includes(from) ? rest.map((x) => (x === from ? to : x)) : [...rest, to];
    });
    setFile((cur) => (cur && cur.path === from ? { ...cur, path: to, rev: Date.now() } : cur));
  };

  const cleanupReviewTab = (t: string) => {
    if (!isReviewTab(t)) return;
    const rsid = reviewTabSession(t);
    deleteReviewMeta(rsid);
    rpc(hostId, 'session.delete', { sessionId: rsid }).catch(() => {});
  };
  /** Closing an unused normal side chat deletes it automatically; review side chats and used side chats remain reopenable */
  const cleanupSideTab = (t: string) => {
    if (!isSideTab(t)) return;
    const ssid = sideTabSession(t);
    const s = hostSessions.find((x) => x.id === ssid);
    if (!s || s.review || getReviewMeta(ssid)) return;
    if (!sideChatHasMessages(hostId, ssid, s)) {
      rpc(hostId, 'session.delete', { sessionId: ssid }).catch(() => {});
    }
  };
  const sideTabInfo = (t: string) => hostSessions.find((x) => x.id === sideTabSession(t));
  const sideTabLabel = (t: string) => sideChatTitle(sideTabInfo(t));
  const reviewTabLabel = (t: string) => {
    const rsid = reviewTabSession(t);
    const meta = getReviewMeta(rsid);
    const label = meta ? TOOL_LABEL[meta.tool] : tx('reviewTabShort');
    return `${tx('reviewTabPrefix')} (${label})`;
  };

  const openPlusMenu = (e: React.MouseEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    const otherTools = canSideChat && sessInfo ? sideChatTools(hostId).filter((tk) => tk !== sessInfo.tool) : [];
    const items: (MenuItem | '-')[] = [
      ...(canSideChat && sessInfo
        ? [
            {
              label: tx('sideChatNew'),
              icon: 'sideChat',
              hint: TOOL_LABEL[sessInfo.tool],
              run: () => void startSideChat({ hostId, parent: sessInfo }).catch(errorToast),
            },
            ...otherTools.map((tk) => ({
              label: tx('sideChatNewWithTool', { tool: TOOL_LABEL[tk] }),
              icon: 'sideChat',
              run: () => void startSideChat({ hostId, parent: sessInfo, tool: tk }).catch(errorToast),
            })),
            '-' as const,
          ]
        : []),
      { label: tx('openWebPageNewBrowser'), icon: 'globe', run: () => openWeb() },
      ...(!file ? [{ label: tx('newUntitledFile'), icon: 'file', run: () => newUntitled() }] : []),
    ];
    openMenu({ clientX: r.right - 220, clientY: r.bottom + 4 }, items);
  };

  /** Close a tab: if it's the current file, switch to its neighbor; once all are closed, go back to tree only */
  const closeTab = async (path: string) => {
    const i = tabs.indexOf(path);
    const rest = tabs.filter((t) => t !== path);
    if (file?.path !== path) {
      if (!(await confirmLoseUntitled([path]))) return;
      untitled.delete(path);
      cleanupReviewTab(path);
      cleanupSideTab(path);
      return setTabs(rest);
    }
    if (!(await confirmDiscard())) return;
    untitled.delete(path);
    cleanupReviewTab(path);
    cleanupSideTab(path);
    setTabs(rest);
    const next = rest[Math.min(i, rest.length - 1)];
    if (next) {
      const f = await readFile(next);
      setFile(f);
      setShowDiff(false);
      previewFor(next);
    } else setFile(null);
    setCompare(null);
    setSaveState(null);
  };

  /** Keep only the tabs in keep; if the current file gets closed, switch to the right-clicked tab t (like VS Code) */
  const keepTabs = async (t: string, keep: string[]) => {
    const closing = tabs.filter((x) => !keep.includes(x));
    if (!(await confirmLoseUntitled(closing.filter((x) => x !== file?.path)))) return;
    const dropClosed = () =>
      closing.forEach((x) => {
        untitled.delete(x);
        cleanupReviewTab(x);
        cleanupSideTab(x);
      });
    if (!file || keep.includes(file.path)) return dropClosed(), setTabs(keep);
    if (!(await confirmDiscard())) return;
    dropClosed();
    setTabs(keep);
    const f = await readFile(t);
    setFile(f);
    setShowDiff(false);
    previewFor(t);
    setCompare(null);
    setSaveState(null);
  };
  const closeItems = (t: string) => {
    const i = tabs.indexOf(t);
    return [
      { label: tx('closeOthers'), disabled: tabs.length < 2, run: () => keepTabs(t, [t]) },
      { label: tx('closeToTheLeft'), disabled: i <= 0, run: () => keepTabs(t, tabs.slice(i)) },
      { label: tx('closeToTheRight'), disabled: i < 0 || i >= tabs.length - 1, run: () => keepTabs(t, tabs.slice(0, i + 1)) },
    ];
  };

  // On desktop, keep the preview area next to the tree even with no file open (mobile shows only the tree)
  const [desktop, setDesktop] = useState(() => !window.matchMedia('(max-width: 760px)').matches);
  useEffect(() => {
    const mq = window.matchMedia('(max-width: 760px)');
    const on = () => setDesktop(!mq.matches);
    mq.addEventListener('change', on);
    return () => mq.removeEventListener('change', on);
  }, []);
  const emptyPane = desktop && !file;
  // The panel is wide whenever the preview area shows (a file is open, or the empty placeholder on desktop)
  const paneShown = !!file || emptyPane;
  useEffect(() => onFileOpen?.(paneShown), [paneShown]); // eslint-disable-line react-hooks/exhaustive-deps

  // Remember opened files (per host + working directory; Files sessions per session) and restore the tabs and current file next time the panel opens
  const tabsKey = standalone ? `wp-open-s-${hostId}/${sessionId}` : `wp-open-${hostId}/${cwd}`;
  const untitled = untitledStore(tabsKey);
  /** Tab title: untitled tabs are named after their first line, like VS Code */
  const nameOf = (t: string) => (isUntitled(t) ? untitledLabel(t, t === file?.path ? file.draft : untitled.get(t) ?? '') : baseName(t));
  /** Closing untitled tabs (other than the current one) that have text: ask first, since nothing is on disk */
  const confirmLoseUntitled = async (list: string[]) => {
    const lost = list.filter((x) => isUntitled(x) && untitled.get(x));
    return (
      !lost.length ||
      askConfirm({
        title: tx('discardChangesTo', { v: lost.map(nameOf).join(', ') }),
        message: tx('unsavedEditsWillBeDiscarded'),
        okText: tx('discard'),
        danger: true,
      })
    );
  };
  /** Like VS Code's double-click on the empty tab bar: a new Untitled-N tab (the lowest number not open) */
  const newUntitled = () => {
    let n = 1;
    while (tabs.includes(`${UNTITLED}Untitled-${n}`)) n++;
    const t = `${UNTITLED}Untitled-${n}`;
    untitled.set(t, '');
    openFile(t);
  };
  const newUntitledRef = useRef(newUntitled);
  newUntitledRef.current = newUntitled;
  const restored = useRef(false);
  /** The panel was opened by "View changes": keep the side column visible when tabs are restored */
  const forChanges = useRef(false);
  const restoring = useRef(false);
  useEffect(() => {
    if (!root || restored.current) return;
    restored.current = true;
    const saved = lsGet<{ tabs: string[]; current: string | null } | null>(tabsKey, null);
    if (!saved?.tabs?.length) return;
    const r = norm(root);
    const inRoot = (p: string) => norm(p).startsWith(r + '/');
    const keep = [...new Set(saved.tabs.map((t) => (isSideTab(t) || isWebTab(t) || isUntitled(t) ? t : unverbatim(t))))].filter((t) =>
      isSideTab(t)
        ? hostSessions.some((s) => s.id === sideTabSession(t) && s.parentId === sessionId && (s.sideChat || !!s.review))
        : isUntitled(t)
          ? untitled.has(t)
          : isAiTab(t)
            ? aiTabSession(t) === sessionId
            : isWebTab(t)
              ? !webTabFile(t) || inRoot(webTabFile(t)!)
              : inRoot(t),
    );
    const cur = saved.current && keep.includes(saved.current) ? saved.current : keep[keep.length - 1];
    if (!cur) return;
    restoring.current = true;
    readFile(cur, true).then((f) => {
      restoring.current = false;
      // If the user (or a file link) already opened a file while restoring, keep it but still bring back the saved tabs
      if (fileRef.current) {
        if (f) setTabs((t) => [...keep.filter((x) => !t.includes(x)), ...t].slice(-30));
        return;
      }
      if (!f) return setTabs(keep.filter((t) => t !== cur));
      setTabs(keep);
      setFile(f);
      previewFor(cur);
      // On phones a restored file normally takes the screen; not when the panel was opened for "Changes".
      // On desktop, auto-hide the tree when restoring an AI browser / side chat tab unless explicitly toggled for this session.
      if (!forChanges.current) {
        if (window.matchMedia('(max-width: 760px)').matches) setTreeOpen(false);
        else if ((isAiTab(cur) || isSideTab(cur)) && lsGet<boolean | null>(treeOpenKey, null) === null) setTreeOpen(false);
      }
    });
  }, [root]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    if (!restored.current || restoring.current || !root) return;
    const persistTabs = tabs.filter((t) => !isReviewTab(t));
    const persistCur = file?.path && !isReviewTab(file.path) ? file.path : null;
    lsSet(tabsKey, { tabs: persistTabs, current: persistCur });
  }, [tabs.join('|'), file?.path, root]); // eslint-disable-line react-hooks/exhaustive-deps

  // Drop side chat tabs whose underlying session was deleted elsewhere (e.g. from the topbar side chat menu)
  useEffect(() => {
    if (!rawHostSessions) return;
    const dead = tabs.filter((t) => isSideTab(t) && !hostSessions.some((s) => s.id === sideTabSession(t)));
    if (!dead.length) return;
    const alive = tabs.filter((t) => !dead.includes(t));
    setTabs(alive);
    if (file && dead.includes(file.path)) {
      const next = alive[alive.length - 1];
      if (next) readFile(next, true).then((f) => setFile(f));
      else setFile(null);
    }
  }, [rawHostSessions, hostSessions]); // eslint-disable-line react-hooks/exhaustive-deps

  const reload = async () => {
    if (!file) return;
    const f = await readFile(file.path);
    if (f) {
      setFile(f);
      setSaveState(null);
      toast(tx('reloadedFromDisk', { v: baseName(f.path) }));
    }
  };

  /** Saving an untitled tab: ask for a path (relative to the workspace or absolute), write it, then the tab becomes that file */
  const saveUntitled = async (f: OpenFile) => {
    if (!root) return;
    const label = untitledLabel(f.path, f.draft).replace(/[\\/:*?"<>|]+/g, ' ').trim() || f.path.slice(UNTITLED.length);
    const name = (
      await askText({ title: tx('saveAs'), label: tx('pathRelativeToOrAbsolute', { v: root }), value: /\.[^.\s]+$/.test(label) ? label : `${label}.txt`, okText: tx('save') })
    )?.trim();
    if (!name) return;
    const path = isAbsPath(name) ? name : join(root, name.split(/[\\/]/).join(sepOf(root)));
    const write = (overwrite: boolean) => rpc(hostId, 'fs.write', { path, content: f.draft, createOnly: !overwrite });
    setSaveState('saving');
    try {
      const dir = parentOf(path);
      // Saving into a folder that doesn't exist yet: create it (fs.mkdir refuses an existing one, which is fine)
      if (norm(dir) !== norm(root)) await rpc(hostId, 'fs.mkdir', { path: dir }).catch(() => {});
      try {
        await write(false);
      } catch (e) {
        if (!String((e as Error)?.message ?? e).includes('已经存在同名文件')) throw e;
        const ok = await askConfirm({
          title: tx('overwrite', { name: baseName(path) }),
          message: tx('alreadyHasAFileWith', { dir: rel(dir) }),
          okText: tx('overwrite2'),
          danger: true,
        });
        if (!ok) return setSaveState(null);
        await write(true);
      }
      const nf = await readFile(path);
      untitled.delete(f.path);
      setTabs((t) => [...new Set(t.map((x) => (x === f.path ? path : x)))]);
      // Stay in the editor even for Markdown / HTML (no switch to the preview)
      if (nf) setFile((cur) => (cur?.path === f.path ? nf : cur));
      setSaveState(null);
      refreshTree();
      toast(tx('saved', { v: baseName(path) }));
    } catch (e) {
      setSaveState(null);
      errorToast(e);
    }
  };

  const save = async (overwrite = false) => {
    const f = fileRef.current;
    // As in VS Code, Ctrl+S on an untitled tab asks where to save it even when it's empty
    if (f?.untitled) return saveState === 'saving' ? undefined : saveUntitled(f);
    if (!f || f.draft === f.content || saveState === 'saving') return;
    setSaveState('saving');
    try {
      const r = await rpc<{ modified: number; size: number }>(hostId, 'fs.write', {
        path: f.path,
        content: f.draft,
        expectModified: overwrite ? undefined : f.modified,
        expectSize: overwrite ? undefined : f.size,
      });
      setFile((cur) => (cur && cur.path === f.path ? { ...cur, content: f.draft, modified: r.modified, size: r.size } : cur));
      setSaveState(null);
      loadGit(true);
      toast(tx('saved', { v: baseName(f.path) }));
    } catch (e) {
      const msg = String((e as Error)?.message ?? e);
      setSaveError(msg);
      setSaveState(msg.includes('被修改过') ? 'conflict' : 'error');
      if (!msg.includes('被修改过')) errorToast(e);
    }
  };

  // Turn ended: refresh the tree; reread open files that have no local edits so the agent's changes show up immediately
  const prevStatus = useRef(status);
  useEffect(() => {
    const was = prevStatus.current;
    prevStatus.current = status;
    if (!(was === 'running' || was === 'waiting-permission') || status === 'running') return;
    refreshTree();
    const f = fileRef.current;
    if (f && f.draft === f.content && !f.image && !f.web && !f.commit && !f.untitled) {
      rpc<Omit<OpenFile, 'draft' | 'rev'>>(hostId, 'fs.read', { path: f.path })
        .then((r) => {
          if (r.modified === f.modified) return;
          setFile((cur) => (cur && cur.path === f.path && cur.draft === cur.content ? { ...r, draft: r.content, rev: Date.now() } : cur));
        })
        .catch(() => {});
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status]);

  // An untitled tab's text stays in memory when the panel closes, so there's nothing to confirm for it
  const closePanel = async () => {
    if (file?.untitled || (await confirmDiscard())) onClose();
  };

  // ---------- New, rename, delete ----------

  const create = async (dir: string, kind: 'file' | 'dir') => {
    const name = await askText({ title: kind === 'file' ? tx('newFile') : tx('newFolder'), label: dir, placeholder: tx('name') });
    if (!name?.trim()) return;
    const path = join(dir, name.trim());
    try {
      if (kind === 'file') await rpc(hostId, 'fs.write', { path, content: '', createOnly: true });
      else await rpc(hostId, 'fs.mkdir', { path });
      if (dir !== root) setExpanded((s) => new Set(s).add(dir));
      await load(dir);
      if (kind === 'file') openFile(path);
    } catch (e) {
      errorToast(e);
    }
  };

  const rename = async (path: string) => {
    const old = baseName(path);
    const name = await askText({ title: tx('rename'), value: old });
    if (!name?.trim() || name.trim() === old) return;
    const to = join(parentOf(path), name.trim());
    try {
      await rpc(hostId, 'fs.rename', { from: path, to });
      // A renamed folder moves everything inside it: open tabs (including HTML previews), the current file and expanded folders
      const sep = sepOf(path);
      const move = (p: string) => (p === path ? to : p.startsWith(path + sep) ? to + p.slice(path.length) : p);
      const moveTab = (t: string) => {
        const f = isWebTab(t) ? webTabFile(t) : null;
        return f ? (move(f) === f ? t : t.slice(0, t.length - f.length) + move(f)) : isWebTab(t) ? t : move(t);
      };
      setFile((cur) => (cur && moveTab(cur.path) !== cur.path ? { ...cur, path: moveTab(cur.path) } : cur));
      setTabs((t) => t.map(moveTab));
      const moved = [...expandedRef.current].filter((d) => move(d) !== d).map(move);
      setExpanded((x) => new Set([...x].map(move)));
      await load(parentOf(path));
      for (const d of moved) load(d, true);
    } catch (err) {
      errorToast(err);
    }
  };

  const remove = async (path: string, isDir: boolean) => {
    const name = baseName(path);
    const ok = await askConfirm({
      title: isDir ? tx('deleteFolder', { name }) : tx('delete', { name }),
      message: isDir
        ? tx('thisCannotBeUndoneEmpty', { path })
        : tx('thisCannotBeUndone', { path }),
      okText: tx('delete2'),
      danger: true,
    });
    if (!ok) return;
    try {
      await rpc(hostId, 'fs.delete', { path });
      if (file?.path === path) setFile(null);
      setTabs((t) => t.filter((x) => x !== path));
      await load(parentOf(path));
    } catch (err) {
      errorToast(err);
    }
  };

  const rel = (p: string) => (root && p.startsWith(root) ? p.slice(root.length).replace(/^[\\/]/, '').replace(/\\/g, '/') || '.' : p);
  const copy = async (text: string, ok: string = tx('pathCopied')) => toast((await writeClipboard(text)) ? ok : tx('couldNotCopy'));
  const copyRef = async (path: string, isDir: boolean) => {
    const r = rel(path) + (isDir ? '/' : '');
    if (!(await writeClipboard(r))) return toast(tx('couldNotCopy'));
    copiedRef = { plain: r, ref: `@${r} ` };
    toast(standalone ? tx('relativePathCopied') : tx('copiedPasteInTheInput'));
  };
  // The row last clicked / right-clicked in the tree; Ctrl+C copies it
  const lastRow = useRef<{ path: string; isDir: boolean } | null>(null);
  const addToChat = (path: string, isDir: boolean) => {
    const refPath = isOtherWorktree ? path : rel(path);
    window.dispatchEvent(new CustomEvent(INSERT_COMPOSER, { detail: { sessionId, text: `@${refPath}${isDir ? '/' : ''} ` } }));
    if (window.matchMedia('(max-width: 760px)').matches) {
      onClose();
      toast(tx('addedToInput'));
    }
  };

  // ---------- Git operations: discard, push, switch/create branch, switch worktree ----------
  const doDiscard = async (rels: string[], label: string) => {
    if (!rels.length) return;
    const ok = await askConfirm({
      title: tx('discardGitChangesTitle', { v: label }),
      message: tx('discardGitChangesWarn'),
      okText: tx('discard'),
      danger: true,
    });
    if (!ok) return;
    try {
      await rpc(hostId, 'git.discard', { cwd, paths: rels });
      toast(tx('discardedChanges'));
      refreshTree();
      const f = fileRef.current;
      if (f && !f.web && !f.commit && !f.review && !f.untitled) {
        const g = gitOf(f.path);
        if (g && rels.includes(g.rel)) {
          const next = await readFile(f.path, true);
          if (next) setFile(next);
          else closeTab(f.path);
        }
      }
    } catch (e) {
      errorToast(e);
    }
  };

  const doPush = async () => {
    if (pushing || !git) return;
    setPushing(true);
    try {
      const r = await rpc<{ branch: string; remote: string }>(hostId, 'git.push', { cwd });
      toast(tx('pushedBranch', { branch: r.branch, remote: r.remote }));
      void loadBranches();
    } catch (e) {
      errorToast(e);
    } finally {
      setPushing(false);
    }
  };

  const toggleBwPopover = () => {
    if (bwOpen) {
      setBwOpen(false);
      return;
    }
    setBwOpen(true);
    setBranchesLoading(true);
    loadWorktrees();
    void loadBranches().finally(() => setBranchesLoading(false));
  };

  const switchWorktree = (targetPath: string) => {
    setNewWorktree(null);
    if (onSwitchCwd && norm(targetPath) !== norm(cwd)) onSwitchCwd(targetPath);
  };

  const checkoutBranch = async (name: string) => {
    try {
      await rpc(hostId, 'git.checkout', { cwd, branch: name });
      toast(tx('switchedToBranch', { branch: name }));
      refreshTree();
      void loadBranches();
    } catch (err) {
      errorToast(err);
    }
  };

  const createBranch = async () => {
    const name = (await askText({ title: tx('newBranch'), placeholder: tx('newBranchPlaceholder') }))?.trim();
    if (!name) return;
    try {
      await rpc(hostId, 'git.checkout', { cwd, branch: name, create: true });
      toast(tx('switchedToBranch', { branch: name }));
      refreshTree();
      void loadBranches();
    } catch (err) {
      errorToast(err);
    }
  };

  const openBaseBranchMenu = async (e: React.MouseEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    const info = (await loadBranches()) ?? branchesInfo;
    const activeBase = branchBase || branchDiff?.base || info?.defaultBranch || '';
    const names = [...new Set([...(info?.defaultBranch ? [info.defaultBranch] : []), ...(info?.branches ?? []).map((b) => b.name)])];
    openMenu(
      { clientX: r.left, clientY: r.bottom + 4 },
      names.map((name) => ({
        label: name,
        icon: 'git-branch',
        active: name === activeBase,
        hint: name === info?.defaultBranch ? tx('defaultBranchTag') : undefined,
        run: () => {
          setBranchBase(name === info?.defaultBranch ? '' : name);
          setDiffScope('branch');
        },
      })),
    );
  };

  const openTurnMenu = (e: React.MouseEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    openMenu(
      { clientX: r.left, clientY: r.bottom + 4 },
      [...turnCheckpoints].reverse().map((c) => ({
        label: tx('diffScopeTurnN', { n: c.turn }),
        icon: 'history',
        active: activeTurnCkpt?.turn === c.turn,
        hint: `+${c.added} −${c.deleted}`,
        run: () => {
          setSelectedTurn(c.turn);
          setDiffScope('turn');
        },
      })),
    );
  };

  // ---------- Download, open in the system ----------
  const download = async (path: string) => {
    try {
      const r = await rpc<{ base64: string }>(hostId, 'fs.read', { path, raw: true });
      const name = baseName(path);
      if (isTauri) {
        const { invoke } = await import('@tauri-apps/api/core');
        try {
          if (await invoke<boolean>('save_file', { name, data: r.base64 })) toast(tx('saved2', { name }));
          return;
        } catch {
          // No save dialog on mobile, fall back to a browser download
        }
      }
      const a = document.createElement('a');
      a.href = `data:application/octet-stream;base64,${r.base64}`;
      a.download = name;
      document.body.appendChild(a);
      a.click();
      a.remove();
    } catch (e) {
      errorToast(e);
    }
  };
  const openNative = (path: string, reveal: boolean) =>
    rpc(hostId, 'fs.open', { path, reveal }).catch((e) =>
      errorToast(String((e as Error)?.message ?? e).includes('未知方法') ? tx('daemonIsTooOldRestart') : e),
    );
  const nativeItems = (path: string): MenuItem[] =>
    isLocal
      ? [
          { label: tx('openWithDefaultApp'), icon: 'external', run: () => openNative(path, false) },
          { label: tx('showInFileManager'), icon: 'folder', run: () => openNative(path, true) },
        ]
      : [];

  // ---------- Side by side, compare ----------
  const wide = () => !window.matchMedia('(max-width: 900px)').matches;
  const openSecond = async (path: string) => {
    if (!file) return openFile(path);
    if (path === file.path) return;
    const f = await readFile(path);
    if (!f) return;
    setSecond(f);
    // Three columns is too cramped: collapse the tree first (not saved in settings); the top-right button reopens it
    setTreeOpen(false);
  };
  const compareWith = async (path: string) => {
    if (!file || path === file.path) return;
    if (file.binary) return toast(tx('currentFileIsNotText'));
    const f = await readFile(path);
    if (!f) return;
    if (f.binary) return toast(tx('isNotATextFile', { v: baseName(path) }));
    setCompare({ path, text: f.content });
    setShowDiff(false);
  };

  // ---------- Drag and drop ----------
  const dragRow = (e: React.DragEvent, r: TreeRow) => {
    e.dataTransfer.setData(DRAG_REF, `@${rel(r.path)}${r.isDir ? '/' : ''} `);
    e.dataTransfer.setData('text/plain', r.path);
    e.dataTransfer.effectAllowed = 'copy';
  };
  const upload = async (dir: string, list: FileList) => {
    const files = [...list];
    if (!files.length) return;
    setUploading((n) => n + files.length);
    let ok = 0;
    for (const f of files) {
      try {
        if (f.size > UPLOAD_MAX) throw new Error(tx('exceeds50mb', { name: f.name }));
        const data = toB64(await f.arrayBuffer());
        const path = join(dir, f.name);
        try {
          await rpc(hostId, 'fs.upload', { path, data });
        } catch (e) {
          if (!String((e as Error)?.message ?? e).includes('已经存在')) throw e;
          const yes = await askConfirm({
            title: tx('overwrite', { name: f.name }),
            message: tx('alreadyHasAFileWith', { dir }),
            okText: tx('overwrite2'),
            danger: true,
          });
          if (!yes) continue;
          await rpc(hostId, 'fs.upload', { path, data, overwrite: true });
        }
        ok++;
      } catch (e) {
        errorToast(e);
      } finally {
        setUploading((n) => n - 1);
      }
    }
    if (dir !== root) setExpanded((x) => new Set(x).add(dir));
    await load(dir);
    loadGit();
    if (ok) toast(tx('uploadedFilesTo', { ok, v: rel(dir) }));
  };
  const treeDrop = {
    onDragOver: (e: React.DragEvent) => {
      if (!e.dataTransfer.types.includes('Files') || !root) return;
      e.preventDefault();
      if (dropDir === null) setDropDir(root);
    },
    onDragLeave: (e: React.DragEvent) => {
      if (!e.currentTarget.contains(e.relatedTarget as Node)) setDropDir(null);
    },
    onDrop: (e: React.DragEvent) => {
      if (!e.dataTransfer.files.length || !root) return;
      e.preventDefault();
      const dir = dropDir ?? root;
      setDropDir(null);
      upload(dir, e.dataTransfer.files);
    },
  };

  // ---------- Keyboard in the tree (VS Code's list bindings) ----------
  const [focusKey, setFocusKey] = useState<string | null>(null);
  const treeBox = useRef<HTMLDivElement>(null);
  const moveFocus = (r: TreeRow | undefined) => {
    if (!r) return;
    setFocusKey(r.key);
    lastRow.current = { path: r.path, isDir: r.isDir };
    requestAnimationFrame(() =>
      treeBox.current?.querySelector(`[data-key="${CSS.escape(r.key)}"]`)?.scrollIntoView({ block: 'nearest' }),
    );
  };
  const treeKey = (e: React.KeyboardEvent) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'c' && lastRow.current && !window.getSelection()?.toString()) {
      e.preventDefault();
      return copyRef(lastRow.current.path, lastRow.current.isDir);
    }
    if (e.target !== e.currentTarget || e.ctrlKey || e.metaKey || e.altKey) return;
    const list = view === 'all' && q ? sRows : view === 'changes' ? cRows : view === 'all' ? rows : [];
    if (!list.length) return;
    let i = list.findIndex((r) => r.key === focusKey);
    if (i < 0) i = Math.max(0, list.findIndex((r) => r.path === file?.path));
    const r = list[i];
    const flip = (x: TreeRow) => (view === 'all' && !q ? toggle(x.path) : toggleCollapsed(x));
    const page = Math.max(1, Math.floor((treeBox.current?.clientHeight ?? 300) / 28) - 1);
    const go: Record<string, () => void> = {
      ArrowDown: () => moveFocus(list[focusKey == null ? i : Math.min(list.length - 1, i + 1)]),
      ArrowUp: () => moveFocus(list[Math.max(0, i - 1)]),
      PageDown: () => moveFocus(list[Math.min(list.length - 1, i + page)]),
      PageUp: () => moveFocus(list[Math.max(0, i - page)]),
      Home: () => moveFocus(list[0]),
      End: () => moveFocus(list[list.length - 1]),
      // Right: expand a folder, or step into it when already open; Left: collapse, or go to the parent folder
      ArrowRight: () => (r.isDir ? (r.expanded ? list[i + 1]?.depth > r.depth && moveFocus(list[i + 1]) : flip(r)) : undefined),
      ArrowLeft: () => {
        if (r.isDir && r.expanded) return flip(r);
        for (let j = i - 1; j >= 0; j--) if (list[j].depth < r.depth) return moveFocus(list[j]);
      },
      Enter: () => (r.isDir ? flip(r) : openFile(r.path, view === 'changes')),
      ' ': () => (r.isDir ? flip(r) : openFile(r.path, view === 'changes')),
      F2: () => !q && view === 'all' && rename(r.path),
      Delete: () => !q && view === 'all' && remove(r.path, r.isDir),
    };
    const f = go[e.key];
    if (!f) return;
    e.preventDefault();
    setFocusKey(r.key);
    f();
  };

  const rowMenu = (ev: React.MouseEvent, r: TreeRow) => {
    if (r.loading) return;
    const dir = r.isDir ? r.path : parentOf(r.path);
    const canSide = !r.isDir && !!file && !file.web && !file.commit && !file.review && !file.side && file.path !== r.path && wide();
    const rowGit = !r.isDir ? gitOf(r.path) : undefined;
    openMenu(ev, [
      r.isDir
        ? { label: r.expanded ? tx('collapse') : tx('expand'), icon: 'folder', run: () => toggle(r.path) }
        : { label: tx('open'), icon: 'file', run: () => openFile(r.path) },
      ...(r.isDir && !r.loading
        ? [{ label: tx('findInFolder'), icon: 'search', run: () => (pickView('search'), setGrepInclude({ value: `./${rel(r.path)}`, n: Date.now() })) }]
        : []),
      ...(!r.isDir && isHtml(r.path) ? [{ label: tx('openInBrowserTab'), icon: 'globe', run: () => openWeb(`file:${r.path}`) }] : []),
      ...(canSide
        ? [
            { label: tx('openToTheSide'), icon: 'columns', run: () => openSecond(r.path) },
            { label: tx('compareWith', { v: baseName(file!.path) }), icon: 'compare', run: () => compareWith(r.path) },
          ]
        : []),
      ...(standalone ? [] : [{ label: tx('addToChat'), icon: 'chat', run: () => addToChat(r.path, r.isDir) }]),
      { label: tx('copy'), icon: 'copy', hint: `${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}C`, run: () => copyRef(r.path, r.isDir) },
      '-',
      ...(r.isDir ? [] : [{ label: tx('download'), icon: 'download', run: () => download(r.path) }]),
      ...nativeItems(r.path),
      { label: tx('copyPath'), icon: 'copy', run: () => copy(r.path) },
      { label: tx('copyRelativePath'), run: () => copy(rel(r.path)) },
      '-',
      { label: tx('newFile'), icon: 'plus', run: () => create(dir, 'file') },
      { label: tx('newFolder'), icon: 'folder', run: () => create(dir, 'dir') },
      ...(rowGit
        ? ['-' as const, { label: tx('discardFileChanges'), icon: 'rotate-l', danger: true, run: () => void doDiscard([rowGit.rel], baseName(r.path)) }]
        : []),
      '-',
      { label: tx('rename'), icon: 'edit', run: () => rename(r.path) },
      { label: tx('delete2'), icon: 'trash', danger: true, run: () => remove(r.path, r.isDir) },
    ]);
  };

  const toggleWrap = () => {
    lsSet('wp-wrap', !wrap);
    setWrap(!wrap);
  };
  const displayRefPath = (targetPath: string, commitHash?: string) => {
    if (commitHash) return targetPath;
    if (isUntitled(targetPath)) return nameOf(targetPath);
    return isOtherWorktree ? targetPath : rel(targetPath);
  };
  const pushContextRef = (
    targetPath: string,
    snip: { kind: 'diff' | 'code'; fromLine: number; toLine: number; added?: number; deleted?: number; content: string },
    commitHash?: string,
  ) => {
    if (standalone) return;
    const unt = isUntitled(targetPath);
    const displayPath = displayRefPath(targetPath, commitHash);
    addContextRefToChat(sessionId, {
      kind: snip.kind,
      path: displayPath,
      commit: commitHash,
      untitled: unt || undefined,
      fromLine: snip.fromLine,
      toLine: snip.toLine,
      added: snip.added,
      deleted: snip.deleted,
      content: snip.content,
    });
    toast(tx('addedToInput'));
    if (window.matchMedia('(max-width: 760px)').matches) onClose();
  };
  const pushReviewComment = (
    targetPath: string,
    snip: { kind: 'diff' | 'code'; fromLine: number; toLine: number; added?: number; deleted?: number; content: string },
    comment: string,
    commitHash?: string,
  ) => {
    if (standalone) return;
    const unt = isUntitled(targetPath);
    const displayPath = displayRefPath(targetPath, commitHash);
    addContextRefToChat(
      sessionId,
      {
        kind: 'review',
        path: displayPath,
        commit: commitHash,
        untitled: unt || undefined,
        fromLine: snip.fromLine,
        toLine: snip.toLine,
        added: snip.added,
        deleted: snip.deleted,
        comment,
        content: snip.content,
      },
      { focus: false },
    );
    toast(tx('reviewCommentAdded'));
  };
  /** Right-click menu inside the editor (VS Code's editor context menu, plus Cursor's "add selection to chat") */
  const editorMenu = (path: string, commitHash?: string) => (c: EditorMenuCtx): (MenuItem | '-')[] => {
    const mod = navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+';
    const onDisk = !isUntitled(path) && !commitHash;
    const displayPath = commitHash ? path : isUntitled(path) ? nameOf(path) : rel(path);
    const lines = c.fromLine === c.toLine ? `${c.fromLine}` : `${c.fromLine}-${c.toLine}`;
    return [
      { label: tx('cut'), icon: 'cut', hint: `${mod}X`, disabled: !c.selection || c.readOnly, run: c.cut },
      { label: tx('copy'), icon: 'copy', hint: `${mod}C`, disabled: !c.selection, run: c.copy },
      { label: tx('paste'), icon: 'paste', hint: `${mod}V`, disabled: c.readOnly, run: c.paste },
      { label: tx('selectAll'), hint: `${mod}A`, run: c.selectAll },
      '-',
      { label: tx('find'), icon: 'search', hint: `${mod}F`, run: c.find },
      { label: tx('goToLine'), hint: `${mod}G`, run: () => setQuick(':') },
      '-',
      ...(!standalone && (c.selection || onDisk)
        ? [
            c.selection
              ? {
                  label: tx('addSelectionToChat'),
                  icon: 'chat',
                  hint: `${mod}L`,
                  run: () =>
                    c.addSelection
                      ? c.addSelection()
                      : pushContextRef(
                          path,
                          { kind: 'code', fromLine: c.fromLine, toLine: c.toLine, content: c.selectedLines || c.selection },
                          commitHash,
                        ),
                }
              : { label: tx('addToChat'), icon: 'chat', run: () => addToChat(path, false) },
          ]
        : []),
      // Old side of a diff: its line numbers don't point into the file on disk
      ...(!isUntitled(path) && !c.oldSide ? [{ label: tx('copyPathWithLine'), icon: 'copy', run: () => copy(`${displayPath}:${lines}`) }] : []),
      '-',
      { label: wrap ? tx('disableWordWrap') : tx('enableWordWrap'), icon: 'wrap', hint: 'Alt+Z', run: toggleWrap },
      { label: tx('foldAll'), icon: 'fold-all', hint: `${mod}Alt+[`, run: c.foldAll },
      { label: tx('unfoldAll'), icon: 'unfold-all', hint: `${mod}Alt+]`, run: c.unfoldAll },
    ];
  };

  const fileMenu = (e: React.MouseEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    const items: (MenuItem | '-')[] = file?.untitled
      ? [
          { label: tx('goToSymbol'), icon: 'search', hint: `${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}Shift+O`, run: () => setQuick('@') },
          { label: tx('goToLine'), hint: `${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}G`, run: () => setQuick(':') },
          '-',
          { label: tx('saveAs'), icon: 'save', run: () => save() },
          { label: tx('copyFileContents'), icon: 'copy', run: () => copy(file.draft, tx('fileContentsCopied')) },
          '-',
          { label: wrap ? tx('disableWordWrap') : tx('enableWordWrap'), icon: 'wrap', hint: 'Alt+Z', run: toggleWrap },
        ]
      : file
      ? [
          { label: tx('goToSymbol'), icon: 'search', hint: `${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}Shift+O`, run: () => setQuick('@') },
          { label: tx('goToLine'), hint: `${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}G`, run: () => setQuick(':') },
          '-',
          { label: tx('download'), icon: 'download', disabled: !!file.deleted, run: () => download(file.path) },
          ...nativeItems(file.path),
          { label: tx('copyPath'), icon: 'copy', run: () => copy(file.path) },
          { label: tx('copyRelativePath'), run: () => copy(rel(file.path)) },
          { label: tx('copyFileContents'), icon: 'copy', disabled: !!file.image || file.binary, run: () => copy(file.draft, tx('fileContentsCopied')) },
          ...(standalone
            ? []
            : [
                { label: tx('addToChat'), icon: 'chat', run: () => addToChat(file.path, false) },
                { label: tx('copyPasteInInputTo'), run: () => copyRef(file.path, false) },
              ]),
          '-',
          { label: wrap ? tx('disableWordWrap') : tx('enableWordWrap'), icon: 'wrap', hint: 'Alt+Z', run: toggleWrap },
          dirty
            ? { label: tx('discardAndReload'), icon: 'refresh', danger: true, run: async () => (await confirmDiscard()) && reload() }
            : { label: tx('reload'), icon: 'refresh', run: reload },
          '-',
          { label: tx('rename'), icon: 'edit', run: () => rename(file.path) },
        ]
      : [
          { label: tx('copyDirectoryPath'), icon: 'copy', run: () => copy(root ?? cwd) },
          { label: hidden ? tx('hideHiddenFiles') : tx('showHiddenFiles'), icon: 'eye', run: () => (lsSet('wp-hidden', !hidden), setHidden(!hidden)) },
        ];
    openMenu({ clientX: r.right - 220, clientY: r.bottom + 4 }, items);
  };

  // ---------- Shortcuts: Ctrl+P quick open, Ctrl+Shift+O symbols, Ctrl+G go to line, Ctrl+Alt+N new untitled file ----------
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.isComposing || e.keyCode === 229 || getState().palette) return;
      if (!(e.ctrlKey || e.metaKey)) return;
      const k = e.key.toLowerCase();
      if (e.altKey) {
        if (!e.shiftKey && (k === 'n' || e.code === 'KeyN')) {
          e.preventDefault();
          newUntitledRef.current();
        }
        return;
      }
      const m: QuickMode | null = k === 'p' && !e.shiftKey ? '' : k === 'o' && e.shiftKey ? '@' : k === 'g' && !e.shiftKey ? ':' : null;
      if (m === null || (m !== '' && !fileRef.current)) return;
      e.preventDefault();
      setQuick(m);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  const conflictMenu = (e: React.MouseEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    openMenu({ clientX: r.left, clientY: r.bottom + 4 }, [
      { label: tx('loadFromDiskDiscardEdits'), icon: 'refresh', run: reload },
      { label: tx('overwriteDiskWithCurrentContents'), icon: 'save', danger: true, run: () => save(true) },
    ]);
  };

  // ---------- Drag to resize the file tree ----------
  const dragTree = (e: React.PointerEvent<HTMLDivElement>) => {
    const w0 = Math.round(sideRef.current?.getBoundingClientRect().width ?? treeW);
    const bodyW = bodyRef.current?.clientWidth ?? 0;
    const max = bodyW > 0 ? Math.min(TREE_MAX, Math.max(TREE_MIN, bodyW - MAIN_MIN)) : TREE_MAX;
    const min = Math.min(TREE_MIN, max);
    beginWidthDrag(e, {
      initialWidth: w0,
      min,
      max,
      onMove: setTreeW,
      onEnd: (w) => lsSet('wp-tree-w', w),
      cleanupRef: dragCleanupRef,
    });
  };

  const rows = useMemo(() => (root ? flatten(root, 0, kids, expanded) : []), [root, kids, expanded, lang]);
  const sRows = useMemo(() => (root && search ? searchRows(root, search.paths, collapsed) : []), [root, search, collapsed]);
  // "Changes": files changed under this working directory (uncommitted vs HEAD, or branch vs merge-base)
  // Outside a git repo, the files tools changed in this session stand in for git status
  const uncommittedKeys = useMemo(() => {
    if (!root || !git) return [];
    const r = norm(root);
    return [...git.files.keys()].filter((k) => k.startsWith(r + '/'));
  }, [git, root]);
  const branchKeys = useMemo(() => {
    if (!root || !branchDiff) return [];
    const r = norm(root);
    return [...branchDiff.files.keys()].filter((k) => k.startsWith(r + '/'));
  }, [branchDiff, root]);
  const turnKeys = useMemo(() => {
    if (!root || !turnDiff) return [];
    const r = norm(root);
    return [...turnDiff.files.keys()].filter((k) => k.startsWith(r + '/'));
  }, [turnDiff, root]);
  const activeChangesGit: GitInfo | null = useMemo(() => {
    if (diffScope === 'turn' && turnDiff && git) {
      return { ...git, files: turnDiff.files, total: turnDiff.total };
    }
    if (diffScope === 'branch' && branchDiff && git) {
      return { ...git, files: branchDiff.files, total: branchDiff.total };
    }
    return git;
  }, [diffScope, turnDiff, branchDiff, git]);
  const allChanged = useMemo(() => {
    if (!root) return [];
    if (diffScope === 'turn' && git) return turnKeys;
    if (diffScope === 'branch' && git) return branchKeys;
    const keys = git ? uncommittedKeys : gitReady ? [...touched.keys()] : [];
    const r = norm(root);
    return keys.filter((k) => k.startsWith(r + '/'));
  }, [diffScope, git, turnKeys, branchKeys, uncommittedKeys, gitReady, touched, root]);
  const changed = useMemo(() => {
    if (!root) return [];
    const r = norm(root);
    // Keys are normalized (lowercased on Windows drives): take the original spelling from git status when there is one
    return allChanged.map((k) => (activeChangesGit?.files.get(k)?.path.replace(/\\/g, '/') ?? k).slice(r.length + 1));
  }, [allChanged, root, activeChangesGit]);
  // Totals at the top of the Changes view: lines added/removed across the listed files (files that can't be computed are skipped)
  const totals = useMemo(() => {
    let a = 0, d = 0, any = false;
    for (const k of allChanged) {
      const f = activeChangesGit?.files.get(k);
      if (f?.added != null || f?.deleted != null) any = true;
      a += f?.added ?? 0;
      d += f?.deleted ?? 0;
    }
    return any ? { a, d } : null;
  }, [allChanged, activeChangesGit]);

  const normRootSlash = root ? root.replace(/\\/g, '/').replace(/\/+$/, '') : '';
  const reviewRelOf = useCallback(
    (p: string) => {
      const ps = p.replace(/\\/g, '/');
      if (normRootSlash && ps.toLowerCase().startsWith((normRootSlash + '/').toLowerCase())) {
        return ps.slice(normRootSlash.length + 1);
      }
      if (root && ps.startsWith(root.replace(/\\/g, '/'))) {
        return ps.slice(root.length).replace(/^[\\/]/, '') || '.';
      }
      return ps;
    },
    [normRootSlash, root],
  );
  const knownReviewFiles = useMemo(() => {
    const s = new Set<string>(changed);
    for (const p of activeReviewMeta?.includedFiles ?? []) s.add(p);
    return s;
  }, [changed, activeReviewMeta?.includedFiles]);
  const activeReviewLive = useMemo(
    () => (activeReviewId ? [...activeReviewData.live.values()].join('\n').trim() : ''),
    [activeReviewData.live, activeReviewData.version, activeReviewId],
  );
  const activeReviewSnapshot = useMemo(
    () =>
      activeReviewId
        ? extractReviewSnapshot(activeReviewData.events, activeReviewLive, activeReviewBusy, {
            knownFiles: knownReviewFiles,
            cwd: activeReviewInfo?.cwd ?? cwd,
          })
        : { issues: [], prose: '', latestText: '', hasStructuredBlock: false, round: 0, messageSeq: 0 },
    [activeReviewId, activeReviewData.events, activeReviewData.events.length, activeReviewLive, activeReviewBusy, knownReviewFiles, activeReviewInfo?.cwd, cwd],
  );
  const activeReviewThreads = useMemo(
    () => (activeReviewId ? extractFindingThreads(activeReviewData.events) : new Map()),
    [activeReviewId, activeReviewData.events, activeReviewData.events.length],
  );
  const activeReviewStream = useMemo(
    () => (activeReviewId ? getActiveFindingStream(activeReviewData.events, activeReviewLive, activeReviewBusy) : null),
    [activeReviewId, activeReviewData.events, activeReviewData.events.length, activeReviewLive, activeReviewBusy],
  );
  const activeReviewPendingPerms = useMemo(() => {
    if (!activeReviewId) return [];
    const open = new Map<string, Extract<HubEvent, { kind: 'permission_request' }>>();
    for (const e of activeReviewData.events) {
      if (e.kind === 'permission_request') open.set(e.requestId, e);
      if (e.kind === 'permission_resolved') open.delete(e.requestId);
    }
    return [...open.values()];
  }, [activeReviewId, activeReviewData.events, activeReviewData.events.length]);
  const answerActiveReviewPerm = (requestId: string, optionId: string, answers?: QuestionAnswer[]) => {
    if (!activeReviewId) return;
    rpc(hostId, 'session.permission', {
      sessionId: activeReviewId,
      requestId,
      optionId,
      answers,
    }).catch(errorToast);
  };
  const activeDispositions = activeReviewId ? getAllDispositions(hostId, activeReviewId) : {};
  const fileIssueBadges = useMemo(
    () =>
      activeReviewId
        ? computeFileIssueBadges(activeReviewSnapshot.issues, activeDispositions, knownReviewFiles, activeReviewInfo?.cwd ?? cwd)
        : new Map<string, FileIssueBadge>(),
    [activeReviewId, activeReviewSnapshot.issues, activeDispositions, knownReviewFiles, activeReviewInfo?.cwd, cwd],
  );
  const activeReviewTotals = useMemo(() => {
    let high = 0, medium = 0, low = 0, open = 0;
    for (const it of activeReviewSnapshot.issues) {
      const d = activeDispositions[it.id] ?? 'open';
      if (d === 'dismissed') continue;
      if (it.severity === 'high') high += 1;
      else if (it.severity === 'medium') medium += 1;
      else low += 1;
      if (d === 'open') open += 1;
    }
    return { high, medium, low, total: high + medium + low, open };
  }, [activeReviewSnapshot.issues, activeDispositions]);
  const reviewIncludedSet = useMemo(
    () => (activeReviewMeta?.includedFiles?.length ? new Set(activeReviewMeta.includedFiles) : null),
    [activeReviewMeta?.includedFiles],
  );
  const reviewCleanCheck = useCallback(
    (relPath: string) => {
      if (!activeReviewId || activeReviewBusy || !activeReviewSnapshot.hasStructuredBlock) return false;
      if (reviewIncludedSet && !reviewIncludedSet.has(relPath)) return false;
      return true;
    },
    [activeReviewId, activeReviewBusy, activeReviewSnapshot.hasStructuredBlock, reviewIncludedSet],
  );
  const displayedChanged = useMemo(() => {
    if (!reviewOnlyFilter || !activeReviewId) return changed;
    return changed.filter((r) => (fileIssueBadges.get(r)?.total ?? 0) > 0);
  }, [changed, reviewOnlyFilter, activeReviewId, fileIssueBadges]);
  const cRows = useMemo(() => (root ? searchRows(root, displayedChanged, collapsed) : []), [root, displayedChanged, collapsed]);
  const toggleCollapsed = (r: TreeRow) =>
    setCollapsed((s) => {
      const n = new Set(s);
      const k = r.key.slice(2);
      if (n.has(k)) n.delete(k);
      else n.add(k);
      return n;
    });
  const pickView = (v: 'all' | 'changes' | 'search') => {
    lsSet(viewKey, v);
    setView(v);
    setCollapsed(new Set());
  };
  // "View changes": switch the side column to "Changes" and make sure it's visible; the editor keeps whatever it shows
  // (like VS Code's Source Control view). Taken on mount too, since a closed panel mounts after the event has fired
  const autoOpenTurnRef = useRef<number | null>(null);
  useEffect(() => {
    const take = () => {
      if (!takeOpenChanges(hostId, sessionId)) return;
      const reqTurn = takeOpenTurn(hostId, sessionId);
      forChanges.current = true;
      pickView('changes');
      if (reqTurn != null) {
        autoOpenTurnRef.current = reqTurn;
        setSelectedTurn(reqTurn);
        setDiffScope('turn');
      }
      setTree(true);
      loadGit();
    };
    take();
    const on = (e: Event) => {
      const d = (e as CustomEvent<{ hostId: string; sessionId: string; turn?: number }>).detail;
      if (d.hostId === hostId && d.sessionId === sessionId) take();
    };
    window.addEventListener(OPEN_CHANGES, on);
    return () => window.removeEventListener(OPEN_CHANGES, on);
  }, [hostId, sessionId, loadGit]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    if (autoOpenTurnRef.current == null || diffScope !== 'turn' || !activeTurnCkpt || activeTurnCkpt.turn !== autoOpenTurnRef.current) return;
    if (!turnDiff || turnDiff.endRef !== activeTurnCkpt.endRef || !turnDiff.files.size) return;
    autoOpenTurnRef.current = null;
    const first = [...turnDiff.files.values()][0];
    if (first) void openFileRef.current(first.path, true);
  }, [diffScope, activeTurnCkpt, turnDiff]);
  const showTree = treeOpen || !file;
  // On mobile, after opening a file from the tree, back returns to the tree first; pressing again closes the whole file panel
  useBack(!!file && !treeOpen && window.matchMedia('(max-width: 760px)').matches, () => setTreeOpen(true));
  useBack(quick !== null, () => setQuick(null));
  const fileDiff = file ? diffOf(file.path) : null;
  const fileSessionDiffs = file ? sessionDiffsOf(file.path) : [];
  const diffPending = !!curGit && !curGit.diff && !(curDiffCacheKey in fileDiffs);
  const state = saveState ?? (dirty ? 'dirty' : null);
  const md = !!file && isMarkdown(file.path) && !file.binary;
  const csv = !!file && isCsv(file.path) && !file.binary;
  const html = !!file && isHtml(file.path) && !file.binary;
  const relOpen = (r: string) => (root ? join(root, r.split('/').join(sepOf(root))) : r);
  const layout: 'split' | 'unified' = diffLayout === 'split' && wide() ? 'split' : 'unified';
  const flipLayout = () => {
    const v = diffLayout === 'split' ? 'unified' : 'split';
    lsSet('wp-diff-mode', v);
    setDiffLayout(v);
  };
  const viewing = compare ? 'compare' : showDiff ? 'diff' : 'file';

  // ---------- Full-file diff (VS Code style): both whole files, falling back to the hunk-only diff ----------
  /** Revisions to fetch for the current view: working tree vs HEAD/merge-base, turn checkpoint pair, or commit vs parent */
  const isTurnSnapshot = diffScope === 'turn' && !!activeTurnCkpt?.endRef;
  const diffBaseRev = isTurnSnapshot
    ? activeTurnCkpt!.baseRef
    : diffScope === 'branch' && branchDiff?.mergeBase
      ? branchDiff.mergeBase
      : 'HEAD';
  const diffEndRev = isTurnSnapshot ? activeTurnCkpt!.endRef! : '';
  const blobSpecs: [string, string][] = !file
    ? []
    : file.commit
      ? [[file.commit.baseHash ?? `${file.commit.hash}^`, file.commit.rel], [file.commit.hash, file.commit.rel]]
      : viewing === 'diff' && curGit && isTurnSnapshot
        ? [[diffBaseRev, curGit.rel], [diffEndRev, curGit.rel]]
        : viewing === 'diff' && curGit && !file.truncated && (!file.binary || file.deleted)
          ? [[diffBaseRev, curGit.rel]]
          : [];
  const blobWant = blobSpecs.map(([r, p]) => blobKey(r, p)).join('|');
  useEffect(() => {
    for (const [r, p] of blobSpecs) {
      const k = blobKey(r, p);
      if (k in blobs) continue;
      rpc<GitBlob>(hostId, 'git.blob', { cwd, path: p, rev: r })
        .then((b) => setBlobs((m) => ({ ...m, [k]: b })))
        // Older yi-agentd without git.blob, or a read error: use the hunk-only diff
        .catch(() => setBlobs((m) => ({ ...m, [k]: null })));
    }
  }, [blobWant, blobs]); // eslint-disable-line react-hooks/exhaustive-deps
  const blobText = (b: GitBlob | null | undefined): string | null | undefined =>
    b === undefined ? undefined : !b || b.binary || b.tooLarge ? null : b.exists ? (b.text ?? '') : '';
  /** {a, b} for MergeDiff; undefined while loading; null to fall back to PaneDiff */
  const full: { a: string; b: string } | null | undefined = (() => {
    if (!file) return null;
    if (viewing === 'compare') return compare && !file.truncated && !file.binary ? { a: file.draft, b: compare.text } : null;
    if (!blobSpecs.length) return null;
    const stale = staleBlobs.current.key === gitKey(hostId, cwd) ? staleBlobs.current.blobs : {};
    const texts = blobSpecs.map(([r, p]) => {
      const k = blobKey(r, p);
      return blobText(k in blobs ? blobs[k] : stale[k]);
    });
    if (texts.some((x) => x === null)) return null;
    if (texts.some((x) => x === undefined)) return undefined;
    // Historical snapshot (commit or turn checkpoint): both sides from git.blob; working tree: editor draft
    return file.commit || isTurnSnapshot ? { a: texts[0]!, b: texts[1]! } : { a: texts[0]!, b: file.deleted ? '' : file.draft };
  })();
  const flipCollapse = () => {
    lsSet('wp-diff-fold', !diffCollapse);
    setDiffCollapse(!diffCollapse);
  };
  const mergeView = (f: { a: string; b: string }, name: string, opts?: { editable?: boolean; refPath?: string; commitHash?: string }) => {
    const refPath = opts?.refPath ?? (file?.commit ? file.commit.rel : file?.path ?? name);
    const commitHash = opts?.commitHash ?? file?.commit?.hash;
    const editable = !!opts?.editable;
    const diffRelPath = activeReviewId
      ? normalizeReviewPath(reviewRelOf(refPath), knownReviewFiles, activeReviewInfo?.cwd ?? cwd) ?? reviewRelOf(refPath)
      : '';
    const fileIssues =
      activeReviewId && diffRelPath
        ? activeReviewSnapshot.issues.filter((it) => it.file && it.file === diffRelPath)
        : [];
    const fileIssuesById = new Map<string, ReviewIssue>();
    for (const it of fileIssues) fileIssuesById.set(it.id, it);
    const openInFileIssues = fileIssues.filter((it) => (activeDispositions[it.id] ?? 'open') === 'open');
    const stepFileIssue = (delta: number) => {
      if (!fileIssues.length) return;
      const idx = (diffIssueCursor + delta + fileIssues.length) % fileIssues.length;
      const target = fileIssues[idx];
      setDiffIssueCursor(idx);
      setShowInlineReviews(true);
      setHighlightedFindingId(target.id);
      setGoto({ line: target.line ?? 1, n: Date.now() });
    };
    return (
      <div className="wp-diff wp-diff-full">
        {activeReviewId && activeReviewInfo && fileIssues.length > 0 && (
          <div className="wp-diff-review-bar">
            <div className="wp-diff-review-left">
              <ToolBadge tool={activeReviewInfo.tool} size={13} />
              <span className="wp-diff-review-title">
                {tx('reviewFileRibbonTitle', { tool: TOOL_LABEL[activeReviewInfo.tool], n: fileIssues.length })}
              </span>
              {openInFileIssues.length > 0 && (
                <span className="wp-diff-review-open">{openInFileIssues.length}</span>
              )}
            </div>
            <div className="wp-diff-review-acts">
              {fileIssues.length > 1 && (
                <>
                  <button
                    type="button"
                    className="icon-btn"
                    title={tx('reviewPrevIssue')}
                    onClick={() => stepFileIssue(-1)}
                  >
                    <Icon name="up" size={12} />
                  </button>
                  <button
                    type="button"
                    className="icon-btn"
                    title={tx('reviewNextIssue')}
                    onClick={() => stepFileIssue(1)}
                  >
                    <Icon name="down" size={12} />
                  </button>
                </>
              )}
              <button
                type="button"
                className="wp-review-link"
                onClick={() => setShowInlineReviews((v) => !v)}
              >
                {showInlineReviews ? tx('reviewHideInline') : tx('reviewShowInline')}
              </button>
              {openInFileIssues.length > 0 && (
                <button
                  type="button"
                  className="btn small primary"
                  onClick={() => {
                    const count = adoptReviewFindings({
                      hostId,
                      reviewSessionId: activeReviewId,
                      parentSessionId: sessionId,
                      tool: activeReviewInfo.tool,
                      issues: openInFileIssues,
                      commitHash,
                    });
                    if (count > 0) toast(tx('reviewAdoptedToast', { n: count }));
                  }}
                >
                  <Icon name="check" size={11} />
                  {tx('reviewAdoptAllOpen', { n: openInFileIssues.length })}
                </button>
              )}
              <button
                type="button"
                className="btn small"
                onClick={() => void openFile(sideTabPath(activeReviewId))}
              >
                <Icon name="sideChat" size={11} />
                {tx('reviewOpenSideBtn')}
              </button>
            </div>
          </div>
        )}
        <MergeDiff
          ref={mergeRef}
          docKey={file ? `${file.path}#${compare?.path ?? ''}#${diffBaseRev}#${diffEndRev}` : name}
          a={f.a}
          b={f.b}
          filename={name}
          layout={layout}
          collapse={diffCollapse}
          wrap={wrap}
          readOnly={!editable}
          goto={goto}
          onChange={
            editable
              ? (draft) => {
                  if (file?.untitled) untitled.set(file.path, draft);
                  setFile((cur) => cur && { ...cur, draft });
                }
              : undefined
          }
          onSave={editable ? () => save() : undefined}
          onToggleWrap={toggleWrap}
          menu={editorMenu(refPath, commitHash)}
          onAddContext={standalone ? undefined : (snip) => pushContextRef(refPath, snip, commitHash)}
          onAddReview={standalone ? undefined : (snip, comment) => pushReviewComment(refPath, snip, comment, commitHash)}
          onCopyHunk={(content) => void copy(content, tx('diffCopied'))}
          inlineReviews={showInlineReviews && activeReviewId ? fileIssues.map((it) => ({ id: it.id, line: it.line ?? 1 })) : undefined}
          renderInlineReview={
            showInlineReviews && activeReviewId
              ? (fid) => {
                  const it = fileIssuesById.get(fid);
                  if (!it || !activeReviewInfo) return null;
                  return (
                    <ReviewInlineWidget
                      hostId={hostId}
                      parentSessionId={sessionId}
                      reviewSessionId={activeReviewId}
                      tool={activeReviewInfo.tool}
                      issue={it}
                      disposition={getFindingDisposition(hostId, activeReviewId, it.id)}
                      thread={activeReviewThreads.get(it.id) ?? []}
                      streamingText={activeReviewStream?.findingId === it.id ? activeReviewStream.liveText : undefined}
                      busy={activeReviewBusy}
                      connected={connected}
                      pendingPerms={activeReviewPendingPerms}
                      onAnswerPerm={answerActiveReviewPerm}
                      highlighted={highlightedFindingId === it.id}
                      commitHash={commitHash}
                      onOpenSideChat={() => void openFile(sideTabPath(activeReviewId))}
                    />
                  );
                }
              : undefined
          }
        />
      </div>
    );
  };
  /** Toolbar buttons for the full-file diff: previous / next change and collapsing unchanged regions (none for an added /
   *  deleted file, which MergeDiff shows as the single file) */
  const singleFile = !!full && oneSided(full.a, full.b);
  const mergeBtns = full && !singleFile ? (
    <>
      <button className="icon-btn" title={tx('previousChange')} onClick={() => mergeRef.current?.prev()}>
        <Icon name="up" size={14} />
      </button>
      <button className="icon-btn" title={tx('nextChange')} onClick={() => mergeRef.current?.next()}>
        <Icon name="down" size={14} />
      </button>
      <button className="icon-btn" title={diffCollapse ? tx('showUnchangedRegions') : tx('collapseUnchangedRegions')} onClick={flipCollapse}>
        <Icon name={diffCollapse ? 'unfold-all' : 'fold-all'} size={14} />
      </button>
    </>
  ) : null;
  const loadingChanges = (
    <div className="wp-empty">
      <div className="muted small">
        <span className="spinner" /> {tx('readingChanges')}
      </div>
    </div>
  );

  const aiBrowserOk = !!sessInfo && canUseBrowser(hostStatus(hostId).hello, sessInfo);
  const toggleHidden = () => (lsSet('wp-hidden', !hidden), setHidden(!hidden));
  const newMenu = (e: React.MouseEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    openMenu({ clientX: r.right - 160, clientY: r.bottom + 4 }, [
      { label: tx('newFile'), icon: 'file', run: () => root && create(root, 'file') },
      { label: tx('newFolder'), icon: 'folder', run: () => root && create(root, 'dir') },
      '-',
      { label: tx('openWebPage'), icon: 'globe', run: () => openWeb() },
      ...(aiBrowserOk ? [{ label: aiT('openAiBrowser'), icon: 'bot', run: () => openFile(AI_PREFIX + sessionId) }] : []),
    ]);
  };
  const panelBtns = standalone ? null : (
    <>
      <button className="icon-btn hide-mobile" title={fullscreen ? tx('exitFullScreen') : tx('fullScreen')} onClick={onFullscreen}>
        <Icon name={fullscreen ? 'minimize' : 'maximize'} size={14} />
      </button>
      <button className="icon-btn" title={tx('close')} onClick={closePanel}>
        <Icon name="x" size={15} />
      </button>
    </>
  );

  const viewTab = (v: 'all' | 'changes' | 'search', label: React.ReactNode, extra: Partial<React.ButtonHTMLAttributes<HTMLButtonElement>> = {}) => (
    <button role="tab" aria-selected={view === v} className={view === v ? 'active' : ''} onClick={() => pickView(v)} {...extra}>
      {label}
    </button>
  );

  const currentWorktree = worktrees.find((w) => norm(w.path) === norm(cwd));
  const wtLabel = currentWorktree?.branch || git?.branch || baseName(cwd);
  const effBranchBase = branchBase || branchDiff?.base || branchesInfo?.defaultBranch || '';
  const commitFiles = useMemo(
    () =>
      uncommittedKeys
        .map((k) => git?.files.get(k))
        .filter((f): f is GitFile => !!f)
        .map((f) => ({ rel: f.rel, status: f.status, added: f.added, deleted: f.deleted })),
    [uncommittedKeys, git],
  );

  // Changes view with nothing uncommitted: the commit history box below takes the remaining height
  const histGrow = view === 'changes' && !!git && gitReady && cRows.length === 0;
  const tree = (
    <div className="wp-side" ref={sideRef} style={file || emptyPane ? { width: treeW } : undefined}>
      <div className="wp-side-head">
        <div className="wp-views" role="tablist" aria-label={tx('fileViews')}>
          {viewTab('all', tx('files'))}
          {viewTab(
            'changes',
            <>
              {tx('changes')}
              {changed.length > 0 && <span className="wp-count">{changed.length}</span>}
            </>,
            {
              // Usable while git status is still loading (the commit history below doesn't wait for it)
              disabled: gitReady && !git && touched.size === 0,
              title: git || !gitReady
                ? tx('uncommittedChangesInTheWorkspace')
                : touched.size
                  ? tx('filesChangedByToolsIn')
                  : tx('notAGitRepoAnd'),
            },
          )}
          {viewTab('search', tx('search'), { title: tx('searchInFileContents') })}
        </div>
        <div className="grow" />
        {view !== 'changes' && (
          <>
            <button className="icon-btn" title={tx('quickOpenP', { v: navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+' })} onClick={() => setQuick('')}>
              <Icon name="zoom" size={14} />
            </button>
            <button className="icon-btn" title={tx('new')} disabled={!root} onClick={newMenu}>
              <Icon name="plus" size={15} />
            </button>
          </>
        )}
        <button className="icon-btn" title={tx('refresh')} onClick={refreshTree}>
          <Icon name="refresh" size={14} />
        </button>
      </div>
      {git && (
        <div className={`wp-ctx-bar ${isOtherWorktree ? 'other' : ''}`}>
          <button
            ref={bwBtnRef}
            type="button"
            className={`wp-ctx-branch truncate ${isOtherWorktree ? 'other' : ''} ${bwOpen ? 'open' : ''}`}
            aria-expanded={bwOpen}
            aria-haspopup="dialog"
            title={onSwitchCwd && worktrees.length > 1 ? tx('switchBranchOrWorktree') : tx('switchOrCreateBranch')}
            onClick={toggleBwPopover}
          >
            <Icon name="git-branch" size={12} />
            <span className="truncate">{git.branch || wtLabel || 'HEAD'}</span>
            {onSwitchCwd && worktrees.length > 1 && <span className="wp-ctx-cnt">· {worktrees.length}</span>}
            <Icon name="down" size={10} />
          </button>
          <div className="grow" />
          {onSwitchCwd && isOtherWorktree && baseCwd ? (
            <button
              type="button"
              className="wp-wt-back"
              title={tx('backToSessionDir')}
              onClick={() => switchWorktree(baseCwd)}
            >
              <Icon name="back" size={11} />
              <span>{tx('mainDirShort')}</span>
            </button>
          ) : (
            <span className="wp-ctx-repo truncate" title={cwd}>
              {baseName(cwd)}
            </span>
          )}
        </div>
      )}
      {onSwitchCwd && newWorktree && (
        <div className="wp-wt-notice">
          <span className="truncate" title={newWorktree.path}>
            {tx('newWorktreeDetected', { name: newWorktree.branch || baseName(newWorktree.path) })}
          </span>
          <button
            type="button"
            className="wp-wt-notice-act"
            onClick={() => {
              const target = newWorktree.path;
              setNewWorktree(null);
              onSwitchCwd(target);
            }}
          >
            {tx('switchToWorktree')}
          </button>
          <button type="button" className="icon-btn" title={tx('close')} onClick={() => setNewWorktree(null)}>
            <Icon name="x" size={11} />
          </button>
        </div>
      )}
      {view === 'search' && root ? (
        <GrepPanel
          hostId={hostId}
          root={root}
          onOpen={(r, line, col, len) => openFile(relOpen(r), false, { line, col, len })}
          openFiles={tabs.filter((t) => !isWebTab(t) && !isReviewTab(t) && !isSideTab(t) && !parseCommitTab(t) && rel(t) !== t).map(rel)}
          dirty={file && dirty && !file.web && !file.commit && !file.review && !file.side && rel(file.path) !== file.path ? { rel: rel(file.path), draft: file.draft } : null}
          includeRequest={grepInclude}
          onPreview={async (r, line, col, len, after) => {
            const abs = relOpen(r);
            await openFile(abs, false, { line, col, len });
            if (norm(fileRef.current?.path ?? '') === norm(abs)) setCompare({ path: abs, text: after, replace: true });
          }}
          onDraft={(next, expect) =>
            setFile((cur) => (cur && !cur.web && !cur.commit && (expect == null || cur.draft === expect) ? { ...cur, draft: next, rev: Date.now() } : cur))
          }
          onReplaced={(paths) => {
            refreshTree();
            const f = fileRef.current;
            if (!f || f.draft !== f.content || !paths.some((p) => norm(p) === norm(f.path))) return;
            readFile(f.path, true).then((n) => n && setFile((cur) => (cur && cur.path === f.path && cur.draft === cur.content ? n : cur)));
          }}
        />
      ) : (
        <>
          {view === 'changes' && git && (
            <div className="wp-scope-strip">
              <div className="wp-scope-tabs" role="tablist">
                <button
                  type="button"
                  role="tab"
                  aria-selected={diffScope === 'uncommitted'}
                  className={`wp-scope-tab ${diffScope === 'uncommitted' ? 'active' : ''}`}
                  onClick={() => setDiffScope('uncommitted')}
                >
                  <span>{tx('diffScopeUncommitted')}</span>
                  {uncommittedKeys.length > 0 && <span className="wp-scope-cnt">{uncommittedKeys.length}</span>}
                </button>
                <button
                  type="button"
                  role="tab"
                  aria-selected={diffScope === 'branch'}
                  className={`wp-scope-tab ${diffScope === 'branch' ? 'active' : ''}`}
                  title={diffScope === 'branch' ? tx('changeBaseBranch') : effBranchBase ? tx('diffScopeBranchVs', { base: effBranchBase }) : tx('diffScopeBranch')}
                  onClick={(e) => {
                    if (diffScope === 'branch') void openBaseBranchMenu(e);
                    else setDiffScope('branch');
                  }}
                >
                  <span className="truncate">
                    {effBranchBase ? tx('diffScopeBranchVs', { base: effBranchBase }) : tx('diffScopeBranch')}
                  </span>
                  {diffScope === 'branch' && <Icon name="down" size={10} />}
                </button>
                {turnCheckpoints.length > 0 && activeTurnCkpt && (
                  <button
                    type="button"
                    role="tab"
                    aria-selected={diffScope === 'turn'}
                    className={`wp-scope-tab ${diffScope === 'turn' ? 'active' : ''}`}
                    title={diffScope === 'turn' ? tx('changeTurnCheckpoint') : tx('diffScopeTurnN', { n: activeTurnCkpt.turn })}
                    onClick={(e) => {
                      if (diffScope === 'turn' && turnCheckpoints.length > 1) openTurnMenu(e);
                      else setDiffScope('turn');
                    }}
                  >
                    <span>{tx('diffScopeTurnN', { n: activeTurnCkpt.turn })}</span>
                    {diffScope === 'turn' && turnCheckpoints.length > 1 && <Icon name="down" size={10} />}
                  </button>
                )}
              </div>
              {totals && changed.length > 0 && <LineStat a={totals.a} d={totals.d} />}
            </div>
          )}
          {view === 'all' && (
            <div className="wp-filter">
              <Icon name="search" size={13} />
              <input
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
                placeholder={tx('filterByFileName')}
                aria-label={tx('filterByFileName2')}
                onKeyDown={(e) => e.key === 'Escape' && setFilter('')}
              />
              {filter && (
                <button className="icon-btn" title={tx('clear')} onClick={() => setFilter('')}>
                  <Icon name="x" size={12} />
                </button>
              )}
            </div>
          )}
          <div
            className={`wp-tree ${dropDir === root ? 'drop' : ''} ${histGrow ? 'shrink' : ''}`}
            role="tree"
            aria-label={tx('fileTree')}
            tabIndex={0}
            ref={treeBox}
            {...treeDrop}
            onPointerDownCapture={(e) => {
              const row = (e.target as Element).closest<HTMLElement>('[data-path]');
              if (row) {
                lastRow.current = { path: row.dataset.path!, isDir: row.dataset.dir === '1' };
                setFocusKey(row.dataset.key ?? null);
              }
            }}
            onKeyDown={treeKey}
            onContextMenu={(e) =>
              root &&
              !q &&
              openMenu(e, [
                { label: tx('newFile'), icon: 'plus', run: () => create(root, 'file') },
                { label: tx('newFolder'), icon: 'folder', run: () => create(root, 'dir') },
                '-',
                { label: hidden ? tx('hideHiddenFiles') : tx('showHiddenFiles'), icon: 'eye', run: toggleHidden },
                { label: tx('copyDirectoryPath'), icon: 'copy', run: () => copy(root) },
                ...(isLocal ? [{ label: tx('showInFileManager'), icon: 'folder', run: () => openNative(root, false) }] : []),
              ])
            }
          >
            {view === 'all' && q ? (
              !search || search.q !== q ? (
                <div className="wp-tip">{tx('searching')}</div>
              ) : search.error ? (
                <div className="wp-tip">
                  <b>{tx('cannotSearchFilesRightNow')}</b>
                  {tx('tryAgainLaterOrClear')}
                </div>
              ) : sRows.length === 0 ? (
                <div className="wp-tip">
                  <b>{tx('noMatchingFiles')}</b>
                  {tx('tryAnotherTermOrSwitch')}
                </div>
              ) : (
                <>
                  <Rows
                    rows={sRows}
                    scroller={treeBox}
                    git={git}
                    selected={file?.path}
                    focused={focusKey}
                    onToggle={toggleCollapsed}
                    onOpen={(r) => openFile(r.path)}
                    onMenu={rowMenu}
                    onDragRow={dragRow}
                  />
                  {search.truncated && <div className="wp-limit">{tx('showingTheFirstResultsNarrow', { length: search.paths.length })}</div>}
                </>
              )
            ) : view === 'changes' &&
              (!gitReady ||
                (diffScope === 'branch' && branchDiffLoading && !branchDiff) ||
                (diffScope === 'turn' && turnDiffLoading && !turnDiff)) ? (
              <div className="wp-tip">{tx('readingChanges')}</div>
            ) : view === 'changes' && (git || touched.size > 0) ? (
              <>
                {!git && <div className="wp-limit">{tx('notAGitRepoFiles')}</div>}
                {activeChangesGit?.total && (
                  <div className="wp-limit">
                    {tx('tooManyChangesShowingThe', { total: activeChangesGit.total, size: activeChangesGit.files.size })}
                  </div>
                )}
                {!git && totals && changed.length > 0 && (
                  <div className="wp-totals">
                    {tx('files2', { length: changed.length })}
                    <LineStat a={totals.a} d={totals.d} />
                  </div>
                )}
                {!standalone && activeReviewInfo && (
                  <div className="wp-review-ov-bar">
                    <button
                      type="button"
                      className="wp-review-ov-main"
                      onClick={() => void openFile(sideTabPath(activeReviewInfo.id))}
                      title={activeReviewMeta?.scopeLabel ?? tx('reviewTabPrefix')}
                    >
                      <ToolBadge tool={activeReviewInfo.tool} size={13} />
                      {activeReviewBusy ? (
                        <span className="wp-review-ov-stat">
                          <span className="spinner mini" />
                          <span>{tx('reviewOverviewRunning')}</span>
                        </span>
                      ) : activeReviewSnapshot.hasStructuredBlock ? (
                        activeReviewTotals.total > 0 ? (
                          <span className="wp-review-ov-counts">
                            {activeReviewTotals.high > 0 && <span className="wp-review-sev sev-high">{activeReviewTotals.high}</span>}
                            {activeReviewTotals.medium > 0 && <span className="wp-review-sev sev-medium">{activeReviewTotals.medium}</span>}
                            {activeReviewTotals.low > 0 && <span className="wp-review-sev sev-low">{activeReviewTotals.low}</span>}
                          </span>
                        ) : (
                          <span className="wp-review-ov-clean">{tx('reviewNoIssuesFound')}</span>
                        )
                      ) : (
                        <span className="truncate muted small">{activeReviewMeta?.scopeLabel ?? tx('reviewTabPrefix')}</span>
                      )}
                    </button>
                    <div className="wp-review-ov-acts">
                      {activeReviewTotals.total > 0 && (
                        <button
                          type="button"
                          className={`wp-review-chip ${reviewOnlyFilter ? 'active' : ''}`}
                          onClick={() => setReviewOnlyFilter((v) => !v)}
                        >
                          {tx('reviewOnlyWithIssues')}
                        </button>
                      )}
                      {reviewSessions.length > 1 && (
                        <button
                          type="button"
                          className="icon-btn"
                          title={tx('reviewSwitchRun')}
                          onClick={(e) => {
                            const r = e.currentTarget.getBoundingClientRect();
                            openMenu(
                              { clientX: r.right - 220, clientY: r.bottom + 4 },
                              reviewSessions.map((rs) => ({
                                label: sideChatTitle(rs),
                                icon: 'sparkles',
                                active: rs.id === activeReviewId,
                                hint: rs.review?.scopeLabel ?? TOOL_LABEL[rs.tool],
                                run: () => setActiveReviewId(hostId, sessionId, rs.id),
                              })),
                            );
                          }}
                        >
                          <Icon name="down" size={11} />
                        </button>
                      )}
                    </div>
                  </div>
                )}
                {cRows.length ? (
                  <Rows
                    rows={cRows}
                    scroller={treeBox}
                    stats
                    git={activeChangesGit}
                    selected={file?.path}
                    focused={focusKey}
                    onToggle={toggleCollapsed}
                    onOpen={(r) => openFile(r.path, true)}
                    onMenu={rowMenu}
                    onDragRow={dragRow}
                    reviewBadges={activeReviewId ? fileIssueBadges : undefined}
                    reviewRelOf={activeReviewId ? reviewRelOf : undefined}
                    reviewCleanCheck={activeReviewId ? reviewCleanCheck : undefined}
                  />
                ) : reviewOnlyFilter && changed.length > 0 ? (
                  <div className="wp-tip">
                    <b>{tx('reviewNoIssuesUnderFilter')}</b>
                    <button type="button" className="wp-review-link" onClick={() => setReviewOnlyFilter(false)}>
                      {tx('reviewShowAllChanged')}
                    </button>
                  </div>
                ) : (
                  <div className="wp-tip">
                    <b>{tx('noChanges')}</b>
                    {diffScope === 'branch' && effBranchBase
                      ? tx('branchMatchesBase', { base: effBranchBase })
                      : tx('workingTreeMatchesTheLast')}
                  </div>
                )}
              </>
            ) : root ? (
              rows.length ? (
                <Rows
                  rows={rows}
                  scroller={treeBox}
                  git={git}
                  selected={file?.path}
                  focused={focusKey}
                  onToggle={(r) => toggle(r.path)}
                  onOpen={(r) => openFile(r.path)}
                  onMenu={rowMenu}
                  onDragRow={dragRow}
                  dropDir={dropDir}
                  onDropOver={(d) => dropDir !== d && setDropDir(d)}
                />
              ) : (
                <div className="wp-tip">{tx('thisFolderIsEmpty')}</div>
              )
            ) : (
              <div className="wp-tip">{tx('loading2')}</div>
            )}
          </div>
          {/* Independent of git status: shows right away and loads on its own; hidden only once status says this isn't a repo */}
          {view === 'changes' && (!gitReady || git) && (
            <GitHistory
              hostId={hostId}
              cwd={cwd}
              head={git?.head}
              selected={file?.path}
              grow={histGrow}
              onOpen={(t) => openFile(t)}
              onReviewCommit={standalone ? undefined : (preset) => setReviewPreset(preset)}
            />
          )}
          {view === 'changes' && git && (
            <div className="wp-git-dock">
              <div className="wp-git-dock-cta">
                {commitFiles.length > 0 ? (
                  <>
                    <button type="button" className="wp-dock-btn primary" title={tx('commitChanges')} onClick={() => setCommitOpen(true)}>
                      <Icon name="git-commit" size={13} />
                      <span>{tx('commitCount', { n: commitFiles.length })}</span>
                    </button>
                    {!standalone && (
                      <button type="button" className="wp-dock-btn ai" title={tx('crossToolReviewTitle')} onClick={() => setReviewPreset({})}>
                        <Icon name="sparkles" size={13} />
                        <span>{tx('reviewBtn')}</span>
                      </button>
                    )}
                    <button
                      type="button"
                      className="wp-dock-icon"
                      disabled={pushing}
                      title={tx('pushBranch')}
                      onClick={() => void doPush()}
                    >
                      {pushing ? <span className="spinner mini" /> : <Icon name="upload" size={13} />}
                    </button>
                  </>
                ) : (
                  <>
                    {!standalone && (
                      <button type="button" className="wp-dock-btn ai" title={tx('crossToolReviewTitle')} onClick={() => setReviewPreset({})}>
                        <Icon name="sparkles" size={13} />
                        <span>{tx('reviewBtn')}</span>
                      </button>
                    )}
                    <button type="button" className="wp-dock-btn" disabled={pushing} title={tx('pushBranch')} onClick={() => void doPush()}>
                      {pushing ? <span className="spinner mini" /> : <Icon name="upload" size={13} />}
                      <span>{tx('pushBtn')}</span>
                    </button>
                  </>
                )}
              </div>
            </div>
          )}
          {(dropDir || uploading > 0) && (
            <div className="wp-dropbar">
              {uploading > 0 ? (
                <>
                  <span className="spinner" /> {tx('uploadingFiles', { uploading })}
                </>
              ) : (
                <>
                  <Icon name="upload" size={13} /> {tx('dropToUploadTo', { v: rel(dropDir!) })}
                </>
              )}
            </div>
          )}
        </>
      )}
    </div>
  );

  const editor = file && (
    <CodeEditor
      docKey={`${file.path}#${file.rev}`}
      value={file.draft}
      filename={baseName(file.path)}
      readOnly={file.truncated || !!file.notUtf8}
      wrap={wrap || (md && mdSplit && wide())}
      goto={goto}
      onChange={(draft) => {
        if (file.untitled) untitled.set(file.path, draft);
        setFile((cur) => cur && { ...cur, draft });
      }}
      onSave={() => save()}
      menu={editorMenu(file.path)}
      onToggleWrap={toggleWrap}
      onAddSelection={
        standalone
          ? undefined
          : (sel) => pushContextRef(file.path, { kind: 'code', fromLine: sel.fromLine, toLine: sel.toLine, content: sel.content })
      }
      scrollLine={md && mdSplit ? editorAt : null}
      onTopLine={md && mdSplit ? (line, frac) => syncFrom.current === 'editor' && setPreviewAt({ line, frac, n: Date.now() }) : undefined}
    />
  );

  const paneDiffActions = (refPath: string, commitHash?: string) => ({
    onAddHunk: standalone ? undefined : (h: { fromLine: number; toLine: number; added?: number; deleted?: number; content: string }) => pushContextRef(refPath, { kind: 'diff', ...h }, commitHash),
    onCopyHunk: (content: string) => void copy(content, tx('diffCopied')),
  });

  const openReviewFile = (reviewSid: string, relPath: string, line?: number) => {
    const cleanRel = relPath.replace(/\\/g, '/').replace(/^\.\//, '');
    const rmeta = resolveReviewMeta(hostSessions.find((x) => x.id === reviewSid), getReviewMeta(reviewSid));
    if (rmeta?.scope === 'commits' && rmeta.commits?.length === 1) {
      void openFile(commitTab(rmeta.commits[0], cleanRel), true, line ? { line } : undefined);
      return;
    }
    if (rmeta?.baseRev && rmeta?.endRev) {
      void openFile(commitRangeTab(rmeta.baseRev, rmeta.endRev, cleanRel), true, line ? { line } : undefined);
      return;
    }
    if (rmeta?.scope === 'uncommitted') {
      setDiffScope('uncommitted');
    } else if (rmeta?.scope === 'branch' && rmeta.includeUncommitted && rmeta.base) {
      setBranchBase(rmeta.base);
      setDiffScope('branch');
    }
    const abs = git ? join(git.repo, cleanRel.split('/').join(sepOf(git.repo))) : relOpen(cleanRel);
    void openFile(abs, true, line ? { line } : undefined);
  };

  // Content of the editor column: file / changes / compare / review / side chat
  const mainView = !file ? null : file.side ? (
    <SideChatPane
      key={sideTabSession(file.path)}
      hostId={hostId}
      parentSessionId={sessionId}
      sideSessionId={sideTabSession(file.path)}
      onOpenFile={(relPath, line) => openReviewFile(sideTabSession(file.path), relPath, line)}
      onDeleted={() => void closeTab(file.path)}
    />
  ) : file.review ? (
    <ReviewPane
      key={reviewTabSession(file.path)}
      hostId={hostId}
      reviewSessionId={reviewTabSession(file.path)}
      parentSessionId={sessionId}
      onOpenFile={(relPath, line) => openReviewFile(reviewTabSession(file.path), relPath, line)}
    />
  ) : file.commit ? (
    full ? (
      mergeView(full, baseName(file.commit.rel), { refPath: file.commit.rel, commitHash: file.commit.hash })
    ) : full === undefined ? (
      loadingChanges
    ) : file.commit.diff ? (
      <div className="wp-diff">
        <PaneDiff
          diff={{ path: file.commit.rel, unified: file.commit.diff.slice(Math.max(0, file.commit.diff.indexOf('@@'))) }}
          layout={layout}
          {...paneDiffActions(file.commit.rel, file.commit.hash)}
        />
      </div>
    ) : (
      <div className="wp-empty">
        <div className="wp-empty-title">{tx('noChangesToShow')}</div>
        <div className="muted small">{tx('fileIsTooLargeBinary')}</div>
      </div>
    )
  ) : file.web && isAiTab(file.path) ? (
    <AiBrowserPane hostId={hostId} sessionId={aiTabSession(file.path)} />
  ) : file.web ? (
    <BrowserPane hostId={hostId} root={root ?? cwd} tab={file.path} onNavigate={(t) => navigateWeb(file.path, t)} sessionId={sessionId} onOpenTab={(t) => openFile(t)} />
  ) : html && preview && viewing === 'file' ? (
    <BrowserPane hostId={hostId} root={root ?? cwd} tab={`${WEB_PREFIX}file:${file.path}`} reload={file.modified} onNavigate={(t) => openFile(t)} />
  ) : viewing === 'compare' && full ? (
    mergeView(full, baseName(file.path), { editable: !file.truncated && !file.notUtf8 && !file.binary, refPath: file.path })
  ) : viewing === 'compare' ? (
    <div className="wp-diff">
      <PaneDiff diff={{ path: rel(file.path), oldText: file.draft, newText: compare!.text }} layout={layout} {...paneDiffActions(file.path)} />
    </div>
  ) : viewing === 'diff' ? (
    full ? (
      mergeView(full, baseName(file.path), {
        editable: !isTurnSnapshot && !file.deleted && !file.truncated && !file.notUtf8 && !file.binary,
        refPath: file.path,
      })
    ) : full === undefined ? (
      loadingChanges
    ) : fileDiff ? (
      <div className="wp-diff">
        <PaneDiff diff={{ ...fileDiff, path: rel(file.path) }} layout={layout} {...paneDiffActions(file.path)} />
      </div>
    ) : diffPending ? (
      <div className="wp-empty">
        <div className="muted small">
          <span className="spinner" /> {tx('readingChanges')}
        </div>
      </div>
    ) : fileSessionDiffs.length ? (
      <div className="wp-diff">
        <div className="wp-limit">{tx('notAGitRepoBelow', { length: fileSessionDiffs.length })}</div>
        {fileSessionDiffs.map((d, i) => (
          <PaneDiff key={i} diff={{ ...d, path: rel(file.path) }} layout={layout} {...paneDiffActions(file.path)} />
        ))}
      </div>
    ) : (
      <div className="wp-empty">
        <div className="wp-empty-title">{tx('noChangesToShow')}</div>
        <div className="muted small">{tx('fileIsTooLargeOr')}</div>
      </div>
    )
  ) : file.image ? (
    <ImageView key={`${file.path}#${file.rev}`} src={file.image} name={baseName(file.path)} size={fmtSize(file.size)} />
  ) : file.pdf ? (
    <PdfView base64={file.pdf} onDownload={() => download(file.path)} />
  ) : file.binary ? (
    <div className="wp-empty">
      <div className="wp-empty-title">{tx('cannotPreviewThisFile')}</div>
      <div className="muted small">{tx('formatEncodingOrSizeIs', { v: fmtSize(file.size) })}</div>
      <div className="row wp-empty-acts">
        <button className="btn small" onClick={() => download(file.path)}>
          <Icon name="download" size={13} /> {tx('download')}
        </button>
        {isLocal && (
          <button className="btn small" onClick={() => openNative(file.path, false)}>
            {tx('openWithSystemApp')}
          </button>
        )}
      </div>
    </div>
  ) : md && preview ? (
    <MdPreview hostId={hostId} path={file.path} text={file.draft} onOpen={(p) => openFile(p)} goto={mdGoto} />
  ) : md && mdSplit && wide() ? (
    <div className="wp-md-split">
      <div className="wp-md-half" onPointerEnter={() => (syncFrom.current = 'editor')} onFocus={() => (syncFrom.current = 'editor')}>
        {editor}
      </div>
      <div
        className="wp-md-half"
        onPointerEnter={() => (syncFrom.current = 'preview')}
        onTouchStart={() => (syncFrom.current = 'preview')}
        onWheel={() => (syncFrom.current = 'preview')}
      >
        <MdPreview
          hostId={hostId}
          path={file.path}
          text={file.draft}
          onOpen={(p) => openFile(p)}
          syncTo={previewAt}
          goto={mdGoto}
          onTopLine={(line, frac) => syncFrom.current === 'preview' && setEditorAt({ line, frac, n: Date.now() })}
        />
      </div>
    </div>
  ) : csv && preview ? (
    <CsvView text={file.draft} tsv={extOf(file.path) === 'tsv'} />
  ) : (
    editor
  );

  const secondPane = second && file && viewing === 'file' && wide() && (
    <div className="wp-second">
      <div className="wp-second-head">
        <FileIcon name={baseName(second.path)} size={13} />
        <span className="truncate" title={second.path}>
          {rel(second.path)}
        </span>
        <span className="muted small">{tx('readOnly')}</span>
        <div className="grow" />
        <button className="icon-btn" title={tx('compareWith', { v: baseName(file.path) })} onClick={() => setCompare({ path: second.path, text: second.content })}>
          <Icon name="compare" size={13} />
        </button>
        <button className="icon-btn" title={tx('editOnTheLeft')} onClick={() => (setSecond(null), openFile(second.path))}>
          <Icon name="back" size={13} />
        </button>
        <button className="icon-btn" title={tx('close')} onClick={() => setSecond(null)}>
          <Icon name="x" size={13} />
        </button>
      </div>
      {second.binary ? (
        <div className="wp-empty">
          <div className="wp-empty-title">{tx('notATextFile')}</div>
        </div>
      ) : (
        <CodeEditor
          docKey={`2:${second.path}#${second.rev}`}
          value={second.content}
          filename={baseName(second.path)}
          readOnly
          wrap={wrap}
          onChange={() => {}}
          onSave={() => {}}
          menu={editorMenu(second.path)}
          onToggleWrap={toggleWrap}
          onAddSelection={
            standalone
              ? undefined
              : (sel) => pushContextRef(second.path, { kind: 'code', fromLine: sel.fromLine, toLine: sel.toLine, content: sel.content })
          }
        />
      )}
    </div>
  );

  return (
    <aside
      className={`file-panel ${fullscreen ? 'full' : ''} ${file || emptyPane ? '' : 'browse'} ${standalone ? 'standalone' : ''}`}
      style={{ '--tree-w': `${treeW}px` } as React.CSSProperties}
    >
      {!file ? (
        <div className="wp-top">
          <Icon name="folder" size={14} />
          <span className="wp-title truncate" title={root ?? cwd}>
            {baseName(root ?? cwd) || root || cwd}
          </span>
          <div className="grow" />
          <button
            className="icon-btn"
            title={canSideChat ? tx('sideChatPlusTip') : tx('openWebPageNewBrowser')}
            onClick={canSideChat ? openPlusMenu : () => openWeb()}
          >
            <Icon name={canSideChat ? 'plus' : 'globe'} size={14} />
          </button>
          {panelBtns}
        </div>
      ) : (
        <div
          className="wp-top wp-tabbar"
          onDoubleClick={(e) => e.target === e.currentTarget && newUntitled()}
        >
          <div
            className={`wp-filetabs ${tabsMoreRight ? 'fade-r' : ''}`}
            role="tablist"
            aria-label={tx('openFiles')}
            ref={tabsRef}
            onScroll={measureTabs}
            // Like VS Code: vertical wheel scrolling scrolls the tabs horizontally
            onWheel={(e) => {
              if (Math.abs(e.deltaY) > Math.abs(e.deltaX)) e.currentTarget.scrollLeft += e.deltaY;
            }}
            // Like VS Code: double-clicking the empty part of the tab bar opens a new untitled file
            onDoubleClick={(e) => e.target === e.currentTarget && newUntitled()}
          >
            {tabs.map((t) => {
              const cur = t === file.path;
              if (isSideTab(t)) {
                const sinfo = sideTabInfo(t);
                const sattn = (sinfo?.pendingPermissions ?? 0) > 0 || sinfo?.status === 'waiting-permission';
                const sbusy = sinfo?.status === 'running' || sinfo?.status === 'starting';
                const isRevSide = !!(sinfo?.review || getReviewMeta(sideTabSession(t)));
                const label = sideTabLabel(t);
                return (
                  <div
                    key={t}
                    role="tab"
                    aria-selected={cur}
                    className={`wp-ftab wp-ftab-side ${cur ? 'active' : ''}`}
                    title={label}
                    onClick={() => openFile(t)}
                    onAuxClick={(e) => e.button === 1 && closeTab(t)}
                    onContextMenu={(e) =>
                      openMenu(e, [
                        { label: tx('close'), icon: 'x', run: () => closeTab(t) },
                        ...closeItems(t),
                      ])
                    }
                  >
                    {sattn ? <span className="wp-side-dot attn" /> : sbusy ? <span className="wp-side-dot running" /> : <Icon name={isRevSide ? 'sparkles' : 'sideChat'} size={13} />}
                    <span className="truncate">{label}</span>
                    <button
                      className="wp-ftab-x"
                      title={tx('close')}
                      onClick={(e) => {
                        e.stopPropagation();
                        closeTab(t);
                      }}
                    >
                      <Icon name="x" size={11} />
                    </button>
                  </div>
                );
              }
              if (isReviewTab(t))
                return (
                  <div
                    key={t}
                    role="tab"
                    aria-selected={cur}
                    className={`wp-ftab ${cur ? 'active' : ''}`}
                    title={reviewTabLabel(t)}
                    onClick={() => openFile(t)}
                    onAuxClick={(e) => e.button === 1 && closeTab(t)}
                    onContextMenu={(e) =>
                      openMenu(e, [
                        { label: tx('close'), icon: 'x', run: () => closeTab(t) },
                        ...closeItems(t),
                      ])
                    }
                  >
                    <Icon name="sparkles" size={13} />
                    <span className="truncate">{reviewTabLabel(t)}</span>
                    <button
                      className="wp-ftab-x"
                      title={tx('close')}
                      onClick={(e) => {
                        e.stopPropagation();
                        closeTab(t);
                      }}
                    >
                      <Icon name="x" size={11} />
                    </button>
                  </div>
                );
              const ct = parseCommitTab(t);
              if (ct)
                return (
                  <div
                    key={t}
                    role="tab"
                    aria-selected={cur}
                    className={`wp-ftab ${cur ? 'active' : ''}`}
                    title={`${ct.rel}\n${ct.baseHash ? `${tx('reviewRangeReadOnly')} ${ct.baseHash.slice(0, 7)}..${ct.hash.slice(0, 7)}` : `${tx('commit')} ${ct.hash.slice(0, 10)}`}${cur && file.commit && !file.commit.baseHash ? ` · ${file.commit.subject}` : ''}`}
                    onClick={() => openFile(t)}
                    onAuxClick={(e) => e.button === 1 && closeTab(t)}
                    onContextMenu={(e) =>
                      openMenu(e, [
                        { label: tx('close'), icon: 'x', run: () => closeTab(t) },
                        ...closeItems(t),
                        '-',
                        { label: tx('copyCommitHash'), icon: 'copy', run: () => copy(ct.baseHash ? `${ct.baseHash}..${ct.hash}` : ct.hash, tx('commitHashCopied')) },
                      ])
                    }
                  >
                    <Icon name="diff" size={13} />
                    <span className="truncate">{baseName(ct.rel)}</span>
                    <span className="wp-ftab-hash">{ct.baseHash ? `${ct.baseHash.slice(0, 7)}..${ct.hash.slice(0, 7)}` : ct.hash.slice(0, 7)}</span>
                    <button
                      className="wp-ftab-x"
                      title={tx('close')}
                      onClick={(e) => {
                        e.stopPropagation();
                        closeTab(t);
                      }}
                    >
                      <Icon name="x" size={11} />
                    </button>
                  </div>
                );
              if (isUntitled(t)) {
                const unsaved = cur ? dirty : !!untitled.get(t);
                return (
                  <div
                    key={t}
                    role="tab"
                    aria-selected={cur}
                    className={`wp-ftab ${cur ? 'active' : ''}`}
                    title={nameOf(t)}
                    onClick={() => openFile(t)}
                    onAuxClick={(e) => e.button === 1 && closeTab(t)}
                    onContextMenu={(e) =>
                      openMenu(e, [
                        { label: tx('close'), icon: 'x', run: () => closeTab(t) },
                        ...closeItems(t),
                      ])
                    }
                  >
                    <Icon name="file" size={13} />
                    <span className="truncate">{nameOf(t)}</span>
                    <button
                      className={`wp-ftab-x ${unsaved ? 'dirty' : ''}`}
                      title={tx('close')}
                      onClick={(e) => {
                        e.stopPropagation();
                        closeTab(t);
                      }}
                    >
                      <Icon name="x" size={11} />
                    </button>
                  </div>
                );
              }
              if (isWebTab(t))
                return (
                  <div
                    key={t}
                    role="tab"
                    aria-selected={cur}
                    className={`wp-ftab ${cur ? 'active' : ''}`}
                    title={isAiTab(t) ? webTabTitle(t) : t.slice(WEB_PREFIX.length) || tx('newTab')}
                    onClick={() => openFile(t)}
                    onAuxClick={(e) => e.button === 1 && closeTab(t)}
                    onContextMenu={(e) =>
                      openMenu(e, [
                        { label: tx('close'), icon: 'x', run: () => closeTab(t) },
                        ...closeItems(t),
                      ])
                    }
                  >
                    {isAiTab(t) ? <AiTabIcon hostId={hostId} sessionId={aiTabSession(t)} /> : <Icon name="globe" size={13} />}
                    <span className="truncate">{webTabTitle(t)}</span>
                    <button
                      className="wp-ftab-x"
                      title={tx('close')}
                      onClick={(e) => {
                        e.stopPropagation();
                        closeTab(t);
                      }}
                    >
                      <Icon name="x" size={11} />
                    </button>
                  </div>
                );
              const g = gitOf(t);
              const mark = g ? gitMark(g.status) : null;
              const ignored = !mark && isGitIgnored(git, t);
              return (
                <div
                  key={t}
                  role="tab"
                  aria-selected={cur}
                  className={`wp-ftab ${cur ? 'active' : ''} ${mark ? mark[1] : ignored ? 'g-ign' : ''}`}
                  title={ignored ? `${t}\n${tx('ignored')}` : t}
                  draggable
                  onDragStart={(e) => {
                    e.dataTransfer.setData(DRAG_REF, `@${rel(t)} `);
                    e.dataTransfer.setData('text/plain', t);
                  }}
                  onClick={() => openFile(t)}
                  onAuxClick={(e) => e.button === 1 && closeTab(t)}
                  onContextMenu={(e) =>
                    openMenu(e, [
                      ...(!cur && wide()
                        ? [
                            { label: tx('openToTheSide'), icon: 'columns', run: () => openSecond(t) },
                            { label: tx('compareWith', { v: baseName(file.path) }), icon: 'compare', run: () => compareWith(t) },
                            '-' as const,
                          ]
                        : []),
                      { label: tx('close'), icon: 'x', run: () => closeTab(t) },
                      ...closeItems(t),
                      '-',
                      { label: tx('download'), icon: 'download', run: () => download(t) },
                      { label: tx('copyPasteInInputTo'), icon: 'copy', run: () => copyRef(t, false) },
                      { label: tx('copyRelativePath'), run: () => copy(rel(t)) },
                    ])
                  }
                >
                  <FileIcon name={baseName(t)} size={13} />
                  <span className="truncate">{baseName(t)}</span>
                  <button
                    className={`wp-ftab-x ${cur && dirty ? 'dirty' : ''}`}
                    title={tx('close')}
                    onClick={(e) => {
                      e.stopPropagation();
                      closeTab(t);
                    }}
                  >
                    <Icon name="x" size={11} />
                  </button>
                </div>
              );
            })}
          </div>
          <button
            className="icon-btn wp-tab-new"
            title={tx('newUntitledFileTip')}
            onClick={newUntitled}
          >
            <Icon name="plus" size={14} />
          </button>
          {tabsOverflow && (
            <button className="icon-btn wp-tabs-more" title={tx('allOpenFiles', { length: tabs.length })} onClick={tabsMenu}>
              <Icon name="down" size={14} />
            </button>
          )}
          <button
            className="icon-btn"
            title={canSideChat ? tx('sideChatOrWebTip') : tx('openWebPageNewBrowser')}
            onClick={canSideChat ? openPlusMenu : () => openWeb()}
          >
            <Icon name={canSideChat ? 'sideChat' : 'globe'} size={14} />
          </button>
          <button
            className={`icon-btn ${treeOpen ? 'active' : ''}`}
            title={treeOpen ? tx('hideFileTree') : tx('showFileTree')}
            onClick={() => setTree(!treeOpen)}
          >
            <Icon name="fileTree" size={14} />
          </button>
          {panelBtns}
        </div>
      )}

      {file?.commit && (
        <div className="wp-head">
          {file.commit.baseHash ? (
            <div className="wp-commit-title truncate" title={tx('reviewRangeReadOnly')}>
              <span className="pd-tag">
                {`${file.commit.baseHash.slice(0, 7)}..${file.commit.hash.slice(0, 7)}`}
              </span>
              <span className="muted truncate">{file.commit.rel}</span>
            </div>
          ) : (
            <button
              className="wp-commit-title truncate"
              aria-expanded={commitInfo}
              title={tx('showCommitDetails')}
              onClick={() => setCommitInfo(!commitInfo)}
            >
              <span className="pd-tag">{file.commit.hash.slice(0, 7)}</span>
              <span className="truncate">{file.commit.subject}</span>
              <span className="muted truncate">{file.commit.rel}</span>
              <span className={`chev ${commitInfo ? 'open' : ''}`}>
                <Icon name="chevron" size={12} />
              </span>
            </button>
          )}
          <div className="wp-actions">
            <span className="hide-mobile">{tx('readOnly')}</span>
            {mergeBtns}
            {wide() && !singleFile && (
              <button className="icon-btn" title={layout === 'split' ? tx('switchToStacked') : tx('switchToSideBySide')} onClick={flipLayout}>
                <Icon name={layout === 'split' ? 'rows' : 'columns'} size={14} />
              </button>
            )}
            {git && !gitMark(file.commit.status)[0].includes('D') && (
              <button
                className="icon-btn"
                title={tx('openThisFileInThe')}
                onClick={() => openFile(join(git.repo, file.commit!.rel.split('/').join(sepOf(git.repo))))}
              >
                <Icon name="file" size={14} />
              </button>
            )}
          </div>
        </div>
      )}
      {file?.commit && !file.commit.baseHash && commitInfo && (
        <div className="wp-commit-detail">
          <CommitDetails hash={file.commit.hash} author={file.commit.author} time={file.commit.time} message={file.commit.message} />
        </div>
      )}
      {file && !file.web && !file.commit && !file.review && !file.side && (
        <div className="wp-head">
          {compare ? (
            <div className="wp-compare-title truncate" title={`${file.path}\n${compare.path}`}>
              <span className="pd-tag del">{baseName(file.path)}</span>
              <Icon name="forward" size={12} />
              <span className="pd-tag add">{compare.replace ? tx('afterReplace') : rel(compare.path)}</span>
            </div>
          ) : (
            file.untitled ? (
              <span className="wp-untitled-title truncate">{nameOf(file.path)}</span>
            ) : (
              <Breadcrumbs hostId={hostId} root={root ?? cwd} path={file.path} hidden={hidden} git={git} onOpen={openFile} dirty={dirty} />
            )
          )}
          <div className="wp-actions">
            {state && (
              <span className={state === 'error' || state === 'conflict' ? 'wp-err' : ''} title={saveError ? trErr(saveError) : undefined}>
                {saveStateText(state)}
              </span>
            )}
            {file.truncated && <span>{tx('readOnlyFirst1mb')}</span>}
            {!file.truncated && file.notUtf8 && (
              <span title={tx('shownAsUtf8With')}>
                {tx('readOnlyNotUtf8')}
              </span>
            )}
            {dirty &&
              (saveState === 'conflict' ? (
                <button className="btn small primary" onClick={conflictMenu}>
                  {tx('resolveConflict')}
                </button>
              ) : (
                <button className="btn small primary" disabled={saveState === 'saving'} onClick={() => save()} title={tx('saveCtrlS')}>
                  <Icon name="save" size={13} /> {tx('save')}
                </button>
              ))}
            {compare && (
              <button className="btn small" onClick={() => setCompare(null)}>
                {tx('endCompare')}
              </button>
            )}
            {hasChange(file.path) && !file.deleted && !compare && (
              <div className="seg wp-seg">
                <button className={!showDiff ? 'active' : ''} onClick={() => setShowDiff(false)}>
                  {tx('file')}
                </button>
                <button className={showDiff ? 'active' : ''} onClick={() => setShowDiff(true)} title={tx('changesSinceTheLastCommit')}>
                  {tx('changes')}
                </button>
              </div>
            )}
            {viewing !== 'file' && mergeBtns}
            {viewing !== 'file' && wide() && !singleFile && (
              <button className="icon-btn" title={layout === 'split' ? tx('switchToStacked') : tx('switchToSideBySide')} onClick={flipLayout}>
                <Icon name={layout === 'split' ? 'rows' : 'columns'} size={14} />
              </button>
            )}
            {(csv || html) && viewing === 'file' && (
              <div className="seg wp-seg">
                <button className={!preview ? 'active' : ''} onClick={() => setPreview(false)}>
                  {tx('source')}
                </button>
                <button
                  className={preview ? 'active' : ''}
                  onClick={() => setPreview(true)}
                  title={html ? tx('showsTheSavedFileRefreshes') : undefined}
                >
                  {csv ? tx('table') : tx('preview')}
                </button>
              </div>
            )}
            {md && viewing === 'file' && (
              <div className="seg wp-seg">
                <button className={!preview && !(mdSplit && wide()) ? 'active' : ''} onClick={() => pickMdMode('source')}>
                  {tx('source')}
                </button>
                {wide() && (
                  <button className={!preview && mdSplit ? 'active' : ''} onClick={() => pickMdMode('split')} title={tx('editOnTheLeftLive')}>
                    {tx('split')}
                  </button>
                )}
                <button className={preview ? 'active' : ''} onClick={() => pickMdMode('preview')}>
                  {tx('preview')}
                </button>
              </div>
            )}
            {viewing === 'file' && !file.binary && (!preview || md) && (
              <button className="icon-btn" title={tx('outlineGoToAFunction', { v: navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+' })} onClick={() => setQuick('@')}>
                <Icon name="outline" size={14} />
              </button>
            )}
            <button className="icon-btn" title={tx('moreFileActions')} onClick={fileMenu}>
              <Icon name="more" size={16} />
            </button>
          </div>
        </div>
      )}

      <div className="wp-body" ref={bodyRef}>
        {file && (
          <div className="wp-main">
            {secondPane ? (
              <div className="wp-panes">
                <div className="wp-pane">{mainView}</div>
                {secondPane}
              </div>
            ) : (
              mainView
            )}
          </div>
        )}
        {emptyPane && (
          <div className="wp-main wp-empty" onDoubleClick={newUntitled}>
            <Icon name="file" size={28} />
            <div>
              {view === 'changes'
                ? tx('pickAChangedFileOr')
                : tx('pickAFileInThe')}
            </div>
            <div className="muted small">
              {tx('pToOpenAFile', { v: navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+' })}
            </div>
          </div>
        )}
        {showTree && (
          <>
            {(file || emptyPane) && (
              <div
                className="wp-split hide-mobile"
                title={tx('dragToResizeDoubleClick')}
                onPointerDown={dragTree}
                onDoubleClick={() => {
                  setTreeW(TREE_DEFAULT);
                  lsSet('wp-tree-w', TREE_DEFAULT);
                }}
              />
            )}
            {tree}
          </>
        )}
      </div>
      {quick !== null && root && (
        <QuickOpen
          hostId={hostId}
          root={root}
          initial={quick}
          recent={[...tabs].reverse().filter((t) => !isWebTab(t) && !isReviewTab(t) && !isSideTab(t) && !isCommitTab(t) && !isUntitled(t)).map(rel)}
          current={file && !file.binary ? { name: baseName(file.path), text: file.draft, lines: file.draft.split('\n').length } : null}
          onOpen={(r) => openFile(relOpen(r))}
          onGoto={(line) => {
            setShowDiff(false);
            setCompare(null);
            // Jumping via the outline in the Markdown preview: only scroll the preview, don't switch back to source
            if (md && preview) return setMdGoto({ line, n: Date.now() });
            if (md && mdSplit) setMdGoto({ line, n: Date.now() });
            setPreview(false);
            setGoto({ line, n: Date.now() });
          }}
          onClose={() => setQuick(null)}
        />
      )}
      {commitOpen && git && (
        <CommitDialog
          hostId={hostId}
          cwd={cwd}
          branch={git.branch}
          defaultBranch={branchesInfo?.defaultBranch}
          files={commitFiles}
          onCommitted={() => {
            refreshTree();
            void loadBranches();
            if (diffScope === 'branch') loadBranchDiff();
          }}
          onClose={() => setCommitOpen(false)}
        />
      )}
      {reviewPreset && !standalone && git && (
        <ReviewLaunchDialog
          hostId={hostId}
          cwd={cwd}
          parentSessionId={sessionId}
          currentTool={sessInfo?.tool}
          currentBranch={git.branch}
          branchesInfo={branchesInfo}
          defaultScope={diffScope === 'branch' ? 'branch' : 'uncommitted'}
          branchBase={effBranchBase || undefined}
          branchMergeBase={branchDiff?.mergeBase || undefined}
          uncommittedCount={uncommittedKeys.length}
          branchCount={branchKeys.length}
          preset={reviewPreset}
          onStarted={(rsid) => {
            void openFile(sideTabPath(rsid));
          }}
          onClose={() => setReviewPreset(null)}
        />
      )}
      {bwOpen && bwBtnRef.current && git && (
        <BranchWorktreePopover
          anchor={bwBtnRef.current}
          cwd={cwd}
          baseCwd={baseCwd}
          isOtherWorktree={isOtherWorktree}
          standalone={standalone}
          worktrees={worktrees}
          branchesInfo={branchesInfo}
          loadingBranches={branchesLoading}
          onSwitchWorktree={onSwitchCwd ? switchWorktree : undefined}
          onCheckoutBranch={checkoutBranch}
          onCreateBranch={() => void createBranch()}
          onNewSession={standalone ? undefined : () => openNewSession({ hostId, cwd })}
          onClose={() => setBwOpen(false)}
        />
      )}
    </aside>
  );
}

// ---------- Path above the editor: clicking a directory pops up the tree at that level ----------

function Breadcrumbs({
  hostId,
  root,
  path,
  hidden,
  git,
  dirty,
  onOpen,
}: {
  hostId: string;
  root: string;
  path: string;
  hidden: boolean;
  git: GitInfo | null;
  dirty: boolean;
  onOpen: (p: string) => void;
}) {
  const box = useRef<HTMLDivElement>(null);
  const [pop, setPop] = useState<{ dir: string; x: number; y: number } | null>(null);
  useBack(!!pop, () => setPop(null));
  const inRoot = path.startsWith(root);
  const segs = (inRoot ? path.slice(root.length) : path).split(/[\\/]/).filter(Boolean);
  const rootLabel = inRoot ? baseName(root) || root : '';
  // When the path is too long, scroll to the far right so the file name stays visible
  useLayoutEffect(() => {
    if (box.current) box.current.scrollLeft = box.current.scrollWidth;
  }, [path]);

  const dirAt = (i: number) => (i < 0 ? root : join(root, segs.slice(0, i + 1).join(sepOf(root))));
  const openPop = (e: React.MouseEvent<HTMLElement>, dir: string) => {
    const r = e.currentTarget.getBoundingClientRect();
    setPop({ dir, x: Math.min(r.left, window.innerWidth - 400), y: r.bottom + 4 });
  };

  return (
    <div className="wp-crumbs" ref={box} title={path} aria-label={tx('filePath')}>
      {inRoot && (
        <button className="wp-crumb" onClick={(e) => openPop(e, root)}>
          {rootLabel}
        </button>
      )}
      {segs.map((s, i) => {
        const last = i === segs.length - 1;
        return (
          <span key={i} className="wp-crumb-item">
            {(i > 0 || inRoot) && (
              <span className="wp-crumb-sep">
                <Icon name="chevron" size={12} />
              </span>
            )}
            {last ? (
              <span className="wp-crumb cur">
                {s}
                {dirty && <span className="wp-dot" title={tx('unsavedChanges')}>•</span>}
              </span>
            ) : (
              <button className="wp-crumb" onClick={(e) => (inRoot ? openPop(e, dirAt(i)) : undefined)}>
                {s}
              </button>
            )}
          </span>
        );
      })}
      {pop && (
        <DirPopover
          hostId={hostId}
          dir={pop.dir}
          hidden={hidden}
          git={git}
          selected={path}
          x={pop.x}
          y={pop.y}
          onClose={() => setPop(null)}
          onOpen={(p) => {
            setPop(null);
            onOpen(p);
          }}
        />
      )}
    </div>
  );
}

function DirPopover({
  hostId,
  dir,
  hidden,
  git,
  selected,
  x,
  y,
  onClose,
  onOpen,
}: {
  hostId: string;
  dir: string;
  hidden: boolean;
  git: GitInfo | null;
  selected: string;
  x: number;
  y: number;
  onClose: () => void;
  onOpen: (p: string) => void;
}) {
  const [kids, setKids] = useState<Record<string, DirEntry[]>>({});
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const box = useRef<HTMLDivElement>(null);
  const load = useCallback(
    (d: string) =>
      rpc<{ entries: DirEntry[] }>(hostId, 'fs.list', { path: d, showHidden: hidden })
        .then((r) => setKids((k) => ({ ...k, [d]: r.entries })))
        .catch(errorToast),
    [hostId, hidden],
  );
  useEffect(() => {
    load(dir);
  }, [dir, load]);
  useEffect(() => {
    const down = (e: MouseEvent) => !box.current?.contains(e.target as Node) && onClose();
    const key = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('mousedown', down);
    window.addEventListener('keydown', key);
    return () => {
      window.removeEventListener('mousedown', down);
      window.removeEventListener('keydown', key);
    };
  }, [onClose]);
  const rows = flatten(dir, 0, kids, expanded);
  return (
    <div className="wp-pop" ref={box} style={{ left: x, top: y }} role="tree" aria-label={tx('folderContents')}>
      {!kids[dir] ? (
        <div className="wp-tip">{tx('loadingFiles')}</div>
      ) : rows.length === 0 ? (
        <div className="wp-tip">{tx('thisFolderIsEmpty')}</div>
      ) : (
        <Rows
          rows={rows}
          git={git}
          selected={selected}
          onToggle={(r) => {
            const n = new Set(expanded);
            if (n.has(r.path)) n.delete(r.path);
            else {
              n.add(r.path);
              if (!kids[r.path]) load(r.path);
            }
            setExpanded(n);
          }}
          onOpen={(r) => onOpen(r.path)}
        />
      )}
    </div>
  );
}

/** The AI browser tab's icon: it pulses while the AI acts in that browser */
function AiTabIcon({ hostId, sessionId }: { hostId: string; sessionId: string }) {
  const busy = useAiBrowserBusy(hostId, sessionId);
  return (
    <span className={`aiweb-tab-icon ${busy ? 'busy' : ''}`}>
      <Icon name="bot" size={13} />
    </span>
  );
}
