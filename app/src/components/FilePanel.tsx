import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useBack } from '../back';
import { getT, trErr, useLang } from '../i18n';
import { errorToast, getState, hostStatus, loadSession, navigate, openNewSession, skey, toast, useSessionData, useSessionDiffEvents, useStore } from '../store';
import { isTauri, rpc } from '../transport';
import {
  isHostSession,
  normDirPath,
  TOOL_LABEL,
  type DirEntry,
  type FileDiff,
  type HubEvent,
  type NavDefinitionResult,
  type NavLocation,
  type NavReferenceHit,
  type NavReferencesResult,
  type QuestionAnswer,
} from '../types';
import { fmtSize, lsGet, lsSet } from '../util';
import { writeClipboard } from '../clipboard';
import { addContextRefToChat } from '../contextRefs';
import { outline } from '../outline';
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
import { CodeEditor, type CodeEditorHandle, type EditorMenuCtx, type GotoTarget } from './CodeEditor';
import { IS_MAC, type NavCoords, type NavTrigger } from './EditorCodeNav';
import { CodeNavNotice, DefinitionPicker, PeekReferences } from './CodeNavOverlays';
import { PaneDiff } from './DiffView';
import { MergeDiff, oneSided, type MergeDiffHandle } from './MergeDiff';
import { CsvView, ImageView, MdPreview, PdfView } from './FilePreview';
import { GrepPanel } from './GrepPanel';
import { openUrl } from './PortsView';
import { QuickOpen, type QuickMode } from './QuickOpen';
import { OPEN_CHANGES, OPEN_FILE, takeOpenChanges, takeOpenFiles, takeOpenTurn } from './SessionActions';
import { DRAG_REF, isOtherSessionUpload } from '../dropFiles';
import { BrowserPane, isWebTab, previewFileUrl, WEB_PREFIX, webTabFile, webTabTitle } from './Browser';
import { AI_PREFIX, aiTabSession, AiBrowserPane, isAiTab, useAiBrowserBusy } from './AiBrowser';
import { registerWebTabs } from './NativeWeb';
import { canUseBrowser } from '../browserUse';
import { CommitDetails, commitRangeTab, commitTab, GitHistory, isCommitTab, parseCommitTab } from './GitHistory';
import {
  BranchWorktreePopover,
  CommitDialog,
  NewWorktreeDialog,
  worktreeSlug,
  type BranchesResult,
  type WorktreeEntry,
  type WorktreesResult,
} from './GitOpsDialogs';
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
  /** PDF: base64 (remote fallback) */
  pdf?: string;
  /** PDF: streaming URL on ahfs / /fsv/ */
  pdfSrc?: string;
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
export { DRAG_REF };
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
type GitFile = { status: string; diff: string; path: string; rel: string; oldPath?: string; added?: number | null; deleted?: number | null };

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
  ahead?: number | null;
  behind?: number | null;
  /** True when git.changes included current-branch ahead/behind (false on older daemons) */
  hasTrack?: boolean;
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
  loadingDirs,
  onToggle,
  onOpen,
  onMenu,
  onDragRow,
  dropDir,
  onDropOver,
  stats,
  onDiscard,
  reviewBadges,
  reviewRelOf,
  reviewCleanCheck,
}: {
  rows: TreeRow[];
  /** The scrolling element the rows sit in; given, long lists are virtualized (rows all share one height) */
  scroller?: React.RefObject<HTMLElement | null>;
  /** Changes view: show lines added/removed to the right of file rows */
  stats?: boolean;
  onDiscard?: (r: TreeRow) => void;
  selected?: string;
  /** Keyboard cursor (row key), separate from the opened file as in VS Code */
  focused?: string | null;
  git?: GitInfo | null;
  /** Directories currently fetching children */
  loadingDirs?: Set<string>;
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
  const rowHRef = useRef(rowH);
  rowHRef.current = rowH;
  const [win, setWin] = useState<[number, number]>([0, 2 * OVERSCAN]);
  /** Where the rows start inside the scroller (notices and totals may sit above them) */
  const offsetIn = (el: HTMLElement) => box.current!.getBoundingClientRect().top - el.getBoundingClientRect().top + el.scrollTop;
  useLayoutEffect(() => {
    const el = scroller?.current;
    if (!virtual || !el || !box.current) return;
    let raf = 0;
    const syncRowHeight = () => {
      if (!box.current) return false;
      const sample = box.current.querySelector<HTMLElement>('.wp-row');
      const h = sample?.offsetHeight || rowHRef.current;
      if (h !== rowHRef.current) {
        setRowH(h);
        return true;
      }
      return false;
    };
    const update = () => {
      if (!box.current) return;
      if (syncRowHeight()) return;
      const h = rowHRef.current;
      const y = el.scrollTop - offsetIn(el);
      const from = Math.max(0, Math.floor(y / h) - OVERSCAN);
      const to = Math.min(rows.length, Math.ceil((y + el.clientHeight) / h) + OVERSCAN);
      setWin((w) => (w[0] === from && w[1] === to ? w : [from, to]));
    };
    const onScroll = () => {
      if (raf) return;
      raf = requestAnimationFrame(() => {
        raf = 0;
        update();
      });
    };
    update();
    el.addEventListener('scroll', onScroll, { passive: true });
    const ro = new ResizeObserver(update);
    ro.observe(el);
    return () => {
      if (raf) cancelAnimationFrame(raf);
      el.removeEventListener('scroll', onScroll);
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
            {r.loading || (r.isDir && loadingDirs?.has(r.path)) ? (
              <span className="spinner mini" />
            ) : r.isDir ? (
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
          {onDiscard && !r.loading && (
            <button
              type="button"
              className="wp-row-act danger"
              title={r.isDir ? tx('discardFolderChanges') : tx('discardFileChanges')}
              aria-label={r.isDir ? tx('discardFolderChanges') : tx('discardFileChanges')}
              onClick={(e) => {
                e.stopPropagation();
                onDiscard(r);
              }}
            >
              <Icon name="rotate-l" size={13} />
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
const gitCache = new Map<string, { git: GitInfo | null; sig: string; ignSig: string; trackSig?: string }>();
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

const FILE_CACHE_MAX_ENTRIES = 12;
const FILE_CACHE_MAX_BYTES = 6 * 1024 * 1024;
const GIT_POLL_COOLDOWN_MS = 2000;
const WT_POLL_COOLDOWN_MS = 30_000;
const STALE_REOPEN_MS = 10_000;

function visibleExpandedDirs(root: string, expanded: Set<string>): string[] {
  const rootN = norm(root);
  const normExp = new Set([...expanded].map(norm));
  const out: string[] = [];
  for (const dir of expanded) {
    const dn = norm(dir);
    if (!dn.startsWith(rootN + '/')) continue;
    let ok = true;
    let cur = dn;
    while (cur.length > rootN.length) {
      const slash = cur.lastIndexOf('/');
      if (slash <= rootN.length) break;
      const parent = cur.slice(0, slash);
      if (!normExp.has(parent)) {
        ok = false;
        break;
      }
      cur = parent;
    }
    if (ok) out.push(dir);
  }
  return out;
}

const hasFileExt = (p: string) => {
  const n = baseName(p);
  const dot = n.lastIndexOf('.');
  return dot > 0 && dot < n.length - 1 && /^[a-z0-9_-]{1,12}$/i.test(n.slice(dot + 1));
};

export function FilePanel({
  hostId,
  cwd,
  baseCwd,
  onSwitchCwd,
  sessionId,
  status,
  visible = true,
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
  /** False when kept mounted in the background via CSS display:none (Option 1A) */
  visible?: boolean;
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
  const tabsKey = standalone ? `wp-open-s-${hostId}/${sessionId}` : `wp-open-${hostId}/${cwd}`;
  const untitled = untitledStore(tabsKey);
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
  const mainRef = useRef<HTMLDivElement>(null);
  const sideRef = useRef<HTMLDivElement>(null);
  const dragCleanupRef = useRef<(() => void) | null>(null);
  useEffect(() => () => dragCleanupRef.current?.(), []);
  const [filter, setFilter] = useState('');
  const [search, setSearch] = useState<{ q: string; paths: string[]; truncated: boolean; error?: boolean } | null>(null);
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  // Opened files (tabs); the current file is file
  const [tabs, setTabs] = useState<string[]>([]);
  const openTabsRef = useRef(tabs);
  openTabsRef.current = tabs;
  // Native webviews (NativeWeb.tsx) stay parked while their tab is open, so switching tabs doesn't reload them
  const panelId = useRef(`${Math.random()}`).current;
  useLayoutEffect(() => registerWebTabs(panelId, tabs.filter(isWebTab).map((t) => `${hostId}|${t}`)), [tabs, hostId]); // eslint-disable-line react-hooks/exhaustive-deps
  // Later than the webviews' own unmount, which parks them (closing the panel, switching sessions keeps them)
  useEffect(() => () => void setTimeout(() => registerWebTabs(panelId, null), 0), []); // eslint-disable-line react-hooks/exhaustive-deps
  const [git, setGit] = useState<GitInfo | null>(() => gitCache.get(gitKey(hostId, cwd))?.git ?? null);
  // Until git status comes back we don't know whether this is a repo, so the snippets recorded in the session can't be shown as a diff yet
  const [gitReady, setGitReady] = useState(() => gitCache.has(gitKey(hostId, cwd)));
  const [worktrees, setWorktrees] = useState<WorktreeEntry[]>([]);
  const [wtDefaultBranch, setWtDefaultBranch] = useState<string | null>(null);
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
  const bwOpenRef = useRef(false);
  bwOpenRef.current = bwOpen;
  const wtScopeRef = useRef(`${hostId}\n${cwd}`);
  wtScopeRef.current = `${hostId}\n${cwd}`;
  const wtReqSeq = useRef(0);
  const wtAppliedSeq = useRef(0);
  const bwBtnRef = useRef<HTMLButtonElement>(null);
  const [commitOpen, setCommitOpen] = useState(false);
  const [commitDefaultPush, setCommitDefaultPush] = useState(false);
  const [preferCommitPush, setPreferCommitPush] = useState(() => lsGet('wp-prefer-commit-push', true));
  const [newWtOpen, setNewWtOpen] = useState(false);
  const [newWtBase, setNewWtBase] = useState<string | null>(null);
  const [reviewPreset, setReviewPreset] = useState<ReviewLaunchPreset | null>(null);
  const [pushing, setPushing] = useState(false);
  const [fetching, setFetching] = useState(false);
  const [gitBusy, setGitBusy] = useState(false);
  const [discarding, setDiscarding] = useState(false);
  const [view, setView] = useState<'all' | 'changes' | 'search'>(() => lsGet(viewKey, 'all'));
  const [showDiff, setShowDiff] = useState(false);
  // Default to Cursor-style single-file inline diff with the full file expanded
  const [diffLayout, setDiffLayout] = useState<'split' | 'unified'>(() => lsGet('wp-diff-mode', 'unified'));
  const [diffCollapse, setDiffCollapse] = useState(() => lsGet('wp-diff-fold', false));
  const mergeRef = useRef<MergeDiffHandle>(null);
  const codeRef = useRef<CodeEditorHandle>(null);
  const [treeLoading, setTreeLoading] = useState(false);
  const [loadingDirs, setLoadingDirs] = useState<Set<string>>(() => new Set());
  const [gitLoading, setGitLoading] = useState(false);
  const [openingPath, setOpeningPath] = useState<string | null>(null);
  const fileCacheRef = useRef<Map<string, { file: OpenFile; bytes: number }>>(new Map());
  const fileCacheBytesRef = useRef(0);
  const treeGenRef = useRef(0);
  const gitInFlightRef = useRef<Promise<void> | null>(null);
  const lastGitFetchAtRef = useRef(0);
  const lastWtFetchRef = useRef<{ scope: string; at: number }>({ scope: '', at: 0 });
  const diffInFlightRef = useRef<Set<string>>(new Set());
  const blobInFlightRef = useRef<Set<string>>(new Set());
  const gitCacheGenRef = useRef(0);
  const visibleRef = useRef(visible);
  visibleRef.current = visible;
  // Make the editor jump to a line (search results, outline, : line number, context pills)
  const [goto, setGoto] = useState<GotoTarget | null>(null);
  const [quick, setQuick] = useState<QuickMode | null>(null);
  // Another file opened side by side on the right (read-only); compare diffs the current file against another
  const [second, setSecond] = useState<OpenFile | null>(null);
  const [compare, setCompare] = useState<{ path: string; text: string; replace?: boolean } | null>(null);
  // "Find in folder" from the tree: the search view fills "files to include" with it
  const [grepInclude, setGrepInclude] = useState<{ value: string; n: number } | null>(null);
  // "Open in Search" from Peek References: populate query in GrepPanel
  const [grepQuery, setGrepQuery] = useState<{ query: string; caseSensitive?: boolean; wholeWord?: boolean; regex?: boolean; n: number } | null>(null);
  // Code navigation overlays & history stack (up to 50 locations)
  type NavPoint = { path: string; line: number; col: number; diff?: boolean; side?: 'a' | 'b' | 'del-file'; comparePath?: string; rev?: string };
  const navHistRef = useRef<{ entries: NavPoint[]; index: number }>({ entries: [], index: -1 });
  const [navState, setNavState] = useState({ canBack: false, canForward: false });
  const curPosRef = useRef<{ path: string; line: number; col: number; side?: 'a' | 'b' | 'del-file' }>({ path: '', line: 1, col: 0 });
  const navSteppingRef = useRef(false);
  const navStepChainRef = useRef<Promise<void>>(Promise.resolve());
  const openSeqRef = useRef(0);
  const navReqIdRef = useRef(0);
  const showDiffRef = useRef(showDiff);
  showDiffRef.current = showDiff;
  const compareRef = useRef(compare);
  compareRef.current = compare;
  const secondRef = useRef(second);
  secondRef.current = second;
  const mergeEditableRef = useRef(false);
  const nextFileRevRef = useRef(0);
  const nextFileRev = () => (nextFileRevRef.current = Math.max(Date.now(), nextFileRevRef.current + 1));
  const noticeTimerRef = useRef<number | null>(null);
  const [navPicker, setNavPicker] = useState<{ symbol: string; locations: NavLocation[]; coords: NavCoords; origin: NavPoint } | null>(null);
  const [navPeek, setNavPeek] = useState<{
    symbol: string;
    loading: boolean;
    result: NavReferencesResult | null;
    coords: NavCoords;
    origin: NavPoint;
  } | null>(null);
  const [navNotice, setNavNotice] = useState<string | null>(null);
  const cancelPendingNav = useCallback(() => {
    navReqIdRef.current += 1;
    setNavPicker(null);
    setNavPeek(null);
  }, []);
  useEffect(
    () => () => {
      if (noticeTimerRef.current != null) window.clearTimeout(noticeTimerRef.current);
    },
    [],
  );
  // Commit file tab: the title toggles the full commit details (there's no hover tooltip on phones)
  const [commitInfo, setCommitInfo] = useState(false);
  const [dropDir, setDropDir] = useState<string | null>(null);
  const [uploading, setUploading] = useState(0);
  const isLocal = useStore((st) => st.hosts.find((h) => h.config.id === hostId)?.config.kind === 'local');
  const connected = useStore((st) => st.hosts.find((h) => h.config.id === hostId)?.status.status === 'connected');
  const reviewVer = useReviewStoreVersion();
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

  const sessionEvents = useSessionDiffEvents(skey(hostId, sessionId), gitReady && !git);
  // Files changed by tools in this session: norm(absolute path) -> each edit made to that file
  const touched = useMemo(() => {
    const m = new Map<string, FileDiff[]>();
    for (const e of sessionEvents) {
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
  }, [sessionEvents, sessionEvents.length, root, cwd]);

  const dirty = !!file && file.draft !== file.content;
  const fileRef = useRef(file);
  fileRef.current = file;

  const cacheKey = (p: string) => unverbatim(p);

  const deleteCacheKey = useCallback((k: string) => {
    const old = fileCacheRef.current.get(k);
    if (!old) return;
    fileCacheBytesRef.current = Math.max(0, fileCacheBytesRef.current - old.bytes);
    fileCacheRef.current.delete(k);
  }, []);

  const dropCachedFile = useCallback(
    (p: string) => {
      const exact = cacheKey(p);
      deleteCacheKey(exact);
      const targetNorm = norm(p);
      for (const k of [...fileCacheRef.current.keys()]) {
        if (norm(k) === targetNorm) deleteCacheKey(k);
      }
    },
    [deleteCacheKey],
  );

  const clearFileCache = useCallback(() => {
    fileCacheRef.current.clear();
    fileCacheBytesRef.current = 0;
  }, []);

  const putCachedFile = useCallback(
    (f: OpenFile) => {
      if (f.untitled || f.web || f.side || f.review || f.commit || f.deleted || f.truncated) return;
      if (f.pdf && f.pdf.length > 2 * 1024 * 1024) return;
      const bytes = (f.content.length + (f.pdf?.length ?? 0)) * 2 + (f.image?.length ?? 0) + (f.pdfSrc?.length ?? 0);
      if (bytes > 2 * 1024 * 1024) return;
      const k = cacheKey(f.path);
      deleteCacheKey(k);
      const cache = fileCacheRef.current;
      while (cache.size >= FILE_CACHE_MAX_ENTRIES || fileCacheBytesRef.current + bytes > FILE_CACHE_MAX_BYTES) {
        const oldest = cache.keys().next().value;
        if (oldest === undefined) break;
        deleteCacheKey(oldest);
      }
      cache.set(k, { file: { ...f, path: k, draft: f.content }, bytes });
      fileCacheBytesRef.current += bytes;
    },
    [deleteCacheKey],
  );

  const getCachedFile = useCallback((p: string): OpenFile | null => {
    const k = cacheKey(p);
    const entry = fileCacheRef.current.get(k);
    if (!entry || cacheKey(entry.file.path) !== k) return null;
    fileCacheRef.current.delete(k);
    fileCacheRef.current.set(k, entry);
    return { ...entry.file, draft: entry.file.content, rev: nextFileRev() };
  }, []);

  const hasCachedFile = useCallback((p: string): boolean => {
    const k = cacheKey(p);
    const entry = fileCacheRef.current.get(k);
    return !!entry && cacheKey(entry.file.path) === k;
  }, []);

  const updateFileDraft = useCallback(
    (targetPath: string, targetRev: number, isUnt: boolean | undefined, draft: string) => {
      if (isUnt) untitled.set(targetPath, draft);
      if (fileRef.current && fileRef.current.path === targetPath && fileRef.current.rev === targetRev) {
        fileRef.current = { ...fileRef.current, draft };
      }
      setFile((cur) => (cur && cur.path === targetPath && cur.rev === targetRev ? { ...cur, draft } : cur));
    },
    [untitled],
  );

  const flushActiveEditor = useCallback((): string | undefined => {
    const cur = fileRef.current;
    if (!cur) return undefined;
    codeRef.current?.flush();
    if (mergeEditableRef.current) mergeRef.current?.flush();
    return fileRef.current?.draft;
  }, []);

  const hasUnsyncedEdits = useCallback(
    () => !!codeRef.current?.isDirty() || (mergeEditableRef.current && !!mergeRef.current?.isDirty()),
    [],
  );

  const getActiveEditorText = useCallback((cur: OpenFile): string => {
    if (codeRef.current) return codeRef.current.getValue();
    if (mergeEditableRef.current && mergeRef.current) return mergeRef.current.getValue();
    return cur.draft;
  }, []);

  const openQuick = useCallback(
    (m: QuickMode) => {
      flushActiveEditor();
      setQuick(m);
    },
    [flushActiveEditor],
  );

  /** quiet: restoring remembered folders; one that no longer exists is just forgotten */
  const load = useCallback(
    async (dir: string, quiet = false) => {
      if (!quiet) {
        setLoadingDirs((s) => {
          const n = new Set(s);
          n.add(dir);
          return n;
        });
      }
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
      } finally {
        if (!quiet) {
          setLoadingDirs((s) => {
            if (!s.has(dir)) return s;
            const n = new Set(s);
            n.delete(dir);
            return n;
          });
        }
      }
    },
    [hostId, hidden],
  );

  const loadDirsBatched = useCallback(
    async (dirs: string[], gen: number) => {
      if (!dirs.length) return;
      const CONCURRENCY = 4;
      const batchKids: Record<string, DirEntry[]> = {};
      const missing: string[] = [];
      let idx = 0;
      const worker = async () => {
        while (idx < dirs.length && treeGenRef.current === gen) {
          const d = dirs[idx++];
          try {
            const r = await rpc<{ path: string; entries: DirEntry[] }>(hostId, 'fs.list', { path: d, showHidden: hidden });
            batchKids[d] = r.entries;
          } catch {
            missing.push(d);
          }
        }
      };
      await Promise.all(Array.from({ length: Math.min(CONCURRENCY, dirs.length) }, worker));
      if (treeGenRef.current !== gen) return;
      if (Object.keys(batchKids).length) {
        setKids((k) => ({ ...k, ...batchKids }));
      }
      if (missing.length) {
        const drop = new Set(missing);
        setExpanded((x) => {
          let changed = false;
          const next = new Set<string>();
          for (const d of x) {
            if (drop.has(d)) changed = true;
            else next.add(d);
          }
          return changed ? next : x;
        });
      }
    },
    [hostId, hidden],
  );

  // Root directory: fs.list normalizes ~ and relative forms into absolute paths
  useEffect(() => {
    const gen = ++treeGenRef.current;
    clearFileCache();
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
    setTreeLoading(true);
    rpc<{ path: string; entries: DirEntry[] }>(hostId, 'fs.list', { path: cwd, showHidden: hidden })
      .then((r) => {
        if (treeGenRef.current !== gen) return;
        rootCache.set(key, { path: r.path, entries: r.entries });
        setRoot(r.path);
        setKids((k) => ({ ...k, [r.path]: r.entries }));
        const pre = norm(r.path) + '/';
        const stale = [...want].filter((d) => !norm(d).startsWith(pre));
        if (stale.length) setExpanded((x) => new Set([...x].filter((d) => !stale.includes(d))));
        const toLoad = visibleExpandedDirs(r.path, want);
        return loadDirsBatched(toLoad, gen);
      })
      .catch(errorToast)
      .finally(() => {
        if (treeGenRef.current === gen) setTreeLoading(false);
      });
  }, [hostId, cwd, hidden]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    if (expLoaded.current === expKey) lsSet(expKey, [...expanded].slice(-300));
  }, [expanded, expKey]);

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
  const gitTrackSig = useRef(gitCache.get(gitKey(hostId, cwd))?.trackSig ?? '');
  /** quiet: background refresh (timer / window refocus); if the status didn't change, just quietly refetch the current file's diff */
  const loadWorktrees = useCallback(
    (details = false) => {
      const wantDetails = details || bwOpenRef.current;
      const scope = `${hostId}\n${cwd}`;
      const seq = ++wtReqSeq.current;
      lastWtFetchRef.current = { scope, at: Date.now() };
      rpc<WorktreesResult>(hostId, 'git.worktrees', { cwd, ...(wantDetails ? { details: true } : {}) })
        .then((r) => {
          if (wtScopeRef.current !== scope || seq < wtAppliedSeq.current || (!wantDetails && seq < wtReqSeq.current)) {
            return;
          }
          wtAppliedSeq.current = seq;
          const list = (r.worktrees ?? []).filter((w) => !w.bare);
          if (wantDetails) {
            setWorktrees(list);
          } else {
            setWorktrees((prev) => {
              const prevByPath = new Map(prev.map((item) => [norm(item.path), item]));
              return list.map((w) => {
                const p = prevByPath.get(norm(w.path));
                if (!p || p.head !== w.head || (p.branch ?? null) !== (w.branch ?? null) || p.prunable !== w.prunable) {
                  return w;
                }
                return {
                  ...w,
                  ...(p.dirtyCount !== undefined ? { dirtyCount: p.dirtyCount } : {}),
                  ...(p.upstream !== undefined ? { upstream: p.upstream } : {}),
                  ...(p.ahead !== undefined ? { ahead: p.ahead } : {}),
                  ...(p.behind !== undefined ? { behind: p.behind } : {}),
                  ...(p.aheadBase !== undefined ? { aheadBase: p.aheadBase } : {}),
                  ...(p.merged !== undefined ? { merged: p.merged } : {}),
                };
              });
            });
          }
          if (r.defaultBranch !== undefined) setWtDefaultBranch(r.defaultBranch ?? null);
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
    },
    [hostId, cwd, onSwitchCwd],
  );

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

  const loadGit = useCallback(
    (quiet = false) => {
      if (quiet && (gitInFlightRef.current || Date.now() - lastGitFetchAtRef.current < GIT_POLL_COOLDOWN_MS)) {
        return;
      }
      lastGitFetchAtRef.current = Date.now();
      if (!quiet) setGitLoading(true);
      // Without diffs: in a big repo with many changed files, running git diff for each takes ages, so fetch only the file that gets opened
      const p = rpc<{
        repo: string | null;
        branch?: string;
        head?: string | null;
        ahead?: number | null;
        behind?: number | null;
        total?: number | null;
        files: { path: string; oldPath?: string; status: string; diff: string; added?: number | null; deleted?: number | null }[];
        ignored?: string[];
      }>(hostId, 'git.changes', { cwd, diff: false })
        .then((r) => {
          setGitReady(true);
          const rawIgnored = r.ignored ?? [];
          const sig = `${r.repo}|${r.branch}|${r.head}|${r.files.map((f) => `${f.status} ${f.oldPath ?? ''} ${f.path} ${f.added ?? ''}/${f.deleted ?? ''}`).join('|')}`;
          const ignSig = rawIgnored.join('\0');
          const hasTrack = 'ahead' in r || 'behind' in r;
          const trackSig = hasTrack ? `${r.ahead ?? ''}|${r.behind ?? ''}` : '';
          const filesChanged = !quiet || sig !== gitSig.current;
          const ignChanged = !quiet || ignSig !== gitIgnSig.current;
          const trackChanged = !quiet || trackSig !== gitTrackSig.current;
          const wtScope = `${hostId}\n${cwd}`;
          if (
            r.repo &&
            (!quiet ||
              filesChanged ||
              lastWtFetchRef.current.scope !== wtScope ||
              Date.now() - lastWtFetchRef.current.at >= WT_POLL_COOLDOWN_MS)
          ) {
            loadWorktrees(false);
          }
          if (!filesChanged && !ignChanged && !trackChanged) return refreshCurDiff.current();
          gitSig.current = sig;
          gitIgnSig.current = ignSig;
          gitTrackSig.current = trackSig;
          if (filesChanged) {
            gitCacheGenRef.current += 1;
            setFileDiffs({});
            setBlobs((m) => {
              const key = gitKey(hostId, cwd);
              staleBlobs.current = { key, blobs: staleBlobs.current.key === key ? { ...staleBlobs.current.blobs, ...m } : m };
              return {};
            });
          }
          if (!r.repo) {
            gitCache.set(gitKey(hostId, cwd), { git: null, sig, ignSig, trackSig });
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
          const info: GitInfo = {
            repo: r.repo,
            branch: r.branch ?? '',
            head: r.head,
            ahead: r.ahead,
            behind: r.behind,
            hasTrack,
            total: r.total,
            files,
            dirs,
            ignoredFiles,
            ignoredDirs,
          };
          gitCache.set(gitKey(hostId, cwd), { git: info, sig, ignSig, trackSig });
          setGit(info);
          if (!filesChanged) refreshCurDiff.current();
        })
        .catch(() => {
          setGitReady(true);
          if (!quiet) setGit(null);
        })
        .finally(() => {
          if (gitInFlightRef.current === p) gitInFlightRef.current = null;
          lastGitFetchAtRef.current = Date.now();
          if (!quiet) setGitLoading(false);
        });
      gitInFlightRef.current = p;
    },
    [hostId, cwd, loadWorktrees],
  );
  const refreshCurDiff = useRef(() => {});
  const revalidateActiveRef = useRef<(cached: OpenFile, quiet?: boolean, reqPath?: string) => Promise<void>>(async () => {});
  useEffect(() => {
    gitCacheGenRef.current += 1;
    diffInFlightRef.current.clear();
    blobInFlightRef.current.clear();
    setFileDiffs({});
    setBlobs({});
  }, [hostId, cwd]);
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
    if (git && (bwOpen || commitOpen || reviewPreset !== null || diffScope === 'branch' || (!git.hasTrack && !branchesInfo))) {
      void loadBranches();
    }
  }, [git?.branch, git?.hasTrack, bwOpen, commitOpen, reviewPreset, diffScope, loadBranches]); // eslint-disable-line react-hooks/exhaustive-deps
  // Commits made by the AI in a terminal or files changed by other programs aren't caught by the end-of-turn refresh: check again when the window regains focus and every 10 s (while visible)
  useEffect(() => {
    if (!visible) return;
    const tick = () => document.visibilityState === 'visible' && loadGit(true);
    const t = setInterval(tick, 10000);
    window.addEventListener('focus', tick);
    return () => {
      clearInterval(t);
      window.removeEventListener('focus', tick);
    };
  }, [loadGit, visible]);

  const prevVisibleRef = useRef(visible);
  useEffect(() => {
    const was = prevVisibleRef.current;
    prevVisibleRef.current = visible;
    if (!was && visible) {
      if (Date.now() - lastGitFetchAtRef.current > STALE_REOPEN_MS) {
        loadGit(true);
      }
      const cur = fileRef.current;
      if (
        cur &&
        cur.draft === cur.content &&
        !hasUnsyncedEdits() &&
        !cur.web &&
        !cur.commit &&
        !cur.review &&
        !cur.side &&
        !cur.untitled &&
        !cur.deleted
      ) {
        void revalidateActiveRef.current(cur, true, cur.path);
      }
    }
  }, [visible, loadGit, hasUnsyncedEdits]);

  const refreshTree = () => {
    if (!root) return;
    clearFileCache();
    const gen = ++treeGenRef.current;
    setTreeLoading(true);
    load(root)
      .then(() => loadDirsBatched(visibleExpandedDirs(root, expandedRef.current), gen))
      .finally(() => {
        if (treeGenRef.current === gen) setTreeLoading(false);
      });
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
      if (!kids[dir]) void load(dir);
      const unloadedSubs = visibleExpandedDirs(dir, next).filter((d) => !kids[d]);
      if (unloadedSubs.length) void loadDirsBatched(unloadedSubs, treeGenRef.current);
    }
    setExpanded(next);
  };

  // ---------- Opening and saving files ----------

  const readFileWithMeta = async (path: string, quiet = false, force = false): Promise<{ file: OpenFile; fromCache: boolean } | null> => {
    if (isSideTab(path)) return { file: { path, content: '', draft: '', binary: true, truncated: false, size: 0, modified: 0, rev: nextFileRev(), side: true }, fromCache: false };
    if (isReviewTab(path)) return { file: { path, content: '', draft: '', binary: true, truncated: false, size: 0, modified: 0, rev: nextFileRev(), review: true }, fromCache: false };
    if (isWebTab(path)) return { file: { path, content: '', draft: '', binary: true, truncated: false, size: 0, modified: 0, rev: nextFileRev(), web: true }, fromCache: false };
    if (isUntitled(path))
      return { file: { path, content: '', draft: untitled.get(path) ?? '', binary: false, truncated: false, size: 0, modified: 0, rev: nextFileRev(), untitled: true }, fromCache: false };
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
          return { file: { path, content: '', draft: '', binary: true, truncated: false, size: 0, modified: 0, rev: nextFileRev(), commit }, fromCache: false };
        }
        const r = await rpc<{ hash: string; author: string; time: number; message: string; files: { path: string; status: string; diff: string }[] }>(hostId, 'git.show', {
          cwd,
          hash: ct.hash,
          paths: [ct.rel],
        });
        const f = r.files.find((x) => x.path === ct.rel);
        const commit = { hash: r.hash, rel: ct.rel, subject: r.message.split('\n')[0], message: r.message, author: r.author, time: r.time, status: f?.status ?? '', diff: f?.diff ?? '' };
        return { file: { path, content: '', draft: '', binary: true, truncated: false, size: 0, modified: 0, rev: nextFileRev(), commit }, fromCache: false };
      } catch (e) {
        if (!quiet) errorToast(e);
        return null;
      }
    }
    if (!force) {
      const hit = getCachedFile(path);
      if (hit) return { file: hit, fromCache: true };
    }
    const mime = IMAGE_MIME[extOf(path)];
    try {
      let opened: OpenFile;
      if (mime) {
        if (isLocal) {
          const r = await previewFileUrl(hostId, path, 'image');
          opened = {
            path: r.path || path,
            size: r.size ?? 0,
            modified: r.modified ?? 0,
            content: '',
            draft: '',
            binary: true,
            truncated: false,
            image: r.url,
            rev: nextFileRev(),
          };
        } else {
          const r = await rpc<{ path: string; size: number; modified: number; base64: string }>(hostId, 'fs.read', { path, image: true });
          opened = { ...r, content: '', draft: '', binary: true, truncated: false, image: `data:${mime};base64,${r.base64}`, rev: nextFileRev() };
        }
      } else if (extOf(path) === 'pdf') {
        if (isLocal) {
          const r = await previewFileUrl(hostId, path, 'pdf');
          opened = {
            path: r.path || path,
            size: r.size ?? 0,
            modified: r.modified ?? 0,
            content: '',
            draft: '',
            binary: true,
            truncated: false,
            pdfSrc: r.url,
            rev: nextFileRev(),
          };
        } else {
          const r = await rpc<{ path: string; size: number; modified: number; base64: string }>(hostId, 'fs.read', { path, raw: true });
          opened = { ...r, content: '', draft: '', binary: true, truncated: false, pdf: r.base64, rev: nextFileRev() };
        }
      } else {
        const r = await rpc<Omit<OpenFile, 'draft' | 'rev'>>(hostId, 'fs.read', { path });
        opened = { ...r, draft: r.content, rev: nextFileRev() };
      }
      putCachedFile(opened);
      return { file: opened, fromCache: false };
    } catch (e) {
      if (!quiet) errorToast(e);
      return null;
    }
  };

  const readFile = async (path: string, quiet = false, force = false): Promise<OpenFile | null> =>
    (await readFileWithMeta(path, quiet, force))?.file ?? null;

  const revalidateFile = async (cached: OpenFile, reqPath?: string): Promise<OpenFile | 'unchanged'> => {
    const targetPath = reqPath ?? cached.path;
    if (isLocal && (cached.image || cached.pdfSrc)) {
      const kind = cached.image ? 'image' : 'pdf';
      const prevUrl = kind === 'image' ? cached.image : cached.pdfSrc;
      const r = await previewFileUrl(hostId, targetPath, kind);
      if (r.modified === cached.modified && r.size === cached.size && r.url === prevUrl) return 'unchanged';
      const fresh: OpenFile =
        kind === 'image'
          ? {
              path: r.path || targetPath,
              size: r.size ?? 0,
              modified: r.modified ?? 0,
              content: '',
              draft: '',
              binary: true,
              truncated: false,
              image: r.url,
              rev: nextFileRev(),
            }
          : {
              path: r.path || targetPath,
              size: r.size ?? 0,
              modified: r.modified ?? 0,
              content: '',
              draft: '',
              binary: true,
              truncated: false,
              pdfSrc: r.url,
              rev: nextFileRev(),
            };
      putCachedFile(fresh);
      return fresh;
    }
    const mime = cached.image ? IMAGE_MIME[extOf(targetPath)] : undefined;
    if (cached.image && mime) {
      const r = await rpc<{ unchanged?: boolean; path: string; size: number; modified: number; base64?: string }>(hostId, 'fs.read', {
        path: targetPath,
        image: true,
        ifModified: cached.modified,
        ifSize: cached.size,
      });
      if (r.unchanged) return 'unchanged';
      const dataUrl = `data:${mime};base64,${r.base64 ?? ''}`;
      if (r.modified === cached.modified && r.size === cached.size && dataUrl === cached.image) return 'unchanged';
      const fresh: OpenFile = {
        path: r.path || targetPath,
        size: r.size,
        modified: r.modified,
        content: '',
        draft: '',
        binary: true,
        truncated: false,
        image: dataUrl,
        rev: nextFileRev(),
      };
      putCachedFile(fresh);
      return fresh;
    }
    if (cached.pdf) {
      const r = await rpc<{ unchanged?: boolean; path: string; size: number; modified: number; base64?: string }>(hostId, 'fs.read', {
        path: targetPath,
        raw: true,
        ifModified: cached.modified,
        ifSize: cached.size,
      });
      if (r.unchanged) return 'unchanged';
      const b64 = r.base64 ?? '';
      if (r.modified === cached.modified && r.size === cached.size && b64 === cached.pdf) return 'unchanged';
      const fresh: OpenFile = {
        path: r.path || targetPath,
        size: r.size,
        modified: r.modified,
        content: '',
        draft: '',
        binary: true,
        truncated: false,
        pdf: b64,
        rev: nextFileRev(),
      };
      putCachedFile(fresh);
      return fresh;
    }
    const r = await rpc<{ unchanged?: boolean } & Omit<OpenFile, 'draft' | 'rev'>>(hostId, 'fs.read', {
      path: targetPath,
      ifModified: cached.modified,
      ifSize: cached.size,
    });
    if (r.unchanged) return 'unchanged';
    if (
      r.modified === cached.modified &&
      r.size === cached.size &&
      r.content === cached.content &&
      r.binary === cached.binary &&
      r.truncated === cached.truncated &&
      !!r.notUtf8 === !!cached.notUtf8
    ) {
      return 'unchanged';
    }
    const fresh: OpenFile = { ...r, draft: r.content, rev: nextFileRev() };
    putCachedFile(fresh);
    return fresh;
  };

  const revalidateActiveCached = async (cached: OpenFile, quiet = false, reqPath?: string) => {
    try {
      const res = await revalidateFile(cached, reqPath);
      if (res === 'unchanged') return;
      const live = fileRef.current;
      if (!live || live.path !== cached.path || live.rev !== cached.rev || live.draft !== live.content || hasUnsyncedEdits()) {
        return;
      }
      fileRef.current = res;
      setFile((cur) =>
        cur && cur.path === cached.path && cur.rev === cached.rev && cur.draft === cur.content && !hasUnsyncedEdits() ? res : cur,
      );
      setSaveState(null);
      setSaveError('');
      loadGit(true);
    } catch (e) {
      dropCachedFile(reqPath ?? cached.path);
      loadGit(true);
      const live = fileRef.current;
      if (!live || live.path !== cached.path || live.rev !== cached.rev || live.draft !== live.content || hasUnsyncedEdits()) {
        return;
      }
      if (!quiet) errorToast(e);
      const msg = String((e as Error)?.message ?? e);
      if (!/(连接|超时|timeout|disconnect|未连接)/i.test(msg)) {
        fileRef.current = null;
        setFile((cur) =>
          cur && cur.path === cached.path && cur.rev === cached.rev && cur.draft === cur.content && !hasUnsyncedEdits() ? null : cur,
        );
      }
    }
  };
  revalidateActiveRef.current = revalidateActiveCached;

  const readFileValidated = async (path: string, quiet = false): Promise<OpenFile | null> => {
    const res = await readFileWithMeta(path, quiet, false);
    if (!res) return null;
    if (!res.fromCache) return res.file;
    try {
      const checked = await revalidateFile(res.file, path);
      return checked === 'unchanged' ? res.file : checked;
    } catch (e) {
      dropCachedFile(path);
      if (!quiet) errorToast(e);
      return null;
    }
  };

  const confirmDiscard = async () => {
    const latestDraft = flushActiveEditor();
    const cur = fileRef.current;
    const curDraft = latestDraft ?? cur?.draft ?? '';
    if (!cur || curDraft === cur.content) return true;
    return askConfirm({
      title: tx('discardChangesTo', { v: nameOf(cur.path) }),
      message: tx('unsavedEditsWillBeDiscarded'),
      okText: tx('discard'),
      danger: true,
    });
  };

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
    if (!showDiff || !curKey || !curGit || curGit.diff || curDiffCacheKey in fileDiffs) return;
    const k = curDiffCacheKey;
    const gen = gitCacheGenRef.current;
    const inflightKey = `${gen}\n${hostId}\n${cwd}\n${k}`;
    if (diffInFlightRef.current.has(inflightKey)) return;
    diffInFlightRef.current.add(inflightKey);
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
          : rpc<{ files: { path: string; diff: string }[] }>(hostId, 'git.changes', {
              cwd,
              paths: curGit.oldPath ? [curGit.rel, curGit.oldPath] : [curGit.rel],
            });
    req
      .then((r) => {
        if (gitCacheGenRef.current !== gen) return;
        setFileDiffs((m) => ({ ...m, [k]: r.files.find((f) => f.path === curGit.rel)?.diff ?? '' }));
      })
      .catch(() => {
        if (gitCacheGenRef.current !== gen) return;
        setFileDiffs((m) => ({ ...m, [k]: '' }));
      })
      .finally(() => diffInFlightRef.current.delete(inflightKey));
  }, [showDiff, curKey, curDiffCacheKey, curGit, fileDiffs, diffScope, branchBase, activeTurnCkpt?.baseRef, activeTurnCkpt?.endRef]); // eslint-disable-line react-hooks/exhaustive-deps
  // Status unchanged but the content may have changed again: refetch the current file's diff in the background and swap it in when it arrives, without showing "loading"
  refreshCurDiff.current = () => {
    if (!showDiff || !curKey || !curGit || !(curDiffCacheKey in fileDiffs) || diffScope === 'turn') return;
    const k = curDiffCacheKey;
    const gen = gitCacheGenRef.current;
    const inflightKey = `${gen}\n${hostId}\n${cwd}\n${k}`;
    if (diffInFlightRef.current.has(inflightKey)) return;
    diffInFlightRef.current.add(inflightKey);
    const req =
      diffScope === 'branch'
        ? rpc<{ files: { path: string; diff: string }[] }>(hostId, 'git.branchDiff', {
            cwd,
            base: branchBase || undefined,
            paths: [curGit.rel],
          })
        : rpc<{ files: { path: string; diff: string }[] }>(hostId, 'git.changes', {
            cwd,
            paths: curGit.oldPath ? [curGit.rel, curGit.oldPath] : [curGit.rel],
          });
    req
      .then((r) => {
        if (gitCacheGenRef.current !== gen) return;
        const d = r.files.find((f) => f.path === curGit.rel)?.diff ?? '';
        setFileDiffs((m) => (m[k] === d ? m : { ...m, [k]: d }));
      })
      .catch(() => {})
      .finally(() => diffInFlightRef.current.delete(inflightKey));
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
    flushActiveEditor();
    lsSet('wp-md-mode', m);
    setMdSplit(m === 'split');
    setPreview(m === 'preview');
  };

  const showNavNotice = useCallback((msg: string) => {
    if (noticeTimerRef.current != null) window.clearTimeout(noticeTimerRef.current);
    setNavNotice(msg);
    noticeTimerRef.current = window.setTimeout(() => {
      setNavNotice(null);
      noticeTimerRef.current = null;
    }, 1800);
  }, []);

  const syncNavState = useCallback(() => {
    const h = navHistRef.current;
    setNavState({ canBack: h.index > 0, canForward: h.index >= 0 && h.index < h.entries.length - 1 });
  }, []);

  const isNavTrackable = (p: string) => !isWebTab(p) && !isReviewTab(p) && !isSideTab(p) && !isUntitled(p);

  const recordNavJump = useCallback(
    (from: NavPoint | null, to: NavPoint) => {
      if (!isNavTrackable(to.path)) return;
      const h = navHistRef.current;
      let entries = h.entries.slice(0, h.index + 1);
      const samePoint = (a?: NavPoint, b?: NavPoint) =>
        !!a &&
        !!b &&
        norm(a.path) === norm(b.path) &&
        a.line === b.line &&
        !!a.diff === !!b.diff &&
        (a.side ?? 'b') === (b.side ?? 'b') &&
        (a.comparePath ?? '') === (b.comparePath ?? '') &&
        (a.rev ?? '') === (b.rev ?? '');
      if (from && isNavTrackable(from.path) && !samePoint(entries[entries.length - 1], from)) {
        entries.push(from);
      }
      if (!samePoint(entries[entries.length - 1], to)) {
        entries.push(to);
      }
      if (entries.length > 50) entries = entries.slice(entries.length - 50);
      navHistRef.current = { entries, index: entries.length - 1 };
      syncNavState();
    },
    [syncNavState],
  );

  useEffect(() => {
    navHistRef.current = { entries: [], index: -1 };
    syncNavState();
    cancelPendingNav();
  }, [hostId, cwd, syncNavState, cancelPendingNav]);

  useEffect(() => {
    cancelPendingNav();
  }, [file?.path, showDiff, compare?.path, compare?.replace, second?.path, diffLayout, cancelPendingNav]);

  /** diff: files opened from the changes list show the diff by default; line: jump to this line after opening.
   *  Returns explicit outcome so history jumps only commit after the target opens cleanly. */
  const openFileInternal = async (
    path0: string,
    diff = false,
    at?: GotoTarget | { line: number; endLine?: number; col?: number; len?: number; side?: 'a' | 'b' | 'del-file' },
    opts?: { skipHistory?: boolean; comparePath?: string; quiet?: boolean },
  ): Promise<'opened' | 'cancelled' | 'failed'> => {
    const seq = ++openSeqRef.current;
    setOpeningPath(null);
    const path = unverbatim(path0);
    const jump = () => at && setGoto({ ...at, n: Date.now() });
    const cur = fileRef.current;
    const prevPoint: NavPoint | null =
      cur && isNavTrackable(cur.path)
        ? {
            path: cur.path,
            line: norm(curPosRef.current.path) === norm(cur.path) ? curPosRef.current.line : 1,
            col: norm(curPosRef.current.path) === norm(cur.path) ? curPosRef.current.col : 0,
            diff: showDiffRef.current || !!cur.commit || !!compareRef.current,
            side: norm(curPosRef.current.path) === norm(cur.path) ? curPosRef.current.side : undefined,
            comparePath: compareRef.current && !compareRef.current.replace ? compareRef.current.path : undefined,
          }
        : null;
    if (cur?.path === path) {
      const canDiff = !!cur.commit || !!cur.deleted || !!opts?.comparePath || hasChange(path);
      if (opts?.comparePath) {
        flushActiveEditor();
        if (norm(compareRef.current?.path ?? '') !== norm(opts.comparePath)) {
          setOpeningPath(opts.comparePath);
          const cf = await readFileValidated(opts.comparePath, !!opts?.quiet);
          if (openSeqRef.current === seq) setOpeningPath(null);
          if (openSeqRef.current !== seq) return 'cancelled';
          if (!cf) return 'failed';
          setCompare({ path: cf.path, text: cf.content });
        }
        setShowDiff(false);
      } else if (diff && canDiff) {
        flushActiveEditor();
        setCompare(null);
        setShowDiff(true);
      }
      if (at) {
        if (!opts?.skipHistory && !navSteppingRef.current && !restoring.current && prevPoint && prevPoint.line !== at.line) {
          recordNavJump(prevPoint, { path, line: at.line, col: at.col ?? 0, diff: !opts?.comparePath && diff && canDiff, side: at.side, comparePath: opts?.comparePath });
        }
        curPosRef.current = { path, line: at.line, col: at.col ?? 0, side: at.side };
        if (!opts?.comparePath) {
          if (!diff || !canDiff) {
            flushActiveEditor();
            setShowDiff(false);
          }
          setCompare(null);
        }
        if (preview && !diff && !opts?.comparePath) previewFor(path, at);
        jump();
      }
      if (window.matchMedia('(max-width: 760px)').matches) setTreeOpen(false);
      return 'opened';
    }
    // An untitled tab keeps its text in memory when you switch away; flush debounced edits first
    if (cur?.untitled) flushActiveEditor();
    else if (!(await confirmDiscard())) return 'cancelled';
    if (openSeqRef.current !== seq) return 'cancelled';
    const g = diffGitOf(path);
    const isDel = g && gitMark(g.status)[0] === 'D';
    const isSpecial = isSideTab(path) || isReviewTab(path) || isWebTab(path) || isUntitled(path);
    const cachedHit = !isDel && !isSpecial && !parseCommitTab(path) ? hasCachedFile(path) : false;
    if (!cachedHit && !isDel && !isSpecial) setOpeningPath(path);
    let f: OpenFile | null = null;
    let openedFromCache = false;
    try {
      if (isDel) {
        f = { path, content: '', draft: '', binary: true, truncated: false, size: 0, modified: 0, rev: nextFileRev(), deleted: true };
      } else {
        const res = await readFileWithMeta(path, !!opts?.quiet);
        if (res) {
          f = res.file;
          openedFromCache = res.fromCache;
        } else if (diffScope === 'turn' && g) {
          f = { path, content: '', draft: '', binary: false, truncated: false, size: 0, modified: 0, rev: nextFileRev(), deleted: true };
        }
      }
      if (openSeqRef.current !== seq) return 'cancelled';
      if (!f) return 'failed';
      let nextCompare: { path: string; text: string; replace?: boolean } | null = null;
      if (opts?.comparePath) {
        const cf = await readFileValidated(opts.comparePath, !!opts?.quiet);
        if (openSeqRef.current !== seq) return 'cancelled';
        if (!cf) return 'failed';
        nextCompare = { path: cf.path, text: cf.content };
      }
      const willDiff = !nextCompare && (!!f.deleted || (diff && hasChange(path)) || !!f.commit);
      if (!opts?.skipHistory && !navSteppingRef.current && !restoring.current && isNavTrackable(path) && !f.binary && !f.image && !f.pdf && !f.pdfSrc) {
        recordNavJump(prevPoint, { path, line: at?.line ?? 1, col: at?.col ?? 0, diff: willDiff || !!nextCompare, side: at?.side, comparePath: nextCompare?.path });
      }
      curPosRef.current = { path, line: at?.line ?? 1, col: at?.col ?? 0, side: at?.side };
      fileRef.current = f;
      setFile(f);
      setTabs((t) => (t.includes(path) ? t : [...t, path].slice(-30)));
      // The live picture / side chat needs the room: hide the tree for this session
      if (isAiTab(path) || isSideTab(path)) setTree(false);
      setSaveState(null);
      previewFor(path, at);
      setCompare(nextCompare);
      setGoto(at ? { ...at, n: Date.now() } : null);
      setShowDiff(willDiff);
      if (openedFromCache) void revalidateActiveCached(f, !!opts?.quiet, path);
      if (window.matchMedia('(max-width: 760px)').matches) setTreeOpen(false);
      return 'opened';
    } finally {
      if (openSeqRef.current === seq) setOpeningPath(null);
    }
  };
  const openFile = async (path0: string, diff = false, at?: { line: number; endLine?: number; col?: number; len?: number; side?: 'a' | 'b' | 'del-file' }, quiet = false): Promise<boolean> => {
    const res = await openFileInternal(path0, diff, at, quiet ? { quiet: true } : undefined);
    return res !== 'cancelled';
  };
  // A file link or context pill in chat (SessionActions.openFileInPanel): taken on mount too, since a closed panel mounts after the event.
  // Through a ref, so the listener always calls the openFile of the latest render
  const openFileRef = useRef(openFile);
  openFileRef.current = openFile;
  const openFileInternalRef = useRef(openFileInternal);
  openFileInternalRef.current = openFileInternal;
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
    const nextExpanded = new Set([...expandedRef.current, ...chain]);
    for (const d of chain.slice(0, -1)) if (!kids[d]) load(d, true);
    const unloadedSubs = visibleExpandedDirs(top, nextExpanded).filter((d) => d !== chain[chain.length - 1] && !kids[d]);
    if (unloadedSubs.length) void loadDirsBatched(unloadedSubs, treeGenRef.current);
    setExpanded(nextExpanded);
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
  const openQueueRef = useRef(Promise.resolve());
  useEffect(() => {
    let active = true;
    const drain = () => {
      const reqs = takeOpenFiles(hostId, sessionId);
      if (!reqs.length) return;
      openQueueRef.current = openQueueRef.current
        .catch(() => {})
        .then(async () => {
          for (const r of reqs) {
            if (!active) return;
            const isVirtual =
              isWebTab(r.path) || isAiTab(r.path) || isSideTab(r.path) || isReviewTab(r.path) || isUntitled(r.path) || isCommitTab(r.path);
            const likelyFile = !/[\\/]$/.test(r.path) && hasFileExt(r.path);
            if (!r.line && !r.diff && !isVirtual && !likelyFile && (await revealRef.current(r.path))) continue;
            const res = await openFileInternalRef.current(
              r.path,
              !!r.diff,
              r.line ? { line: r.line, endLine: r.endLine } : undefined,
              likelyFile && !r.line && !r.diff && !isVirtual ? { quiet: true } : undefined,
            );
            if (res === 'cancelled') break;
            if (res === 'failed' && likelyFile && !r.line && !r.diff && !isVirtual) {
              if (!(await revealRef.current(r.path))) {
                // Re-run loudly once to surface actual permission / missing-file toast if neither worked
                await openFileInternalRef.current(r.path, false);
                break;
              }
            }
          }
        });
    };
    drain();
    const on = (e: Event) => {
      const d = (e as CustomEvent<{ hostId: string; sessionId: string }>).detail;
      if (d.hostId === hostId && d.sessionId === sessionId) drain();
    };
    window.addEventListener(OPEN_FILE, on);
    return () => {
      active = false;
      window.removeEventListener(OPEN_FILE, on);
    };
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
      const res = await readFileWithMeta(next);
      const f = res?.file ?? null;
      fileRef.current = f;
      setFile(f);
      setShowDiff(false);
      previewFor(next);
      if (res?.fromCache) void revalidateActiveCached(res.file, false, next);
    } else {
      fileRef.current = null;
      setFile(null);
    }
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
    const res = await readFileWithMeta(t);
    const f = res?.file ?? null;
    fileRef.current = f;
    setFile(f);
    setShowDiff(false);
    previewFor(t);
    if (res?.fromCache) void revalidateActiveCached(res.file, false, t);
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
              : inRoot(t) || (isAbsPath(t) && !isCommitTab(t) && !isReviewTab(t) && !isOtherSessionUpload(t, sessionId)),
    );
    const cur = saved.current && keep.includes(saved.current) ? saved.current : keep[keep.length - 1];
    if (!cur) return;
    restoring.current = true;
    readFileWithMeta(cur, true).then((res) => {
      restoring.current = false;
      const f = res?.file ?? null;
      // If the user (or a file link) already opened a file while restoring, keep it but still bring back the saved tabs
      if (fileRef.current) {
        if (f) setTabs((t) => [...keep.filter((x) => !t.includes(x)), ...t].slice(-30));
        return;
      }
      if (!f) return setTabs(keep.filter((t) => t !== cur));
      fileRef.current = f;
      setTabs(keep);
      setFile(f);
      previewFor(cur);
      if (res?.fromCache) void revalidateActiveCached(f, true, cur);
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
      if (next) {
        readFileWithMeta(next, true).then((res) => {
          const f = res?.file ?? null;
          fileRef.current = f;
          setFile(f);
          if (res?.fromCache && f) void revalidateActiveCached(f, true, next);
        });
      } else {
        fileRef.current = null;
        setFile(null);
      }
    }
  }, [rawHostSessions, hostSessions]); // eslint-disable-line react-hooks/exhaustive-deps

  const reload = async () => {
    if (!file) return;
    dropCachedFile(file.path);
    const f = await readFile(file.path, false, true);
    if (f) {
      fileRef.current = f;
      setFile(f);
      setSaveState(null);
      setSaveError('');
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
      const nf = await readFile(path, false, true);
      const sameOpen = fileRef.current?.path === f.path && fileRef.current.rev === f.rev;
      const flushedDraft = sameOpen ? flushActiveEditor() : undefined;
      const latestDraft = sameOpen ? (flushedDraft ?? fileRef.current?.draft ?? f.draft) : undefined;
      const finalPath = nf ? unverbatim(nf.path) : path;
      untitled.delete(f.path);
      setTabs((t) => [...new Set(t.map((x) => (x === f.path ? finalPath : x)))]);
      // Stay in the editor even for Markdown / HTML (no switch to the preview)
      if (nf) {
        const savedFile = nf.path === finalPath ? nf : { ...nf, path: finalPath };
        const curNow = fileRef.current;
        const preservedDraft =
          curNow?.path === f.path && curNow.rev === f.rev
            ? (latestDraft !== undefined && latestDraft !== f.draft ? latestDraft : curNow.draft)
            : undefined;
        const nextF =
          preservedDraft !== undefined && preservedDraft !== f.draft
            ? { ...savedFile, draft: preservedDraft }
            : savedFile;
        if (curNow?.path === f.path && curNow.rev === f.rev) fileRef.current = nextF;
        setFile((cur) => {
          if (!cur || cur.path !== f.path || cur.rev !== f.rev) return cur;
          const d = cur.draft !== f.draft ? cur.draft : preservedDraft;
          return d !== undefined && d !== f.draft ? { ...savedFile, draft: d } : savedFile;
        });
      }
      setSaveState(null);
      refreshTree();
      toast(tx('saved', { v: baseName(path) }));
    } catch (e) {
      setSaveState(null);
      errorToast(e);
    }
  };

  const save = async (overwrite = false) => {
    const latestDraft = flushActiveEditor();
    const f = fileRef.current;
    const draftText = latestDraft ?? f?.draft ?? '';
    // As in VS Code, Ctrl+S on an untitled tab asks where to save it even when it's empty
    if (f?.untitled) return saveState === 'saving' ? undefined : saveUntitled({ ...f, draft: draftText });
    if (!f || draftText === f.content || saveState === 'saving') return;
    const targetPath = f.path;
    const targetRev = f.rev;
    setSaveState('saving');
    try {
      const r = await rpc<{ modified: number; size: number }>(hostId, 'fs.write', {
        path: targetPath,
        content: draftText,
        expectModified: overwrite ? undefined : f.modified,
        expectSize: overwrite ? undefined : f.size,
      });
      putCachedFile({ ...f, content: draftText, draft: draftText, modified: r.modified, size: r.size });
      const live = fileRef.current;
      if (live && live.path === targetPath && live.rev === targetRev) {
        fileRef.current = { ...live, content: draftText, modified: r.modified, size: r.size };
        setSaveState(null);
      }
      setFile((cur) =>
        cur && cur.path === targetPath && cur.rev === targetRev
          ? { ...cur, content: draftText, modified: r.modified, size: r.size }
          : cur,
      );
      loadGit(true);
      toast(tx('saved', { v: baseName(targetPath) }));
    } catch (e) {
      const msg = String((e as Error)?.message ?? e);
      if (fileRef.current?.path === targetPath && fileRef.current?.rev === targetRev) {
        setSaveError(msg);
        setSaveState(msg.includes('被修改过') ? 'conflict' : 'error');
      }
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
    if (f && f.draft === f.content && !hasUnsyncedEdits() && !f.image && !f.pdf && !f.pdfSrc && !f.web && !f.commit && !f.untitled) {
      dropCachedFile(f.path);
      rpc<Omit<OpenFile, 'draft' | 'rev'>>(hostId, 'fs.read', { path: f.path })
        .then((r) => {
          if (r.modified === f.modified && r.size === f.size && r.content === f.content) return;
          const nextF = { ...r, draft: r.content, rev: nextFileRev() };
          putCachedFile(nextF);
          const live = fileRef.current;
          if (!live || live.path !== f.path || live.rev !== f.rev || live.draft !== live.content || hasUnsyncedEdits()) {
            return;
          }
          fileRef.current = nextF;
          setFile((cur) =>
            cur && cur.path === f.path && cur.rev === f.rev && cur.draft === cur.content && !hasUnsyncedEdits() ? nextF : cur,
          );
        })
        .catch(() => {});
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status]);

  // An untitled tab's text stays in memory when the panel closes; flush debounced edits first
  const closePanel = async () => {
    const cur = fileRef.current;
    if (cur?.untitled) {
      flushActiveEditor();
      onClose();
      return;
    }
    const latestDraft = flushActiveEditor();
    const afterFlush = fileRef.current;
    const curDraft = latestDraft ?? afterFlush?.draft ?? '';
    if (!afterFlush || curDraft === afterFlush.content) {
      onClose();
      return;
    }
    const targetPath = afterFlush.path;
    const targetRev = afterFlush.rev;
    const ok = await askConfirm({
      title: tx('discardChangesTo', { v: nameOf(targetPath) }),
      message: tx('unsavedEditsWillBeDiscarded'),
      okText: tx('discard'),
      danger: true,
    });
    if (!ok) return;
    const nextRev = nextFileRev();
    if (fileRef.current && fileRef.current.path === targetPath && fileRef.current.rev === targetRev) {
      fileRef.current = { ...fileRef.current, draft: fileRef.current.content, rev: nextRev };
      setSaveState(null);
      setSaveError('');
    }
    setFile((c) =>
      c && c.path === targetPath && c.rev === targetRev ? { ...c, draft: c.content, rev: nextRev } : c,
    );
    onClose();
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
      clearFileCache();
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

  const isSameOrInside = (candidate: string, target: string) => {
    const nc = norm(candidate);
    const nt = norm(target);
    return nc === nt || nc.startsWith(nt + '/');
  };
  const isUntrackedGit = (f: GitFile) => f.status === 'new' || f.status === '??';
  const tabDiskPath = (t: string): string | null => {
    if (isSideTab(t) || isReviewTab(t) || isCommitTab(t) || isUntitled(t)) return null;
    if (isWebTab(t)) return webTabFile(t);
    return t;
  };
  const closeRemovedTabs = async (matchPath: (absPath: string) => boolean) => {
    const curTabs = openTabsRef.current;
    const isMatchedTab = (t: string) => {
      const dp = tabDiskPath(t);
      return dp !== null && matchPath(dp);
    };
    const cur = fileRef.current;
    const curPath = cur?.path;
    const closingCur = !!curPath && isMatchedTab(curPath);
    const rest = curTabs.filter((t) => !isMatchedTab(t));
    if (rest.length !== curTabs.length) {
      openTabsRef.current = rest;
      setTabs(rest);
    }
    if (secondRef.current && matchPath(secondRef.current.path)) setSecond(null);
    if (compareRef.current && matchPath(compareRef.current.path)) setCompare(null);
    if (closingCur) {
      const i = curTabs.indexOf(curPath);
      const next = i >= 0 ? rest[Math.min(i, rest.length - 1)] : rest[rest.length - 1];
      if (next) {
        const nf = await readFile(next, true);
        setFile(nf);
        setShowDiff(false);
        previewFor(next);
      } else {
        setFile(null);
      }
      setCompare(null);
      setSaveState(null);
    }
  };

  const remove = async (path: string, isDir: boolean) => {
    const name = baseName(path);
    const ok = await askConfirm({
      title: isDir ? tx('deleteFolder', { name }) : tx('delete', { name }),
      message: isDir
        ? tx('thisCannotBeUndoneFolder', { path })
        : tx('thisCannotBeUndone', { path }),
      okText: tx('delete2'),
      danger: true,
    });
    if (!ok) return;
    try {
      await rpc(hostId, 'fs.delete', isDir ? { path, recursive: true } : { path });
      if (isDir) clearFileCache();
      else dropCachedFile(path);
      await closeRemovedTabs(isDir ? (p) => isSameOrInside(p, path) : (p) => norm(p) === norm(path));
      if (isDir) {
        setExpanded((x) => new Set([...x].filter((d) => !isSameOrInside(d, path))));
        setKids((k) => {
          const next = { ...k };
          for (const key of Object.keys(next)) {
            if (isSameOrInside(key, path)) delete next[key];
          }
          return next;
        });
      }
      refreshTree();
      if (diffScope === 'branch') loadBranchDiff();
    } catch (err) {
      errorToast(err);
    }
  };

  const rel = (p: string) => {
    if (!root) return p;
    const nr = norm(root);
    const np = norm(p);
    if (np === nr) return '.';
    if (np.startsWith(nr + '/')) return p.slice(root.replace(/[\\/]+$/, '').length).replace(/^[\\/]/, '').replace(/\\/g, '/') || '.';
    return p;
  };
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
  const uncommittedInDir = (dirPath: string): GitFile[] => {
    if (!git) return [];
    const prefix = norm(dirPath) + '/';
    const out: GitFile[] = [];
    for (const [k, f] of git.files) {
      if (k.startsWith(prefix)) out.push(f);
    }
    return out;
  };
  const discardPathsOf = (files: GitFile[]): string[] => {
    const seen = new Set<string>();
    const out: string[] = [];
    for (const f of files) {
      for (const p of f.oldPath ? [f.rel, f.oldPath] : [f.rel]) {
        if (!seen.has(p)) {
          seen.add(p);
          out.push(p);
        }
      }
    }
    return out;
  };
  const doDiscardFiles = async (files: GitFile[], title: string) => {
    if (!files.length || discarding) return;
    const untrackedCount = files.filter(isUntrackedGit).length;
    const trackedCount = files.length - untrackedCount;
    const message =
      untrackedCount === files.length
        ? untrackedCount === 1
          ? tx('discardUntrackedFileWarn')
          : tx('discardUntrackedManyWarn', { n: untrackedCount })
        : untrackedCount === 0
          ? tx('discardTrackedWarn')
          : tx('discardMixedWarn', { tracked: trackedCount, untracked: untrackedCount });
    const ok = await askConfirm({
      title,
      message,
      okText: tx('discard'),
      danger: true,
    });
    if (!ok) return;
    setDiscarding(true);
    try {
      await rpc(hostId, 'git.discard', { cwd, paths: discardPathsOf(files) });
      toast(tx('discardedChanges'));
      clearFileCache();
      const removedNorms = new Set(
        files.filter((f) => isUntrackedGit(f) || f.status.startsWith('A') || !!f.oldPath).map((f) => norm(f.path)),
      );
      if (removedNorms.size) {
        await closeRemovedTabs((p) => removedNorms.has(norm(p)));
      }
      const cur = fileRef.current;
      if (cur && !cur.web && !cur.commit && !cur.review && !cur.side && !cur.untitled) {
        const curNorm = norm(cur.path);
        if (files.some((f) => norm(f.path) === curNorm && !removedNorms.has(curNorm))) {
          const next = await readFile(cur.path, true, true);
          if (next) {
            fileRef.current = next;
            setFile(next);
            setShowDiff(false);
            setSaveState(null);
          } else {
            await closeRemovedTabs((p) => norm(p) === curNorm);
          }
        }
      }
      refreshTree();
      if (diffScope === 'branch') loadBranchDiff();
    } catch (e) {
      errorToast(e);
    } finally {
      setDiscarding(false);
    }
  };
  const discardFile = (f: GitFile) => void doDiscardFiles([f], tx('discardGitChangesTitle', { v: baseName(f.path) }));
  const discardFolder = (dirPath: string, displayName?: string) => {
    const files = uncommittedInDir(dirPath);
    if (!files.length) return;
    const label = displayName || baseName(dirPath);
    const title =
      files.length === 1
        ? tx('discardFolderChangesSingleTitle', { v: label })
        : tx('discardFolderChangesTitle', { v: label, n: files.length });
    void doDiscardFiles(files, title);
  };
  const discardAllUncommitted = () => {
    if (!git || !uncommittedKeys.length) return;
    const files = uncommittedKeys.map((k) => git.files.get(k)).filter((f): f is GitFile => !!f);
    if (!files.length) return;
    if (files.length === 1) return discardFile(files[0]);
    void doDiscardFiles(files, tx('discardAllChangesTitle', { n: files.length }));
  };
  const discardRow = (r: TreeRow) => {
    if (r.isDir) return discardFolder(r.path, r.name);
    const g = gitOf(r.path);
    if (g) discardFile(g);
  };

  const refreshGitAll = (withDetails = true) => {
    refreshTree();
    loadWorktrees(withDetails);
    void loadBranches();
    if (diffScope === 'branch') loadBranchDiff();
  };

  const openCommit = (pushDefault = preferCommitPush) => {
    if (!branchesInfo) void loadBranches();
    setCommitDefaultPush(pushDefault);
    setCommitOpen(true);
  };

  const doPush = async () => {
    if (pushing || gitBusy || !git) return;
    setPushing(true);
    try {
      const r = await rpc<{ branch: string; remote: string }>(hostId, 'git.push', { cwd, setUpstream: true });
      toast(tx('pushedBranch', { branch: r.branch, remote: r.remote }));
      refreshGitAll(bwOpen);
    } catch (e) {
      errorToast(e);
    } finally {
      setPushing(false);
    }
  };

  const doFetch = async () => {
    if (fetching || !git) return;
    setFetching(true);
    try {
      await rpc(hostId, 'git.fetch', { cwd, prune: true });
      toast(tx('fetchedRemotesToast'));
      loadWorktrees(true);
      void loadBranches();
    } catch (e) {
      errorToast(e);
    } finally {
      setFetching(false);
    }
  };

  const doPull = async (rebase = false) => {
    if (gitBusy || !git) return;
    setGitBusy(true);
    try {
      const r = await rpc<{ branch: string }>(hostId, 'git.pull', { cwd, rebase });
      toast(tx('pulledBranchToast', { branch: r?.branch || git.branch }));
      refreshGitAll(true);
    } catch (e) {
      errorToast(e);
    } finally {
      setGitBusy(false);
    }
  };

  const toggleBwPopover = () => {
    if (bwOpen) {
      setBwOpen(false);
      return;
    }
    setBwOpen(true);
    setBranchesLoading(true);
    loadWorktrees(true);
    void loadBranches().finally(() => setBranchesLoading(false));
  };

  const switchWorktree = (targetPath: string) => {
    setNewWorktree(null);
    if (onSwitchCwd && norm(targetPath) !== norm(cwd)) onSwitchCwd(targetPath);
  };

  const checkoutBranch = async (name: string) => {
    try {
      const r = await rpc<{ branch?: string }>(hostId, 'git.checkout', { cwd, branch: name });
      toast(tx('switchedToBranch', { branch: r?.branch || name }));
      refreshGitAll(true);
    } catch (err) {
      errorToast(err);
    }
  };

  const createBranch = async (base?: string) => {
    const name = (
      await askText({
        title: base ? tx('newBranchFromTitle', { base }) : tx('newBranch'),
        placeholder: tx('newBranchPlaceholder'),
      })
    )?.trim();
    if (!name) return;
    try {
      await rpc(hostId, 'git.checkout', { cwd, branch: name, create: true, ...(base ? { base } : {}) });
      toast(tx('switchedToBranch', { branch: name }));
      refreshGitAll(true);
    } catch (err) {
      errorToast(err);
    }
  };

  const openNewWorktree = (base?: string) => {
    setNewWtBase(base ?? null);
    setNewWtOpen(true);
  };

  const doOpenBranchInWorktree = async (branch: string) => {
    const slug = worktreeSlug(branch, worktrees.map((w) => w.path));
    setGitBusy(true);
    try {
      const res = await rpc<{ path: string; branch: string }>(hostId, 'git.worktreeAdd', {
        cwd,
        name: slug,
        branch,
        existingBranch: true,
        prepareEnv: true,
      });
      toast(tx('createdWorktreeToast', { branch: res.branch || branch }));
      loadWorktrees(true);
      void loadBranches();
      if (onSwitchCwd) switchWorktree(res.path);
    } catch (err) {
      errorToast(err);
    } finally {
      setGitBusy(false);
    }
  };

  const effDefaultBranch = branchesInfo?.defaultBranch || wtDefaultBranch || '';

  const doMergeToDefault = async (sourceBranch: string, squash: boolean) => {
    const target = effDefaultBranch;
    if (!target || gitBusy) return;
    let message: string | undefined;
    if (squash) {
      const m = (
        await askText({
          title: tx('squashCommitMsgTitle', { source: sourceBranch, target }),
          label: tx('squashCommitMsgLabel'),
          value: `feat: ${sourceBranch}`,
        })
      )?.trim();
      if (!m) return;
      message = m;
    } else {
      const ok = await askConfirm({
        title: tx('mergeBackConfirmTitle', { source: sourceBranch, target }),
        message: tx('mergeBackConfirmMsg', { target }),
        okText: tx('mergeBackIntoShort', { target }),
      });
      if (!ok) return;
    }
    setGitBusy(true);
    try {
      const r = await rpc<{ alreadyUpToDate?: boolean }>(hostId, 'git.merge', {
        cwd,
        source: sourceBranch,
        target,
        squash,
        ...(message ? { message } : {}),
      });
      toast(r?.alreadyUpToDate ? tx('mergeAlreadyUpToDateToast', { source: sourceBranch, target }) : tx('mergedBranchToast', { source: sourceBranch, target }));
      refreshGitAll(true);
    } catch (err) {
      errorToast(err);
    } finally {
      setGitBusy(false);
    }
  };

  const doMergeIntoCurrent = async (sourceBranch: string) => {
    if (!git || gitBusy) return;
    const ok = await askConfirm({
      title: tx('mergeIntoCurrentConfirm', { source: sourceBranch, target: git.branch || 'HEAD' }),
      okText: tx('mergeIntoCurrentBranch'),
    });
    if (!ok) return;
    setGitBusy(true);
    try {
      const r = await rpc<{ alreadyUpToDate?: boolean }>(hostId, 'git.merge', { cwd, source: sourceBranch });
      toast(
        r?.alreadyUpToDate
          ? tx('mergeAlreadyUpToDateToast', { source: sourceBranch, target: git.branch || 'HEAD' })
          : tx('mergedBranchToast', { source: sourceBranch, target: git.branch || 'HEAD' }),
      );
      refreshGitAll(true);
    } catch (err) {
      errorToast(err);
    } finally {
      setGitBusy(false);
    }
  };

  const doRebaseOnto = async (onto: string) => {
    if (!git || gitBusy) return;
    const ok = await askConfirm({
      title: tx('rebaseOntoConfirmTitle', { branch: git.branch || 'HEAD', onto }),
      message: tx('rebaseOntoConfirmMsg', { onto }),
      okText: tx('rebaseOntoBranch'),
    });
    if (!ok) return;
    setGitBusy(true);
    try {
      await rpc(hostId, 'git.rebase', { cwd, onto });
      toast(tx('rebasedBranchToast', { branch: git.branch || 'HEAD', onto }));
      refreshGitAll(true);
    } catch (err) {
      errorToast(err);
    } finally {
      setGitBusy(false);
    }
  };

  const doCompareBranch = (branch: string) => {
    setView('changes');
    lsSet(viewKey, 'changes');
    setBranchBase(branch);
    setDiffScope('branch');
  };

  const doRenameBranch = async (oldName: string) => {
    const newName = (
      await askText({
        title: tx('renameBranchTitle', { branch: oldName }),
        value: oldName,
        placeholder: tx('newBranchPlaceholder'),
        okText: tx('save'),
      })
    )?.trim();
    if (!newName || newName === oldName) return;
    try {
      const r = await rpc<{ oldBranch: string; branch: string; changed: boolean }>(hostId, 'git.renameBranch', {
        cwd,
        oldBranch: oldName,
        newBranch: newName,
      });
      toast(tx('renamedBranchToast', { branch: r.branch || newName }));
      refreshGitAll(true);
    } catch (err) {
      errorToast(err);
    }
  };

  const doDeleteBranch = async (branch: string) => {
    const ok = await askConfirm({
      title: tx('deleteBranchConfirmTitle', { branch }),
      message: tx('deleteBranchConfirmMsg'),
      okText: tx('delete2'),
      danger: true,
    });
    if (!ok) return;
    try {
      await rpc(hostId, 'git.branchDelete', { cwd, branch, force: false });
      toast(tx('deletedBranchToast', { branch }));
      loadWorktrees(true);
      void loadBranches();
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      if (/not fully merged|git branch -D|未完全合并/i.test(msg)) {
        const forceOk = await askConfirm({
          title: tx('forceDeleteBranchTitle', { branch }),
          message: tx('forceDeleteBranchMsg'),
          okText: tx('delete2'),
          danger: true,
        });
        if (!forceOk) return;
        try {
          await rpc(hostId, 'git.branchDelete', { cwd, branch, force: true });
          toast(tx('deletedBranchToast', { branch }));
          loadWorktrees(true);
          void loadBranches();
        } catch (err2) {
          errorToast(err2);
        }
      } else {
        errorToast(err);
      }
    }
  };

  const doDeleteWorktree = async (wt: WorktreeEntry) => {
    const label = wt.branch || baseName(wt.path);
    const hasRunning = hostSessions.some(
      (s) =>
        isHostSession(s) &&
        !s.sideChat &&
        !s.review &&
        !s.parentId &&
        norm(normDirPath(s.cwd)) === norm(wt.path) &&
        (s.status === 'running' || s.status === 'starting' || s.status === 'waiting-permission'),
    );
    const dirtyCount = wt.dirtyCount ?? 0;
    const warns: string[] = [];
    if (hasRunning) warns.push(tx('deleteWorktreeRunningWarn'));
    if (dirtyCount > 0) warns.push(tx('deleteWorktreeDirtyWarn', { n: dirtyCount }));
    warns.push(wt.merged ? tx('deleteWorktreeMergedMsg') : tx('deleteWorktreeSafeMsg'));
    const ok = await askConfirm({
      title: tx('deleteWorktreeConfirmTitle', { name: label }),
      message: warns.join('\n'),
      okText: tx('delete2'),
      danger: true,
    });
    if (!ok) return;
    const isViewingTarget = norm(wt.path) === norm(cwd);
    const mainWt = worktrees.find((w) => w.isMain);
    const fallbackCwd = baseCwd && norm(baseCwd) !== norm(wt.path) ? baseCwd : mainWt && norm(mainWt.path) !== norm(wt.path) ? mainWt.path : cwd;
    const force = dirtyCount > 0 || hasRunning;
    try {
      await rpc(hostId, 'git.worktreeRemove', {
        cwd: fallbackCwd,
        path: wt.path,
        force,
        deleteBranch: !!wt.merged,
      });
      toast(tx('deletedWorktreeToast', { name: label }));
      if (isViewingTarget && onSwitchCwd && norm(fallbackCwd) !== norm(cwd)) {
        switchWorktree(fallbackCwd);
      } else {
        loadWorktrees(true);
        void loadBranches();
      }
    } catch (err) {
      errorToast(err);
    }
  };

  const doCleanupMergedWorktrees = async (candidates: WorktreeEntry[]) => {
    const runningDirs = new Set(
      hostSessions
        .filter(
          (s) =>
            isHostSession(s) &&
            !s.sideChat &&
            !s.review &&
            !s.parentId &&
            (s.status === 'running' || s.status === 'starting' || s.status === 'waiting-permission'),
        )
        .map((s) => norm(normDirPath(s.cwd))),
    );
    const safe = candidates.filter(
      (w) =>
        !w.isMain &&
        norm(w.path) !== norm(cwd) &&
        !runningDirs.has(norm(w.path)) &&
        (w.prunable || (!!w.merged && (w.dirtyCount ?? 0) === 0)),
    );
    const skippedCount = candidates.length - safe.length;
    if (!safe.length) {
      toast(tx('noSafeCleanupToast'), 'info');
      return;
    }
    const ok = await askConfirm({
      title: tx('cleanupMergedConfirmTitle', { n: safe.length }),
      message: tx('cleanupMergedConfirmMsg', { n: safe.length, skipped: skippedCount }),
      okText: tx('oneClickCleanup'),
      danger: true,
    });
    if (!ok) return;
    try {
      const r = await rpc<{ removed: string[]; skipped: { path: string; reason: string }[] }>(hostId, 'git.worktreePrune', {
        cwd,
        paths: safe.map((w) => w.path),
        deleteBranches: true,
      });
      const remN = r?.removed?.length ?? 0;
      const skipN = (r?.skipped?.length ?? 0) + skippedCount;
      toast(
        skipN > 0
          ? tx('cleanupMergedResultWithSkippedToast', { removed: remN, skipped: skipN })
          : tx('cleanupMergedResultToast', { removed: remN }),
      );
      loadWorktrees(true);
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
    const f = await readFileValidated(path);
    if (!f) return;
    setSecond(f);
    // Three columns is too cramped: collapse the tree first (not saved in settings); the top-right button reopens it
    setTreeOpen(false);
  };
  const compareWith = async (path: string) => {
    if (!file || path === file.path) return;
    if (file.binary) return toast(tx('currentFileIsNotText'));
    const f = await readFileValidated(path);
    if (!f) return;
    if (f.binary) return toast(tx('isNotATextFile', { v: baseName(path) }));
    flushActiveEditor();
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
    const dirGitFiles = r.isDir ? uncommittedInDir(r.path) : [];
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
        ? ['-' as const, { label: tx('discardFileChanges'), icon: 'rotate-l', danger: true, run: () => discardFile(rowGit) }]
        : dirGitFiles.length > 0
          ? ['-' as const, { label: tx('discardFolderChangesCount', { n: dirGitFiles.length }), icon: 'rotate-l', danger: true, run: () => discardFolder(r.path, r.name) }]
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
  const stepHistory = useCallback(
    (delta: -1 | 1) => {
      navStepChainRef.current = navStepChainRef.current.then(async () => {
        const h = navHistRef.current;
        if (h.index < 0) return;
        const entriesSnapshot = h.entries;
        const prevIdx = h.index;
        const nextIdx = prevIdx + delta;
        if (nextIdx < 0 || nextIdx >= h.entries.length) return;
        const curFile = fileRef.current;
        let updatedPrev: NavPoint | null = null;
        if (curFile && h.entries[prevIdx] && norm(h.entries[prevIdx].path) === norm(curFile.path)) {
          if (norm(curPosRef.current.path) === norm(curFile.path)) {
            updatedPrev = {
              ...h.entries[prevIdx],
              line: curPosRef.current.line,
              col: curPosRef.current.col,
              side: curPosRef.current.side,
            };
          }
        }
        const target = h.entries[nextIdx];
        cancelPendingNav();
        navSteppingRef.current = true;
        let res: 'opened' | 'cancelled' | 'failed' = 'failed';
        try {
          res = await openFileInternalRef.current(
            target.path,
            !!target.diff,
            { line: target.line, col: target.col, side: target.side },
            { skipHistory: true, comparePath: target.comparePath },
          );
        } finally {
          navSteppingRef.current = false;
        }
        if (res === 'opened' && navHistRef.current.entries === entriesSnapshot && navHistRef.current.index === prevIdx) {
          if (updatedPrev) navHistRef.current.entries[prevIdx] = updatedPrev;
          navHistRef.current.index = nextIdx;
          syncNavState();
        }
      });
      return navStepChainRef.current;
    },
    [cancelPendingNav, syncNavState],
  );
  const stepHistoryRef = useRef(stepHistory);
  stepHistoryRef.current = stepHistory;

  const jumpToNavTarget = useCallback(
    (origin: NavPoint, targetAbsPath: string, line: number, col: number, len = 0) => {
      navStepChainRef.current = navStepChainRef.current.then(async () => {
        cancelPendingNav();
        navSteppingRef.current = true;
        let res: 'opened' | 'cancelled' | 'failed' = 'failed';
        try {
          res = await openFileInternalRef.current(targetAbsPath, false, { line, col, len }, { skipHistory: true });
        } finally {
          navSteppingRef.current = false;
        }
        if (res === 'opened') {
          recordNavJump(origin, { path: targetAbsPath, line, col, diff: false, side: 'b' });
        }
      });
    },
    [cancelPendingNav, recordNavJump],
  );

  const handleCodeNav = useCallback(
    async (
      trigger: NavTrigger,
      navPath: string,
      originPath: string,
      source: 'main' | 'second' | 'diff',
      originRev?: string,
      comparePath?: string,
    ) => {
      cancelPendingNav();
      const reqId = navReqIdRef.current;

      if (trigger.token.kind === 'url') {
        openUrl(trigger.token.text);
        return;
      }

      const expectedDiff = source === 'diff';
      const isOriginStillActive = () => {
        if (navReqIdRef.current !== reqId) return false;
        if (source === 'second') {
          return norm(secondRef.current?.path ?? '') === norm(originPath) && !showDiffRef.current && !compareRef.current;
        }
        const curF = fileRef.current;
        if (!curF || norm(curF.path) !== norm(originPath)) return false;
        const curIsDiff = showDiffRef.current || !!curF.commit || !!compareRef.current;
        if (curIsDiff !== expectedDiff) return false;
        if (comparePath != null && norm(compareRef.current?.path ?? '') !== norm(comparePath)) return false;
        return true;
      };

      const workspaceRoot = root ?? cwd;
      const origin: NavPoint = {
        path: originPath,
        line: trigger.token.line,
        col: trigger.token.col,
        diff: expectedDiff,
        side: trigger.side,
        comparePath,
        rev: originRev,
      };
      const isUnknownMethod = (e: unknown) => {
        const msg = String((e as Error)?.message ?? e).toLowerCase();
        return msg.includes('未知方法') || msg.includes('unknown method');
      };
      const charSliceToUtf16 = (txt: string, charStart: number, charLen: number): [number, number] => {
        const chars = [...txt];
        const s = Math.max(0, Math.min(chars.length, charStart));
        const e = Math.max(s, Math.min(chars.length, s + Math.max(0, charLen)));
        return [chars.slice(0, s).join('').length, chars.slice(s, e).join('').length];
      };

      const runReferences = async (symbol: string) => {
        if (!isOriginStillActive()) return;
        setNavPeek({ symbol, loading: true, result: null, coords: trigger.coords, origin });
        try {
          let res: NavReferencesResult;
          try {
            res = await rpc<NavReferencesResult>(hostId, 'nav.references', {
              root: workspaceRoot,
              path: navPath,
              line: trigger.token.line,
              col: trigger.token.col,
              symbol,
              content: trigger.content,
              includeDeclaration: false,
              contextLines: 3,
              limit: 200,
            });
          } catch (e) {
            if (!isUnknownMethod(e)) throw e;
            // Fallback for older daemons using existing fs.grep
            const g = await rpc<{ hits: { path: string; line: number; col: number; len: number; offset: number; text: string; matches?: [number, number, string][] }[]; truncated: boolean; files: number }>(
              hostId,
              'fs.grep',
              { root: workspaceRoot, query: symbol, caseSensitive: true, wholeWord: true, regex: false, limit: 200 },
            );
            const hits: NavReferenceHit[] = [];
            for (const h of g.hits) {
              const cols = h.matches?.length ? h.matches.map((m) => [m[0] - h.offset, m[1]] as const) : [[h.col, h.len] as const];
              for (const [charCol, charLen] of cols) {
                const [c16, l16] = charSliceToUtf16(h.text, charCol, charLen);
                hits.push({
                  path: h.path,
                  absPath: join(workspaceRoot, h.path.split('/').join(sepOf(workspaceRoot))),
                  line: h.line,
                  col: c16,
                  len: l16,
                  text: h.text,
                  isDefinition: false,
                  contextStartLine: h.line,
                  context: [h.text],
                });
              }
            }
            res = {
              provider: 'grep-fallback',
              symbol,
              truncated: g.truncated,
              totalFiles: g.files,
              totalHits: hits.length,
              hits,
            };
          }
          if (!isOriginStillActive()) return;
          if (!res.hits.length) {
            setNavPeek(null);
            showNavNotice(tx('navNoReferences', { v: symbol }));
            return;
          }
          setNavPeek({ symbol, loading: false, result: res, coords: trigger.coords, origin });
        } catch (e) {
          if (!isOriginStillActive()) return;
          setNavPeek(null);
          errorToast(e);
        }
      };

      if (trigger.mode === 'references') {
        await runReferences(trigger.token.text);
        return;
      }

      try {
        let res: NavDefinitionResult;
        try {
          res = await rpc<NavDefinitionResult>(hostId, 'nav.definition', {
            root: workspaceRoot,
            path: navPath,
            line: trigger.token.line,
            col: trigger.token.col,
            symbol: trigger.token.kind === 'symbol' ? trigger.token.text : undefined,
            kind: trigger.token.kind,
            pathText: trigger.token.kind === 'path' ? trigger.token.text : undefined,
            container: trigger.token.container,
            content: trigger.content,
            limit: 30,
          });
        } catch (e) {
          if (!isUnknownMethod(e)) throw e;
          // Graceful fallback for older daemons: current-buffer outline/line scan or fs.search
          if (trigger.token.kind === 'path') {
            const cleaned = trigger.token.text.replace(/^mod:/, '').replace(/:\d+(?::\d+)?$/, '');
            const base = cleaned.slice(cleaned.lastIndexOf('/') + 1);
            const found = await rpc<{ path: string; isDir: boolean }[]>(hostId, 'fs.search', { root: workspaceRoot, query: base, limit: 15 });
            const locs: NavLocation[] = found
              .filter((x) => !x.isDir)
              .map((x) => ({
                path: x.path,
                absPath: join(workspaceRoot, x.path.split('/').join(sepOf(workspaceRoot))),
                line: 1,
                col: 0,
                len: 0,
                kind: 'file',
                symbol: base,
                preview: x.path,
                score: 500,
                source: 'path',
              }));
            res = { provider: 'fallback', kind: 'path', query: trigger.token.text, atDefinition: false, locations: locs };
          } else {
            const syms = outline(baseName(navPath), trigger.content).filter((s) => s.name === trigger.token.text);
            const atDef = syms.some((s) => s.line === trigger.token.line);
            const locs: NavLocation[] = syms
              .filter((s) => !(atDef && s.line === trigger.token.line))
              .map((s) => ({
                path: rel(navPath),
                absPath: navPath,
                line: s.line,
                col: 0,
                len: s.name.length,
                kind: s.kind,
                symbol: s.name,
                preview: trigger.content.split('\n')[s.line - 1]?.trim() ?? s.name,
                score: 900,
                source: 'same-file',
              }));
            res = { provider: 'fallback', kind: 'symbol', query: trigger.token.text, atDefinition: atDef, locations: locs };
          }
        }
        if (!isOriginStillActive()) return;

        if (res.atDefinition) {
          if ((trigger.mode === 'click' || res.locations.length === 0) && trigger.token.kind === 'symbol') {
            await runReferences(trigger.token.text);
            return;
          }
          if (res.locations.length === 1) {
            const loc = res.locations[0];
            jumpToNavTarget(origin, loc.absPath, loc.line, loc.col, loc.len);
            return;
          }
          if (res.locations.length > 1) {
            setNavPicker({ symbol: trigger.token.text, locations: res.locations, coords: trigger.coords, origin });
            return;
          }
        }

        if (res.locations.length === 0) {
          showNavNotice(
            trigger.token.kind === 'path'
              ? tx('navPathNotFound', { v: trigger.token.text })
              : tx('navDefNotFound', { v: trigger.token.text }),
          );
          return;
        }

        const first = res.locations[0];
        const secondLoc = res.locations[1];
        const highConfidenceSingle =
          res.locations.length === 1 ||
          first.source === 'import' ||
          first.source === 'local' ||
          (first.score >= 900 && (!secondLoc || first.score - secondLoc.score >= 120));
        if (highConfidenceSingle) {
          jumpToNavTarget(origin, first.absPath, first.line, first.col, first.len);
          return;
        }

        setNavPicker({ symbol: trigger.token.text, locations: res.locations, coords: trigger.coords, origin });
      } catch (e) {
        if (!isOriginStillActive()) return;
        errorToast(e);
      }
    },
    [hostId, root, cwd, cancelPendingNav, jumpToNavTarget, showNavNotice],
  );

  /** Right-click menu inside the editor (VS Code's editor context menu, plus Cursor's "add selection to chat") */
  const editorMenu = (path: string, commitHash?: string) => (c: EditorMenuCtx): (MenuItem | '-')[] => {
    const mod = IS_MAC ? '⌘' : 'Ctrl+';
    const onDisk = !isUntitled(path) && !commitHash;
    const displayPath = commitHash ? path : isUntitled(path) ? nameOf(path) : rel(path);
    const lines = c.fromLine === c.toLine ? `${c.fromLine}` : `${c.fromLine}-${c.toLine}`;
    const navItems: (MenuItem | '-')[] =
      !isUntitled(path)
        ? [
            { label: tx('goToDefinition'), icon: 'locate', hint: 'F12', disabled: !c.canNav, run: () => c.gotoDefinition?.() },
            { label: tx('findAllReferences'), icon: 'search', hint: 'Shift+F12', disabled: !c.canFindRefs, run: () => c.findReferences?.() },
            '-',
          ]
        : [];
    return [
      ...navItems,
      { label: tx('cut'), icon: 'cut', hint: `${mod}X`, disabled: !c.selection || c.readOnly, run: c.cut },
      { label: tx('copy'), icon: 'copy', hint: `${mod}C`, disabled: !c.selection, run: c.copy },
      { label: tx('paste'), icon: 'paste', hint: `${mod}V`, disabled: c.readOnly, run: c.paste },
      { label: tx('selectAll'), hint: `${mod}A`, run: c.selectAll },
      '-',
      { label: tx('find'), icon: 'search', hint: `${mod}F`, run: c.find },
      { label: tx('goToLine'), hint: `${mod}G`, run: () => openQuick(':') },
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
          { label: tx('goToSymbol'), icon: 'search', hint: `${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}Shift+O`, run: () => openQuick('@') },
          { label: tx('goToLine'), hint: `${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}G`, run: () => openQuick(':') },
          '-',
          { label: tx('saveAs'), icon: 'save', run: () => save() },
          { label: tx('copyFileContents'), icon: 'copy', run: () => copy(file.draft, tx('fileContentsCopied')) },
          '-',
          { label: wrap ? tx('disableWordWrap') : tx('enableWordWrap'), icon: 'wrap', hint: 'Alt+Z', run: toggleWrap },
        ]
      : file
      ? [
          { label: tx('goToSymbol'), icon: 'search', hint: `${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}Shift+O`, run: () => openQuick('@') },
          { label: tx('goToLine'), hint: `${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}G`, run: () => openQuick(':') },
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

  // ---------- Shortcuts: Ctrl+P quick open, Ctrl+Shift+O symbols, Ctrl+G go to line, Ctrl+Alt+N new untitled file, nav history ----------
  useEffect(() => {
    if (!visible) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.isComposing || e.keyCode === 229 || getState().palette) return;
      if (!IS_MAC && e.altKey && !e.ctrlKey && !e.metaKey && !e.shiftKey && (e.key === 'ArrowLeft' || e.key === 'ArrowRight')) {
        if (!fileRef.current) return;
        e.preventDefault();
        void stepHistoryRef.current(e.key === 'ArrowLeft' ? -1 : 1);
        return;
      }
      if (IS_MAC && e.ctrlKey && !e.metaKey && !e.altKey && (e.key === '-' || e.key === '_' || e.code === 'Minus')) {
        if (!fileRef.current) return;
        e.preventDefault();
        void stepHistoryRef.current(e.shiftKey ? 1 : -1);
        return;
      }
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
      openQuick(m);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [visible, openQuick]);

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
  const activeDispositions = useMemo(
    () => (activeReviewId ? getAllDispositions(hostId, activeReviewId) : {}),
    [hostId, activeReviewId, reviewVer], // eslint-disable-line react-hooks/exhaustive-deps
  );
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
  useBack(visible && !!file && !treeOpen && window.matchMedia('(max-width: 760px)').matches, () => setTreeOpen(true));
  useBack(visible && quick !== null, () => setQuick(null));
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
    flushActiveEditor();
    const v = diffLayout === 'split' ? 'unified' : 'split';
    lsSet('wp-diff-mode', v);
    setDiffLayout(v);
  };
  const viewing = compare ? 'compare' : showDiff ? 'diff' : 'file';

  // ---------- Full-file diff (VS Code style): both whole files, falling back to the hunk-only diff ----------
  /** Revisions to fetch for the current view: working tree vs HEAD/merge-base, turn checkpoint pair, or commit vs parent */
  const isTurnSnapshot = diffScope === 'turn' && !!activeTurnCkpt?.endRef;
  mergeEditableRef.current =
    !!file && viewing === 'diff' && !file.commit && !isTurnSnapshot && !file.deleted && !file.truncated && !file.notUtf8 && !file.binary;
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
          ? [[diffBaseRev, !isTurnSnapshot && diffBaseRev === 'HEAD' ? (curGit.oldPath ?? curGit.rel) : curGit.rel]]
          : [];
  const blobWant = blobSpecs.map(([r, p]) => blobKey(r, p)).join('|');
  useEffect(() => {
    const gen = gitCacheGenRef.current;
    for (const [r, p] of blobSpecs) {
      const k = blobKey(r, p);
      const inflightKey = `${gen}\n${hostId}\n${cwd}\n${k}`;
      if (k in blobs || blobInFlightRef.current.has(inflightKey)) continue;
      blobInFlightRef.current.add(inflightKey);
      const revArg = r === 'HEAD' && git?.head ? git.head : r;
      rpc<GitBlob>(hostId, 'git.blob', { cwd, path: p, rev: revArg })
        .then((b) => {
          if (gitCacheGenRef.current !== gen) return;
          setBlobs((m) => ({ ...m, [k]: b }));
        })
        // Older yi-agentd without git.blob, or a read error: use the hunk-only diff
        .catch(() => {
          if (gitCacheGenRef.current !== gen) return;
          setBlobs((m) => ({ ...m, [k]: null }));
        })
        .finally(() => blobInFlightRef.current.delete(inflightKey));
    }
  }, [hostId, cwd, blobWant, blobs, git?.head]); // eslint-disable-line react-hooks/exhaustive-deps
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
    flushActiveEditor();
    lsSet('wp-diff-fold', !diffCollapse);
    setDiffCollapse(!diffCollapse);
  };
  const mergeView = (
    f: { a: string; b: string },
    name: string,
    opts?: {
      editable?: boolean;
      refPath?: string;
      pathA?: string;
      pathB?: string;
      revA?: string;
      revB?: string;
      comparePath?: string;
      commitHash?: string;
    },
  ) => {
    const refPath = opts?.refPath ?? (file?.commit ? file.commit.rel : file?.path ?? name);
    const pathA = opts?.pathA ?? refPath;
    const pathB = opts?.pathB ?? refPath;
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
          docKey={file ? `${file.path}#${file.rev}#${compare?.path ?? ''}#${diffBaseRev}#${diffEndRev}` : name}
          a={f.a}
          b={f.b}
          filename={name}
          layout={layout}
          collapse={diffCollapse}
          wrap={wrap}
          readOnly={!editable}
          goto={goto}
          onChange={
            editable && file
              ? (() => {
                  const targetPath = file.path;
                  const targetRev = file.rev;
                  const isUnt = !!file.untitled;
                  return (draft) => updateFileDraft(targetPath, targetRev, isUnt, draft);
                })()
              : undefined
          }
          onSave={editable ? () => save() : undefined}
          onToggleWrap={toggleWrap}
          sideAFilename={baseName(pathA)}
          sideBFilename={baseName(pathB)}
          menu={(c) => editorMenu(c.oldSide ? pathA : pathB, commitHash)(c)}
          onNav={(t) => {
            const sideIsA = t.side === 'a' || t.side === 'del-file';
            const navPath = sideIsA ? pathA : pathB;
            const navRev = sideIsA ? opts?.revA : opts?.revB;
            void handleCodeNav(t, navPath, file?.path ?? pathB, 'diff', navRev, opts?.comparePath);
          }}
          onCursorPos={(line, col, side) => {
            curPosRef.current = { path: file?.path ?? pathB, line, col, side };
          }}
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
        .map((f) => ({ rel: f.rel, oldPath: f.oldPath, status: f.status, added: f.added, deleted: f.deleted })),
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
            <button className="icon-btn" title={tx('quickOpenP', { v: navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+' })} onClick={() => openQuick('')}>
              <Icon name="zoom" size={14} />
            </button>
            <button className="icon-btn" title={tx('new')} disabled={!root} onClick={newMenu}>
              <Icon name="plus" size={15} />
            </button>
          </>
        )}
        <button className="icon-btn" title={tx('refresh')} onClick={refreshTree}>
          {treeLoading || (view === 'changes' && gitLoading) ? <span className="spinner mini" /> : <Icon name="refresh" size={14} />}
        </button>
      </div>
      {git && (() => {
        const curWt = worktrees.find((w) => w.isCurrent || norm(w.path) === norm(cwd));
        const curBr = branchesInfo?.branches.find((b) => b.current || (!!git.branch && b.name === git.branch));
        const aheadCount = git.hasTrack ? (git.ahead ?? 0) : (curBr?.ahead ?? curWt?.ahead ?? 0);
        const behindCount = git.hasTrack ? (git.behind ?? 0) : (curBr?.behind ?? curWt?.behind ?? 0);
        const inLinkedWt = isOtherWorktree || (!!curWt && !curWt.isMain);
        const pullOnly = behindCount > 0 && aheadCount === 0;
        return (
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
              <Icon name={inLinkedWt ? 'git-worktree' : 'git-branch'} size={12} />
              <span className="truncate">{git.branch || wtLabel || 'HEAD'}</span>
              <Icon name="down" size={10} />
            </button>
            <div className="grow" />
            {onSwitchCwd && isOtherWorktree && baseCwd && (
              <button
                type="button"
                className="wp-wt-back"
                title={tx('backToSessionDir')}
                onClick={() => switchWorktree(baseCwd)}
              >
                <Icon name="back" size={11} />
                <span>{tx('mainDirShort')}</span>
              </button>
            )}
            <button
              type="button"
              className={`wp-ctx-sync ${fetching || gitBusy ? 'spinning' : ''}`}
              disabled={fetching || gitBusy}
              title={pullOnly ? tx('pullSyncAction') : tx('fetchRemotesAction')}
              aria-label={pullOnly ? tx('pullSyncAction') : tx('fetchRemotesAction')}
              onClick={() => void (pullOnly ? doPull() : doFetch())}
            >
              <Icon name="refresh" size={11} />
              {aheadCount > 0 && <span className="mono">↑{aheadCount}</span>}
              {behindCount > 0 && <span className="mono">↓{behindCount}</span>}
            </button>
          </div>
        );
      })()}
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
          visible={visible && view === 'search'}
          onOpen={(r, line, col, len) => openFile(relOpen(r), false, { line, col, len })}
          openFiles={tabs.filter((t) => !isWebTab(t) && !isReviewTab(t) && !isSideTab(t) && !parseCommitTab(t) && rel(t) !== t).map(rel)}
          dirty={file && dirty && !file.web && !file.commit && !file.review && !file.side && rel(file.path) !== file.path ? { rel: rel(file.path), draft: file.draft } : null}
          getLiveDirty={() => {
            const cur = fileRef.current;
            if (!cur || cur.web || cur.commit || cur.review || cur.side || cur.untitled) return null;
            const r = rel(cur.path);
            if (r === cur.path) return null;
            const liveDraft = getActiveEditorText(cur);
            return liveDraft !== cur.content ? { path: cur.path, rel: r, rev: cur.rev, draft: liveDraft } : null;
          }}
          includeRequest={grepInclude}
          queryRequest={grepQuery}
          onPreview={async (r, line, col, len, after) => {
            const abs = relOpen(r);
            await openFile(abs, false, { line, col, len });
            if (norm(fileRef.current?.path ?? '') === norm(abs)) {
              flushActiveEditor();
              setCompare({ path: abs, text: after, replace: true });
            }
          }}
          onDraft={(next, expect) => {
            const curNow = fileRef.current;
            if (!curNow || curNow.web || curNow.commit || curNow.review || curNow.side) return null;
            if (curNow.path !== expect.path || curNow.rev !== expect.rev) return null;
            const liveDraft = getActiveEditorText(curNow);
            if (liveDraft !== expect.draft) return null;
            if (hasUnsyncedEdits()) flushActiveEditor();
            const nextRev = nextFileRev();
            fileRef.current = { ...curNow, draft: next, rev: nextRev };
            setFile((cur) =>
              cur && cur.path === expect.path && cur.rev === expect.rev ? { ...cur, draft: next, rev: nextRev } : cur,
            );
            return nextRev;
          }}
          onReplaced={(paths) => {
            for (const p of paths) dropCachedFile(p);
            refreshTree();
            const f = fileRef.current;
            if (!f || f.draft !== f.content || hasUnsyncedEdits() || !paths.some((p) => norm(p) === norm(f.path))) return;
            readFile(f.path, true, true).then((n) => {
              if (!n) return;
              const live = fileRef.current;
              if (!live || live.path !== f.path || live.rev !== f.rev || live.draft !== live.content || hasUnsyncedEdits()) {
                return;
              }
              fileRef.current = n;
              setFile((cur) =>
                cur && cur.path === f.path && cur.rev === f.rev && cur.draft === cur.content && !hasUnsyncedEdits() ? n : cur,
              );
            });
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
                  onClick={() => {
                    flushActiveEditor();
                    setDiffScope('uncommitted');
                  }}
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
                    else {
                      flushActiveEditor();
                      setDiffScope('branch');
                    }
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
                      else {
                        flushActiveEditor();
                        setDiffScope('turn');
                      }
                    }}
                  >
                    <span>{tx('diffScopeTurnN', { n: activeTurnCkpt.turn })}</span>
                    {diffScope === 'turn' && turnCheckpoints.length > 1 && <Icon name="down" size={10} />}
                  </button>
                )}
              </div>
              <div className="wp-scope-right">
                {totals && changed.length > 0 && <LineStat a={totals.a} d={totals.d} />}
                {diffScope === 'uncommitted' && uncommittedKeys.length > 0 && (
                  <button
                    type="button"
                    className="icon-btn wp-scope-act"
                    title={tx('discardAllChanges')}
                    aria-label={tx('discardAllChanges')}
                    disabled={discarding}
                    onClick={discardAllUncommitted}
                  >
                    <Icon name="rotate-l" size={13} />
                  </button>
                )}
              </div>
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
              <div className="wp-tip wp-tip-loading">
                <span className="spinner mini" />
                <span>{tx('readingChanges')}</span>
              </div>
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
                    onDiscard={diffScope === 'uncommitted' && git ? discardRow : undefined}
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
                  loadingDirs={loadingDirs}
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
              <div className="wp-tip wp-tip-loading">
                <span className="spinner mini" />
                <span>{tx('loading2')}</span>
              </div>
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
          {view === 'changes' && git && (() => {
            const curWt = worktrees.find((w) => w.isCurrent || norm(w.path) === norm(cwd));
            const curBr = branchesInfo?.branches.find((b) => b.current || (!!git.branch && b.name === git.branch));
            const aheadCount = git.hasTrack ? (git.ahead ?? 0) : (curBr?.ahead ?? curWt?.ahead ?? 0);
            const behindCount = git.hasTrack ? (git.behind ?? 0) : (curBr?.behind ?? curWt?.behind ?? 0);
            const canMergeMain = !!effDefaultBranch && !!git.branch && git.branch !== effDefaultBranch && git.branch !== 'HEAD';
            const busyDock = pushing || fetching || gitBusy;

            let splitPrimary = false;
            let splitIcon = 'refresh';
            let splitLabel: string = tx('syncBtnShort');
            let splitTitle: string = tx('fetchRemotesAction');
            let splitRun: () => void = () => {
              void doFetch();
            };

            if (commitFiles.length > 0) {
              splitPrimary = true;
              splitIcon = 'git-commit';
              splitLabel = preferCommitPush
                ? tx('commitAndPushCount', { n: commitFiles.length })
                : tx('commitOnlyCount', { n: commitFiles.length });
              splitTitle = tx('commitChanges');
              splitRun = () => openCommit(preferCommitPush);
            } else if (behindCount > 0 && aheadCount === 0) {
              splitPrimary = true;
              splitIcon = 'download';
              splitLabel = tx('pullBehindBtn', { n: behindCount });
              splitTitle = tx('pullSyncAction');
              splitRun = () => void doPull();
            } else if (aheadCount > 0) {
              splitPrimary = true;
              splitIcon = 'upload';
              splitLabel = tx('pushAheadBtn', { n: aheadCount });
              splitTitle = tx('pushBranch');
              splitRun = () => void doPush();
            } else if (canMergeMain) {
              splitPrimary = false;
              splitIcon = 'git-merge';
              splitLabel = tx('mergeBackIntoShort', { target: effDefaultBranch });
              splitTitle = tx('mergeBackInto', { target: effDefaultBranch });
              splitRun = () => void doMergeToDefault(git.branch, false);
            }

            const openDockMenu = (e: React.MouseEvent<HTMLElement>) => {
              const r = e.currentTarget.closest('.wp-dock-split')?.getBoundingClientRect() ?? e.currentTarget.getBoundingClientRect();
              const items: (MenuItem | '-')[] = [
                {
                  label: tx('commitAndPushItem'),
                  icon: 'upload',
                  active: preferCommitPush,
                  disabled: commitFiles.length === 0 || busyDock,
                  run: () => {
                    setPreferCommitPush(true);
                    lsSet('wp-prefer-commit-push', true);
                    openCommit(true);
                  },
                },
                {
                  label: tx('commitOnlyItem', { n: commitFiles.length }),
                  icon: 'git-commit',
                  active: !preferCommitPush,
                  disabled: commitFiles.length === 0 || busyDock,
                  run: () => {
                    setPreferCommitPush(false);
                    lsSet('wp-prefer-commit-push', false);
                    openCommit(false);
                  },
                },
                '-',
                {
                  label: tx('pushBranch'),
                  icon: 'upload',
                  hint: aheadCount > 0 ? `↑${aheadCount}` : undefined,
                  disabled: busyDock,
                  run: () => void doPush(),
                },
                {
                  label: tx('pullSyncAction'),
                  icon: 'download',
                  hint: behindCount > 0 ? `↓${behindCount}` : undefined,
                  disabled: busyDock,
                  run: () => void doPull(),
                },
                ...(canMergeMain
                  ? [
                      '-' as const,
                      {
                        label: tx('mergeBackIntoItem', { target: effDefaultBranch }),
                        icon: 'git-merge',
                        disabled: busyDock,
                        run: () => void doMergeToDefault(git.branch, false),
                      },
                      {
                        label: tx('squashBackInto', { target: effDefaultBranch }),
                        icon: 'git-commit',
                        disabled: busyDock,
                        run: () => void doMergeToDefault(git.branch, true),
                      },
                    ]
                  : []),
                '-',
                {
                  label: tx('fetchRemotesAction'),
                  icon: 'refresh',
                  disabled: busyDock,
                  run: () => void doFetch(),
                },
              ];
              openMenu({ clientX: r.left, clientY: Math.max(8, r.top - 220) }, items);
            };

            return (
              <div className="wp-git-dock">
                <div className="wp-git-dock-cta">
                  <div className={`wp-dock-split ${splitPrimary ? 'primary' : ''}`}>
                    <button
                      type="button"
                      className="wp-dock-split-main"
                      disabled={busyDock}
                      title={splitTitle}
                      onClick={splitRun}
                    >
                      {busyDock ? <span className="spinner mini" /> : <Icon name={splitIcon} size={13} />}
                      <span className="truncate">{splitLabel}</span>
                    </button>
                    <button
                      type="button"
                      className="wp-dock-split-chev"
                      disabled={busyDock}
                      title={tx('gitActionsMenuTip')}
                      aria-label={tx('gitActionsMenuTip')}
                      onClick={openDockMenu}
                    >
                      <Icon name="down" size={11} />
                    </button>
                  </div>
                  {!standalone && (
                    <button type="button" className="wp-dock-btn ai" title={tx('crossToolReviewTitle')} onClick={() => setReviewPreset({})}>
                      <Icon name="sparkles" size={13} />
                      <span>{tx('reviewBtn')}</span>
                    </button>
                  )}
                </div>
              </div>
            );
          })()}
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

  const editor = file && (() => {
    const targetPath = file.path;
    const targetRev = file.rev;
    const isUnt = !!file.untitled;
    return (
      <CodeEditor
        ref={codeRef}
        docKey={`${file.path}#${file.rev}`}
        value={file.draft}
        filename={baseName(file.path)}
        readOnly={file.truncated || !!file.notUtf8}
        wrap={wrap || (md && mdSplit && wide())}
        goto={goto}
        onChange={(draft) => updateFileDraft(targetPath, targetRev, isUnt, draft)}
        onSave={() => save()}
        menu={editorMenu(file.path)}
        onToggleWrap={toggleWrap}
        onNav={file.untitled ? undefined : (t) => void handleCodeNav(t, file.path, file.path, 'main')}
        onCursorPos={(line, col) => {
          curPosRef.current = { path: file.path, line, col, side: undefined };
        }}
        onAddSelection={
          standalone
            ? undefined
            : (sel) => pushContextRef(file.path, { kind: 'code', fromLine: sel.fromLine, toLine: sel.toLine, content: sel.content })
        }
        scrollLine={md && mdSplit ? editorAt : null}
        onTopLine={md && mdSplit ? (line, frac) => syncFrom.current === 'editor' && setPreviewAt({ line, frac, n: Date.now() }) : undefined}
      />
    );
  })();

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
      mergeView(full, baseName(file.commit.rel), {
        refPath: file.commit.rel,
        pathA: relOpen(file.commit.rel),
        pathB: relOpen(file.commit.rel),
        revA: file.commit.baseHash ?? `${file.commit.hash}^`,
        revB: file.commit.hash,
        commitHash: file.commit.hash,
      })
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
    mergeView(full, baseName(file.path), {
      editable: false,
      refPath: file.path,
      pathA: file.path,
      pathB: compare!.path,
      revA: 'working',
      revB: compare!.replace ? 'replace-preview' : 'compare',
      comparePath: compare!.replace ? undefined : compare!.path,
    })
  ) : viewing === 'compare' ? (
    <div className="wp-diff">
      <PaneDiff diff={{ path: rel(file.path), oldText: file.draft, newText: compare!.text }} layout={layout} {...paneDiffActions(file.path)} />
    </div>
  ) : viewing === 'diff' ? (
    full ? (
      mergeView(full, baseName(file.path), {
        editable: !isTurnSnapshot && !file.deleted && !file.truncated && !file.notUtf8 && !file.binary,
        refPath: file.path,
        pathA: !isTurnSnapshot && diffBaseRev === 'HEAD' && curGit?.oldPath ? relOpen(curGit.oldPath) : file.path,
        pathB: file.path,
        revA: diffBaseRev,
        revB: isTurnSnapshot ? diffEndRev : 'working',
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
  ) : file.pdf || file.pdfSrc ? (
    <PdfView key={`${file.path}#${file.rev}`} src={file.pdfSrc} base64={file.pdf} onDownload={() => download(file.path)} />
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
        <button
          className="icon-btn"
          title={tx('compareWith', { v: baseName(file.path) })}
          onClick={() => {
            flushActiveEditor();
            setCompare({ path: second.path, text: second.content });
          }}
        >
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
          onNav={second.untitled ? undefined : (t) => void handleCodeNav(t, second.path, second.path, 'second')}
          onAddSelection={
            standalone
              ? undefined
              : (sel) => pushContextRef(second.path, { kind: 'code', fromLine: sel.fromLine, toLine: sel.toLine, content: sel.content })
          }
        />
      )}
    </div>
  );

  const navBtns = (
    <>
      <div className="wp-nav-btns">
        <button
          type="button"
          className="icon-btn"
          disabled={!navState.canBack}
          title={tx('navBack', { v: IS_MAC ? 'Ctrl+-' : 'Alt+←' })}
          aria-label={tx('navBack', { v: IS_MAC ? 'Ctrl+-' : 'Alt+←' })}
          onClick={() => void stepHistory(-1)}
        >
          <Icon name="back" size={13} />
        </button>
        <button
          type="button"
          className="icon-btn"
          disabled={!navState.canForward}
          title={tx('navForward', { v: IS_MAC ? 'Ctrl+Shift+-' : 'Alt+→' })}
          aria-label={tx('navForward', { v: IS_MAC ? 'Ctrl+Shift+-' : 'Alt+→' })}
          onClick={() => void stepHistory(1)}
        >
          <Icon name="forward" size={13} />
        </button>
      </div>
      <span className="wp-nav-sep" />
    </>
  );

  return (
    <aside
      className={`file-panel ${fullscreen ? 'full' : ''} ${file || emptyPane ? '' : 'browse'} ${standalone ? 'standalone' : ''} ${visible ? '' : 'wp-hidden'}`}
      style={{ '--tree-w': `${treeW}px` } as React.CSSProperties}
      onMouseDown={(e) => {
        if (e.button === 3 || e.button === 4) e.preventDefault();
      }}
      onMouseUp={(e) => {
        if (e.button === 3 || e.button === 4) {
          e.preventDefault();
          void stepHistory(e.button === 3 ? -1 : 1);
        }
      }}
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
          {navBtns}
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
          {navBtns}
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
              <Breadcrumbs hostId={hostId} root={root ?? cwd} path={file.path} hidden={hidden} git={git} onOpen={openFile} dirty={dirty} visible={visible} />
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
                <button className={!showDiff ? 'active' : ''} onClick={() => { flushActiveEditor(); setShowDiff(false); }}>
                  {tx('file')}
                </button>
                <button className={showDiff ? 'active' : ''} onClick={() => { flushActiveEditor(); setShowDiff(true); }} title={tx('changesSinceTheLastCommit')}>
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
                <button className={!preview ? 'active' : ''} onClick={() => { flushActiveEditor(); setPreview(false); }}>
                  {tx('source')}
                </button>
                <button
                  className={preview ? 'active' : ''}
                  onClick={() => { flushActiveEditor(); setPreview(true); }}
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
              <button className="icon-btn" title={tx('outlineGoToAFunction', { v: navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+' })} onClick={() => openQuick('@')}>
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
          <div className="wp-main" ref={mainRef}>
            {secondPane ? (
              <div className="wp-panes">
                <div className="wp-pane">{mainView}</div>
                {secondPane}
              </div>
            ) : (
              mainView
            )}
            <CodeNavNotice message={navNotice} />
            {openingPath && (
              <div className="wp-loading-overlay" role="status">
                <span className="spinner" />
                <span>{tx('openingFile', { name: baseName(openingPath) })}</span>
              </div>
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
            {openingPath && (
              <div className="wp-loading-overlay" role="status">
                <span className="spinner" />
                <span>{tx('openingFile', { name: baseName(openingPath) })}</span>
              </div>
            )}
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
            flushActiveEditor();
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
          defaultBranch={branchesInfo?.defaultBranch ?? wtDefaultBranch}
          defaultPush={commitDefaultPush}
          files={commitFiles}
          onCommitted={() => refreshGitAll(true)}
          onClose={() => setCommitOpen(false)}
        />
      )}
      {newWtOpen && git && (
        <NewWorktreeDialog
          hostId={hostId}
          cwd={cwd}
          defaultBase={newWtBase || git.branch || effDefaultBranch}
          branchesInfo={branchesInfo}
          worktrees={worktrees}
          canOpenSession={!standalone}
          onCreated={(wtPath, _branch, openSess) => {
            loadWorktrees(true);
            void loadBranches();
            if (openSess && !standalone) openNewSession({ hostId, cwd: wtPath });
            else if (onSwitchCwd) switchWorktree(wtPath);
          }}
          onClose={() => setNewWtOpen(false)}
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
          standalone={standalone}
          worktrees={worktrees}
          defaultBranch={wtDefaultBranch}
          branchesInfo={branchesInfo}
          loadingBranches={branchesLoading}
          fetching={fetching}
          hostSessions={hostSessions}
          currentSessionId={sessionId}
          onSwitchWorktree={onSwitchCwd ? switchWorktree : undefined}
          onJumpToSession={(sid) => navigate({ kind: 'session', hostId, sessionId: sid })}
          onNewSessionAt={standalone ? undefined : (p) => openNewSession({ hostId, cwd: p })}
          onCheckoutBranch={checkoutBranch}
          onCreateBranch={(base) => void createBranch(base)}
          onCreateWorktree={(base) => openNewWorktree(base)}
          onOpenBranchInWorktree={(br) => void doOpenBranchInWorktree(br)}
          onMergeWorktreeToDefault={(wt, squash) => {
            if (wt.branch) void doMergeToDefault(wt.branch, squash);
          }}
          onMergeBranchIntoCurrent={(br) => void doMergeIntoCurrent(br)}
          onRebaseOnto={(br) => void doRebaseOnto(br)}
          onCompareBranch={doCompareBranch}
          onRenameBranch={(br) => void doRenameBranch(br)}
          onDeleteBranch={(br) => void doDeleteBranch(br)}
          onDeleteWorktree={(wt) => void doDeleteWorktree(wt)}
          onCleanupMergedWorktrees={(candidates) => void doCleanupMergedWorktrees(candidates)}
          onFetch={() => void doFetch()}
          onCopyPath={(p) => void copy(p)}
          onClose={() => setBwOpen(false)}
        />
      )}
      {navPicker && (
        <DefinitionPicker
          symbol={navPicker.symbol}
          locations={navPicker.locations}
          coords={navPicker.coords}
          onPick={(loc) => {
            const orig = navPicker.origin;
            cancelPendingNav();
            jumpToNavTarget(orig, loc.absPath, loc.line, loc.col, loc.len);
          }}
          onClose={cancelPendingNav}
        />
      )}
      {navPeek && (
        <PeekReferences
          symbol={navPeek.symbol}
          loading={navPeek.loading}
          result={navPeek.result}
          coords={navPeek.coords}
          containerWidth={mainRef.current?.clientWidth ?? 640}
          onJump={(hit) => {
            const orig = navPeek.origin;
            cancelPendingNav();
            jumpToNavTarget(orig, hit.absPath, hit.line, hit.col, hit.len);
          }}
          onOpenInSearch={(sym) => {
            cancelPendingNav();
            setGrepQuery({ query: sym, caseSensitive: true, wholeWord: true, regex: false, n: Date.now() });
            pickView('search');
            setTree(true);
          }}
          onClose={cancelPendingNav}
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
  visible = true,
  onOpen,
}: {
  hostId: string;
  root: string;
  path: string;
  hidden: boolean;
  git: GitInfo | null;
  dirty: boolean;
  visible?: boolean;
  onOpen: (p: string) => void;
}) {
  const box = useRef<HTMLDivElement>(null);
  const [pop, setPop] = useState<{ dir: string; x: number; y: number } | null>(null);
  useBack(visible && !!pop, () => setPop(null));
  useEffect(() => {
    if (!visible) setPop(null);
  }, [visible]);
  const nr = norm(root);
  const np = norm(path);
  const inRoot = np === nr || np.startsWith(nr + '/');
  const rootClean = root.replace(/[\\/]+$/, '');
  const segs = (inRoot ? path.slice(rootClean.length) : path).split(/[\\/]/).filter(Boolean);
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
        <div className="wp-tip wp-tip-loading">
          <span className="spinner mini" />
          <span>{tx('loadingFiles')}</span>
        </div>
      ) : rows.length === 0 ? (
        <div className="wp-tip">{tx('thisFolderIsEmpty')}</div>
      ) : (
        <Rows
          rows={rows}
          scroller={box}
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
