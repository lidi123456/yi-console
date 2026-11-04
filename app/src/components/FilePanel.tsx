import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useBack } from '../back';
import { getT, trErr, useLang } from '../i18n';
import { errorToast, skey, toast, useSessionData, useStore } from '../store';
import { isTauri, rpc } from '../transport';
import type { DirEntry, FileDiff } from '../types';
import { fmtSize, lsGet, lsSet } from '../util';
import { writeClipboard } from '../clipboard';
import { Icon } from './Icon';
import { askConfirm, askText, openMenu, type MenuItem } from './Overlay';
import { CodeEditor } from './CodeEditor';
import { PaneDiff } from './DiffView';
import { MergeDiff, type MergeDiffHandle } from './MergeDiff';
import { CsvView, ImageView, MdPreview, PdfView } from './FilePreview';
import { GrepPanel } from './GrepPanel';
import { QuickOpen, type QuickMode } from './QuickOpen';
import { OPEN_CHANGES, takeOpenChanges } from './SessionActions';
import { BrowserPane, isWebTab, WEB_PREFIX, webTabFile, webTabTitle } from './Browser';
import { CommitDetails, GitHistory, isCommitTab, parseCommitTab } from './GitHistory';

const tx = getT('filePanel');

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
  /** A file in a commit (path is git:<hash>:<rel>): read-only diff against the commit's first parent */
  commit?: { hash: string; rel: string; subject: string; message: string; author: string; time: number; status: string; diff: string };
}

/** Dragging a file from the file panel to the input box: the value is "@relative-path " */
export const DRAG_REF = 'application/x-yi-console-ref';
/** Per-file size limit for uploads dropped onto the file tree (same as yi-agentd) */
const UPLOAD_MAX = 50 * 1024 * 1024;

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
  /** Total changed files when yi-agentd returned only the first 10000 */
  total?: number | null;
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
/** Title of a commit file tab: "name @abc1234"; null if t isn't one */
const commitTabLabel = (t: string) => {
  const c = parseCommitTab(t);
  return c ? `${baseName(c.rel)} @${c.hash.slice(0, 7)}` : null;
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
  const [g, color] = fileKind(name);
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
      <path d={GLYPH[g]} />
    </svg>
  );
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

function Rows({
  rows,
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
}: {
  rows: TreeRow[];
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
}) {
  return (
    <>
      {rows.map((r) => {
        const g = git && !r.isDir ? git.files.get(norm(r.path)) : undefined;
        const mark = g ? gitMark(g.status) : null;
        const dirChanged = !!git && r.isDir && !r.expanded && git.dirs.has(norm(r.path));
        return (
        <div
          key={r.key}
          role="treeitem"
          aria-expanded={r.isDir ? !!r.expanded : undefined}
          aria-selected={selected === r.path}
          data-path={r.path}
          data-key={r.key}
          data-dir={r.isDir ? '1' : undefined}
          className={`wp-row ${selected === r.path ? 'sel' : ''} ${focused === r.key ? 'kfocus' : ''} ${mark ? mark[1] : ''} ${dirChanged ? 'g-dir' : ''} ${
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
          title={mark ? `${r.path}\n${gitText(mark[0])}` : r.path}
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
          {stats && g && <LineStat a={g.added} d={g.deleted} />}
          {mark && <span className="wp-git">{mark[0]}</span>}
          {dirChanged && <span className="wp-git-dot" />}
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
        </div>
        );
      })}
    </>
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
const gitCache = new Map<string, { git: GitInfo | null; sig: string }>();
const gitKey = (hostId: string, cwd: string) => `${hostId}\n${cwd}`;
/** Root path and top-level listing per host + directory + hidden-files setting. The Changes list and the tree are
 *  both laid out against the root, so without it a remounted panel stays blank until fs.list returns. In memory only */
const rootCache = new Map<string, { path: string; entries: DirEntry[] }>();
const rootKey = (hostId: string, cwd: string, hidden: boolean) => `${hostId}\n${cwd}\n${hidden ? 1 : 0}`;

export function FilePanel({
  hostId,
  cwd,
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
  const [treeOpen, setTreeOpen] = useState(() => lsGet('wp-tree', true));
  const [treeW, setTreeW] = useState(() => lsGet('wp-tree-w', 300));
  const [filter, setFilter] = useState('');
  const [search, setSearch] = useState<{ q: string; paths: string[]; truncated: boolean; error?: boolean } | null>(null);
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  // Opened files (tabs); the current file is file
  const [tabs, setTabs] = useState<string[]>([]);
  const [git, setGit] = useState<GitInfo | null>(() => gitCache.get(gitKey(hostId, cwd))?.git ?? null);
  const [view, setView] = useState<'all' | 'changes' | 'search'>(() => lsGet('wp-view', 'all'));
  const [showDiff, setShowDiff] = useState(false);
  const [diffLayout, setDiffLayout] = useState<'split' | 'unified'>(() => lsGet('wp-diff-layout', 'split'));
  // Collapse unchanged regions in the full-file diff. VS Code's own default is off; ours is on, since the hunk-only diff we had before showed just the changes
  const [diffCollapse, setDiffCollapse] = useState(() => lsGet('wp-diff-collapse', true));
  const mergeRef = useRef<MergeDiffHandle>(null);
  // Make the editor jump to a line (search results, outline, : line number)
  const [goto, setGoto] = useState<{ line: number; col?: number; len?: number; n: number } | null>(null);
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
  // Summary of the last git status: periodic refreshes leave the UI alone if nothing changed, so an open diff doesn't flicker
  const gitSig = useRef(gitCache.get(gitKey(hostId, cwd))?.sig ?? '');
  /** quiet: background refresh (timer / window refocus); if the status didn't change, just quietly refetch the current file's diff */
  const loadGit = useCallback((quiet = false) => {
    // Without diffs: in a big repo with many changed files, running git diff for each takes ages, so fetch only the file that gets opened
    rpc<{ repo: string | null; branch?: string; head?: string | null; total?: number | null; files: { path: string; status: string; diff: string; added?: number | null; deleted?: number | null }[] }>(hostId, 'git.changes', { cwd, diff: false })
      .then((r) => {
        setGitReady(true);
        const sig = `${r.repo}|${r.branch}|${r.head}|${r.files.map((f) => `${f.status} ${f.path} ${f.added ?? ''}/${f.deleted ?? ''}`).join('|')}`;
        if (quiet && sig === gitSig.current) return refreshCurDiff.current();
        gitSig.current = sig;
        setFileDiffs({});
        setBlobs({});
        if (!r.repo) {
          gitCache.set(gitKey(hostId, cwd), { git: null, sig });
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
        const info = { repo: r.repo, branch: r.branch ?? '', head: r.head, total: r.total, files, dirs };
        gitCache.set(gitKey(hostId, cwd), { git: info, sig });
        setGit(info);
      })
      .catch(() => (setGitReady(true), quiet || setGit(null)));
  }, [hostId, cwd]);
  const refreshCurDiff = useRef(() => {});
  // With a cached status this is a background refresh: nothing changes on screen unless the status did
  useEffect(() => loadGit(gitCache.has(gitKey(hostId, cwd))), [loadGit]);
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
    if (isWebTab(path)) return { path, content: '', draft: '', binary: true, truncated: false, size: 0, modified: 0, rev: Date.now(), web: true };
    const ct = parseCommitTab(path);
    if (ct) {
      try {
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
      title: tx('discardChangesTo', { v: baseName(file!.path) }),
      message: tx('unsavedEditsWillBeDiscarded'),
      okText: tx('discard'),
      danger: true,
    }));

  const gitOf = (path: string) => git?.files.get(norm(path));
  /** Changes to this file: in a git repo, the working tree vs the last commit; otherwise the edits recorded by tools in the session */
  const diffOf = (path: string): FileDiff | null => {
    const g = gitOf(path);
    const d = g?.diff || fileDiffs[norm(path)];
    if (g && d) return { path: g.rel, unified: d.slice(Math.max(0, d.indexOf('@@'))) };
    return null;
  };
  /** Outside a git repo, each edit tools made to this file in the session (each is a snippet, shown separately) */
  const sessionDiffsOf = (path: string): FileDiff[] => (gitReady && !git ? touched.get(norm(path)) ?? [] : []);
  /** Whether this file has changes to view (the diff may not have arrived yet) */
  const hasChange = (path: string) => !!gitOf(path) || sessionDiffsOf(path).length > 0;

  // Fetch the diff when a changed file is opened
  const curKey = file && !file.web && !file.commit ? norm(file.path) : '';
  const curGit = file ? gitOf(file.path) : undefined;
  useEffect(() => {
    if (!curKey || !curGit || curGit.diff || curKey in fileDiffs) return;
    const k = curKey;
    rpc<{ files: { path: string; diff: string }[] }>(hostId, 'git.changes', { cwd, paths: [curGit.rel] })
      .then((r) => setFileDiffs((m) => ({ ...m, [k]: r.files.find((f) => f.path === curGit.rel)?.diff ?? '' })))
      .catch(() => setFileDiffs((m) => ({ ...m, [k]: '' })));
  }, [curKey, curGit, fileDiffs]); // eslint-disable-line react-hooks/exhaustive-deps
  // Status unchanged but the content may have changed again: refetch the current file's diff in the background and swap it in when it arrives, without showing "loading"
  refreshCurDiff.current = () => {
    if (!curKey || !curGit || !(curKey in fileDiffs)) return;
    const k = curKey;
    rpc<{ files: { path: string; diff: string }[] }>(hostId, 'git.changes', { cwd, paths: [curGit.rel] })
      .then((r) => {
        const d = r.files.find((f) => f.path === curGit.rel)?.diff ?? '';
        setFileDiffs((m) => (m[k] === d ? m : { ...m, [k]: d }));
      })
      .catch(() => {});
  };
  // The file no longer has changes (e.g. just committed): if we're still on "Changes", switch back to the file
  useEffect(() => {
    if (showDiff && file && !file.deleted && gitReady && !hasChange(file.path)) setShowDiff(false);
  }, [git, showDiff, file?.path]); // eslint-disable-line react-hooks/exhaustive-deps

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
  const openFile = async (path0: string, diff = false, at?: { line: number; col?: number; len?: number }) => {
    const path = unverbatim(path0);
    const jump = () => at && setGoto({ ...at, n: Date.now() });
    if (file?.path === path) {
      if (diff && hasChange(path)) setShowDiff(true);
      if (at) {
        setShowDiff(false);
        if (preview) previewFor(path, at);
        setCompare(null);
        jump();
      }
      if (window.matchMedia('(max-width: 760px)').matches) setTreeOpen(false);
      return;
    }
    if (!(await confirmDiscard())) return;
    const g = gitOf(path);
    const f: OpenFile | null =
      g && gitMark(g.status)[0] === 'D'
        ? { path, content: '', draft: '', binary: true, truncated: false, size: 0, modified: 0, rev: Date.now(), deleted: true }
        : await readFile(path);
    if (!f) return;
    setFile(f);
    setTabs((t) => (t.includes(path) ? t : [...t, path].slice(-30)));
    setSaveState(null);
    previewFor(path, at);
    setCompare(null);
    setGoto(at ? { ...at, n: Date.now() } : null);
    setShowDiff(!!f.deleted || (diff && hasChange(path)));
    if (window.matchMedia('(max-width: 760px)').matches) setTreeOpen(false);
  };

  // ---------- When the tab bar overflows: scroll the current tab into view and add a dropdown on the right listing all tabs (like VS Code) ----------
  const tabsRef = useRef<HTMLDivElement>(null);
  const [tabsOverflow, setTabsOverflow] = useState(false);
  const measureTabs = () => {
    const el = tabsRef.current;
    setTabsOverflow(!!el && el.scrollWidth > el.clientWidth + 1);
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
    if (left < bar.scrollLeft) bar.scrollLeft = left;
    else if (right > bar.scrollLeft + bar.clientWidth) bar.scrollLeft = right - bar.clientWidth;
  };
  useEffect(() => {
    const el = tabsRef.current;
    if (!el) return;
    // When the panel narrows / is resized, keep the current file's tab visible
    const ro = new ResizeObserver(() => {
      measureTabs();
      revealActiveTab();
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [!!file]); // eslint-disable-line react-hooks/exhaustive-deps
  useLayoutEffect(() => {
    measureTabs();
    revealActiveTab();
  }, [file?.path, tabs.length]);
  const tabsMenu = (e: React.MouseEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    openMenu(
      { clientX: r.right - 260, clientY: r.bottom + 4 },
      [...tabs].reverse().map((t) => ({
        label: isWebTab(t) ? webTabTitle(t) : commitTabLabel(t) ?? baseName(t),
        icon: isWebTab(t) ? 'globe' : isCommitTab(t) ? 'diff' : 'file',
        hint: isWebTab(t) || isCommitTab(t) || rel(parentOf(t)) === '.' ? undefined : rel(parentOf(t)),
        active: t === file?.path,
        run: () => openFile(t),
      })),
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

  /** Close a tab: if it's the current file, switch to its neighbor; once all are closed, go back to tree only */
  const closeTab = async (path: string) => {
    const i = tabs.indexOf(path);
    const rest = tabs.filter((t) => t !== path);
    if (file?.path !== path) return setTabs(rest);
    if (!(await confirmDiscard())) return;
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
    if (!file || keep.includes(file.path)) return setTabs(keep);
    if (!(await confirmDiscard())) return;
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
    const keep = [...new Set(saved.tabs.map((t) => (isWebTab(t) ? t : unverbatim(t))))].filter((t) => (isWebTab(t) ? !webTabFile(t) || inRoot(webTabFile(t)!) : inRoot(t)));
    const cur = saved.current && keep.includes(saved.current) ? saved.current : keep[keep.length - 1];
    if (!cur) return;
    restoring.current = true;
    readFile(cur, true).then((f) => {
      restoring.current = false;
      // If the user already opened a file while restoring, don't overwrite it
      if (fileRef.current) return;
      if (!f) return setTabs(keep.filter((t) => t !== cur));
      setTabs(keep);
      setFile(f);
      previewFor(cur);
      // On phones a restored file normally takes the screen; not when the panel was opened for "Changes"
      if (!forChanges.current && window.matchMedia('(max-width: 760px)').matches) setTreeOpen(false);
    });
  }, [root]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    if (!restored.current || restoring.current || !root) return;
    lsSet(tabsKey, { tabs, current: file?.path ?? null });
  }, [tabs.join('|'), file?.path, root]); // eslint-disable-line react-hooks/exhaustive-deps

  const reload = async () => {
    if (!file) return;
    const f = await readFile(file.path);
    if (f) {
      setFile(f);
      setSaveState(null);
      toast(tx('reloadedFromDisk', { v: baseName(f.path) }));
    }
  };

  const save = async (overwrite = false) => {
    const f = fileRef.current;
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
    if (f && f.draft === f.content && !f.image && !f.web && !f.commit) {
      rpc<Omit<OpenFile, 'draft' | 'rev'>>(hostId, 'fs.read', { path: f.path })
        .then((r) => {
          if (r.modified === f.modified) return;
          setFile((cur) => (cur && cur.path === f.path && cur.draft === cur.content ? { ...r, draft: r.content, rev: Date.now() } : cur));
        })
        .catch(() => {});
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status]);

  const closePanel = async () => {
    if (await confirmDiscard()) onClose();
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
    window.dispatchEvent(new CustomEvent(INSERT_COMPOSER, { detail: { sessionId, text: `@${rel(path)}${isDir ? '/' : ''} ` } }));
    if (window.matchMedia('(max-width: 760px)').matches) {
      onClose();
      toast(tx('addedToInput'));
    }
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
    const canSide = !r.isDir && !!file && !file.web && !file.commit && file.path !== r.path && wide();
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
      '-',
      { label: tx('rename'), icon: 'edit', run: () => rename(r.path) },
      { label: tx('delete2'), icon: 'trash', danger: true, run: () => remove(r.path, r.isDir) },
    ]);
  };

  const fileMenu = (e: React.MouseEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    const items: (MenuItem | '-')[] = file
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
          { label: wrap ? tx('disableWordWrap') : tx('enableWordWrap'), icon: 'wrap', run: () => (lsSet('wp-wrap', !wrap), setWrap(!wrap)) },
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

  // ---------- Shortcuts: Ctrl+P quick open, Ctrl+Shift+O symbols, Ctrl+G go to line ----------
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey) || e.altKey) return;
      const k = e.key.toLowerCase();
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
  const dragTree = (e: React.PointerEvent) => {
    e.preventDefault();
    const x0 = e.clientX;
    const w0 = treeW;
    let w = w0;
    const move = (ev: PointerEvent) => {
      w = Math.max(240, Math.min(420, w0 - (ev.clientX - x0)));
      setTreeW(w);
    };
    const up = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
      document.body.classList.remove('resizing');
      lsSet('wp-tree-w', w);
    };
    document.body.classList.add('resizing');
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  };

  const rows = useMemo(() => (root ? flatten(root, 0, kids, expanded) : []), [root, kids, expanded, lang]);
  const sRows = useMemo(() => (root && search ? searchRows(root, search.paths, collapsed) : []), [root, search, collapsed]);
  // "Changes": files changed under this working directory, arranged as a tree by directory
  // Outside a git repo, the files tools changed in this session stand in for git status
  const allChanged = useMemo(() => {
    if (!root) return [];
    const keys = git ? [...git.files.keys()] : gitReady ? [...touched.keys()] : [];
    const r = norm(root);
    return keys.filter((k) => k.startsWith(r + '/'));
  }, [git, gitReady, touched, root]);
  const changed = useMemo(() => {
    if (!root) return [];
    const r = norm(root);
    // Keys are normalized (lowercased on Windows drives): take the original spelling from git status when there is one
    return allChanged.map((k) => (git?.files.get(k)?.path.replace(/\\/g, '/') ?? k).slice(r.length + 1));
  }, [allChanged, root, git]);
  // Totals at the top of the Changes view: lines added/removed across the listed files (files that can't be computed are skipped)
  const totals = useMemo(() => {
    let a = 0, d = 0, any = false;
    for (const k of allChanged) {
      const f = git?.files.get(k);
      if (f?.added != null || f?.deleted != null) any = true;
      a += f?.added ?? 0;
      d += f?.deleted ?? 0;
    }
    return any ? { a, d } : null;
  }, [allChanged, git]);
  const cRows = useMemo(() => (root ? searchRows(root, changed, collapsed) : []), [root, changed, collapsed]);
  const toggleCollapsed = (r: TreeRow) =>
    setCollapsed((s) => {
      const n = new Set(s);
      const k = r.key.slice(2);
      if (n.has(k)) n.delete(k);
      else n.add(k);
      return n;
    });
  const pickView = (v: 'all' | 'changes' | 'search') => {
    lsSet('wp-view', v);
    setView(v);
    setCollapsed(new Set());
  };
  // "View changes": switch the side column to "Changes" and make sure it's visible; the editor keeps whatever it shows
  // (like VS Code's Source Control view). Taken on mount too, since a closed panel mounts after the event has fired
  useEffect(() => {
    const take = () => {
      if (!takeOpenChanges(hostId, sessionId)) return;
      forChanges.current = true;
      pickView('changes');
      setTreeOpen(true);
      loadGit();
    };
    take();
    const on = (e: Event) => {
      const d = (e as CustomEvent<{ hostId: string; sessionId: string }>).detail;
      if (d.hostId === hostId && d.sessionId === sessionId) take();
    };
    window.addEventListener(OPEN_CHANGES, on);
    return () => window.removeEventListener(OPEN_CHANGES, on);
  }, [hostId, sessionId, loadGit]); // eslint-disable-line react-hooks/exhaustive-deps
  const showTree = treeOpen || !file;
  // On mobile, after opening a file from the tree, back returns to the tree first; pressing again closes the whole file panel
  useBack(!!file && !treeOpen && window.matchMedia('(max-width: 760px)').matches, () => setTreeOpen(true));
  useBack(quick !== null, () => setQuick(null));
  const fileDiff = file ? diffOf(file.path) : null;
  const fileSessionDiffs = file ? sessionDiffsOf(file.path) : [];
  const diffPending = !!curGit && !curGit.diff && !(curKey in fileDiffs);
  const state = saveState ?? (dirty ? 'dirty' : null);
  const md = !!file && isMarkdown(file.path) && !file.binary;
  const csv = !!file && isCsv(file.path) && !file.binary;
  const html = !!file && isHtml(file.path) && !file.binary;
  const relOpen = (r: string) => (root ? join(root, r.split('/').join(sepOf(root))) : r);
  const layout: 'split' | 'unified' = diffLayout === 'split' && wide() ? 'split' : 'unified';
  const flipLayout = () => {
    const v = diffLayout === 'split' ? 'unified' : 'split';
    lsSet('wp-diff-layout', v);
    setDiffLayout(v);
  };
  const viewing = compare ? 'compare' : showDiff ? 'diff' : 'file';

  // ---------- Full-file diff (VS Code style): both whole files, falling back to the hunk-only diff ----------
  /** Revisions to fetch for the current view: the working tree is compared with HEAD, a commit with its first parent */
  const blobSpecs: [string, string][] = !file
    ? []
    : file.commit
      ? [[`${file.commit.hash}^`, file.commit.rel], [file.commit.hash, file.commit.rel]]
      : viewing === 'diff' && curGit && !file.truncated && (!file.binary || file.deleted)
        ? [['HEAD', curGit.rel]]
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
    const texts = blobSpecs.map(([r, p]) => blobText(blobs[blobKey(r, p)]));
    if (texts.some((x) => x === null)) return null;
    if (texts.some((x) => x === undefined)) return undefined;
    // Working tree: the editor's current text, so unsaved edits show too (as in VS Code); a deleted file is empty
    return file.commit ? { a: texts[0]!, b: texts[1]! } : { a: texts[0]!, b: file.deleted ? '' : file.draft };
  })();
  const flipCollapse = () => {
    lsSet('wp-diff-collapse', !diffCollapse);
    setDiffCollapse(!diffCollapse);
  };
  const mergeView = (f: { a: string; b: string }, name: string) => (
    <div className="wp-diff wp-diff-full">
      <MergeDiff ref={mergeRef} a={f.a} b={f.b} filename={name} layout={layout} collapse={diffCollapse} wrap={wrap} />
    </div>
  );
  /** Toolbar buttons for the full-file diff: previous / next change and collapsing unchanged regions */
  const mergeBtns = full ? (
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

  const toggleHidden = () => (lsSet('wp-hidden', !hidden), setHidden(!hidden));
  const newMenu = (e: React.MouseEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    openMenu({ clientX: r.right - 160, clientY: r.bottom + 4 }, [
      { label: tx('newFile'), icon: 'file', run: () => root && create(root, 'file') },
      { label: tx('newFolder'), icon: 'folder', run: () => root && create(root, 'dir') },
      '-',
      { label: tx('openWebPage'), icon: 'globe', run: () => openWeb() },
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

  // Changes view with nothing uncommitted: the commit history box below takes the remaining height
  const histGrow = view === 'changes' && !!git && gitReady && cRows.length === 0;
  const tree = (
    <div className="wp-side" style={file || emptyPane ? { width: treeW } : undefined}>
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
        <button className="icon-btn" title={tx('quickOpenP', { v: navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+' })} onClick={() => setQuick('')}>
          <Icon name="zoom" size={14} />
        </button>
        <button className="icon-btn" title={tx('new')} disabled={!root} onClick={newMenu}>
          <Icon name="plus" size={15} />
        </button>
        <button className="icon-btn" title={tx('refresh')} onClick={refreshTree}>
          <Icon name="refresh" size={14} />
        </button>
      </div>
      {view === 'search' && root ? (
        <GrepPanel
          hostId={hostId}
          root={root}
          onOpen={(r, line, col, len) => openFile(relOpen(r), false, { line, col, len })}
          openFiles={tabs.filter((t) => !isWebTab(t) && !parseCommitTab(t) && rel(t) !== t).map(rel)}
          dirty={file && dirty && !file.web && !file.commit && rel(file.path) !== file.path ? { rel: rel(file.path), draft: file.draft } : null}
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
            ) : view === 'changes' && !gitReady ? (
              <div className="wp-tip">{tx('readingChanges')}</div>
            ) : view === 'changes' && (git || touched.size > 0) ? (
              <>
                {!git && <div className="wp-limit">{tx('notAGitRepoFiles')}</div>}
                {git?.total && (
                  <div className="wp-limit">
                    {tx('tooManyChangesShowingThe', { total: git.total, size: git.files.size })}
                  </div>
                )}
                {totals && changed.length > 0 && (
                  <div className="wp-totals">
                    {tx('files2', { length: changed.length })}
                    <LineStat a={totals.a} d={totals.d} />
                  </div>
                )}
                {cRows.length ? (
                  <Rows
                    rows={cRows}
                    stats
                    git={git}
                    selected={file?.path}
                    focused={focusKey}
                    onToggle={toggleCollapsed}
                    onOpen={(r) => openFile(r.path, true)}
                    onMenu={rowMenu}
                    onDragRow={dragRow}
                  />
                ) : (
                  <div className="wp-tip">
                    <b>{tx('noChanges')}</b>
                    {tx('workingTreeMatchesTheLast')}
                  </div>
                )}
              </>
            ) : root ? (
              rows.length ? (
                <Rows
                  rows={rows}
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
            <GitHistory hostId={hostId} cwd={cwd} head={git?.head} selected={file?.path} grow={histGrow} onOpen={(t) => openFile(t)} />
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
      onChange={(draft) => setFile((cur) => cur && { ...cur, draft })}
      onSave={() => save()}
      scrollLine={md && mdSplit ? editorAt : null}
      onTopLine={md && mdSplit ? (line, frac) => syncFrom.current === 'editor' && setPreviewAt({ line, frac, n: Date.now() }) : undefined}
    />
  );

  // Content of the editor column: file / changes / compare
  const mainView = !file ? null : file.commit ? (
    full ? (
      mergeView(full, baseName(file.commit.rel))
    ) : full === undefined ? (
      loadingChanges
    ) : file.commit.diff ? (
      <div className="wp-diff">
        <PaneDiff diff={{ path: file.commit.rel, unified: file.commit.diff.slice(Math.max(0, file.commit.diff.indexOf('@@'))) }} layout={layout} />
      </div>
    ) : (
      <div className="wp-empty">
        <div className="wp-empty-title">{tx('noChangesToShow')}</div>
        <div className="muted small">{tx('fileIsTooLargeBinary')}</div>
      </div>
    )
  ) : file.web ? (
    <BrowserPane hostId={hostId} root={root ?? cwd} tab={file.path} onNavigate={(t) => navigateWeb(file.path, t)} />
  ) : html && preview && viewing === 'file' ? (
    <BrowserPane hostId={hostId} root={root ?? cwd} tab={`${WEB_PREFIX}file:${file.path}`} reload={file.modified} onNavigate={(t) => openFile(t)} />
  ) : viewing === 'compare' && full ? (
    mergeView(full, baseName(file.path))
  ) : viewing === 'compare' ? (
    <div className="wp-diff">
      <PaneDiff diff={{ path: rel(file.path), oldText: file.draft, newText: compare!.text }} layout={layout} />
    </div>
  ) : viewing === 'diff' ? (
    full ? (
      mergeView(full, baseName(file.path))
    ) : full === undefined ? (
      loadingChanges
    ) : fileDiff ? (
      <div className="wp-diff">
        <PaneDiff diff={{ ...fileDiff, path: rel(file.path) }} layout={layout} />
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
          <PaneDiff key={i} diff={{ ...d, path: rel(file.path) }} layout={layout} />
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
          {git?.branch && (
            <span className="wp-branch truncate" title={tx('gitBranch', { branch: git.branch })}>
              {git.branch}
            </span>
          )}
          <div className="grow" />
          <button className="icon-btn" title={tx('openWebPageNewBrowser')} onClick={() => openWeb()}>
            <Icon name="globe" size={14} />
          </button>
          {panelBtns}
        </div>
      ) : (
        <div className="wp-top wp-tabbar">
          <div
            className="wp-filetabs"
            role="tablist"
            aria-label={tx('openFiles')}
            ref={tabsRef}
            onScroll={measureTabs}
            // Like VS Code: vertical wheel scrolling scrolls the tabs horizontally
            onWheel={(e) => {
              if (Math.abs(e.deltaY) > Math.abs(e.deltaX)) e.currentTarget.scrollLeft += e.deltaY;
            }}
          >
            {tabs.map((t) => {
              const cur = t === file.path;
              const ct = parseCommitTab(t);
              if (ct)
                return (
                  <div
                    key={t}
                    role="tab"
                    aria-selected={cur}
                    className={`wp-ftab ${cur ? 'active' : ''}`}
                    title={`${ct.rel}\n${tx('commit')} ${ct.hash.slice(0, 10)}${cur && file.commit ? ` · ${file.commit.subject}` : ''}`}
                    onClick={() => openFile(t)}
                    onAuxClick={(e) => e.button === 1 && closeTab(t)}
                    onContextMenu={(e) =>
                      openMenu(e, [
                        { label: tx('close'), icon: 'x', run: () => closeTab(t) },
                        ...closeItems(t),
                        '-',
                        { label: tx('copyCommitHash'), icon: 'copy', run: () => copy(ct.hash, tx('commitHashCopied')) },
                      ])
                    }
                  >
                    <Icon name="diff" size={13} />
                    <span className="truncate">{baseName(ct.rel)}</span>
                    <span className="wp-ftab-hash">{ct.hash.slice(0, 7)}</span>
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
              if (isWebTab(t))
                return (
                  <div
                    key={t}
                    role="tab"
                    aria-selected={cur}
                    className={`wp-ftab ${cur ? 'active' : ''}`}
                    title={t.slice(WEB_PREFIX.length) || tx('newTab')}
                    onClick={() => openFile(t)}
                    onAuxClick={(e) => e.button === 1 && closeTab(t)}
                    onContextMenu={(e) =>
                      openMenu(e, [
                        { label: tx('close'), icon: 'x', run: () => closeTab(t) },
                        ...closeItems(t),
                      ])
                    }
                  >
                    <Icon name="globe" size={13} />
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
              return (
                <div
                  key={t}
                  role="tab"
                  aria-selected={cur}
                  className={`wp-ftab ${cur ? 'active' : ''} ${mark ? mark[1] : ''}`}
                  title={t}
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
          {tabsOverflow && (
            <button className="icon-btn wp-tabs-more" title={tx('allOpenFiles', { length: tabs.length })} onClick={tabsMenu}>
              <Icon name="down" size={14} />
            </button>
          )}
          <button className="icon-btn" title={tx('openWebPageNewBrowser')} onClick={() => openWeb()}>
            <Icon name="globe" size={14} />
          </button>
          <button
            className={`icon-btn ${treeOpen ? 'active' : ''}`}
            title={treeOpen ? tx('hideFileTree') : tx('showFileTree')}
            onClick={() => (lsSet('wp-tree', !treeOpen), setTreeOpen(!treeOpen))}
          >
            <Icon name="fileTree" size={14} />
          </button>
          {panelBtns}
        </div>
      )}

      {file?.commit && (
        <div className="wp-head">
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
          <div className="wp-actions">
            <span className="hide-mobile">{tx('readOnly')}</span>
            {mergeBtns}
            {wide() && (
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
      {file?.commit && commitInfo && (
        <div className="wp-commit-detail">
          <CommitDetails hash={file.commit.hash} author={file.commit.author} time={file.commit.time} message={file.commit.message} />
        </div>
      )}
      {file && !file.web && !file.commit && (
        <div className="wp-head">
          {compare ? (
            <div className="wp-compare-title truncate" title={`${file.path}\n${compare.path}`}>
              <span className="pd-tag del">{baseName(file.path)}</span>
              <Icon name="forward" size={12} />
              <span className="pd-tag add">{compare.replace ? tx('afterReplace') : rel(compare.path)}</span>
            </div>
          ) : (
            <Breadcrumbs hostId={hostId} root={root ?? cwd} path={file.path} hidden={hidden} onOpen={openFile} dirty={dirty} />
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
            {viewing !== 'file' && wide() && (
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

      <div className="wp-body">
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
          <div className="wp-main wp-empty">
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
            {(file || emptyPane) && <div className="wp-split hide-mobile" onPointerDown={dragTree} />}
            {tree}
          </>
        )}
      </div>
      {quick !== null && root && (
        <QuickOpen
          hostId={hostId}
          root={root}
          initial={quick}
          recent={[...tabs].reverse().filter((t) => !isWebTab(t) && !isCommitTab(t)).map(rel)}
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
    </aside>
  );
}

// ---------- Path above the editor: clicking a directory pops up the tree at that level ----------

function Breadcrumbs({
  hostId,
  root,
  path,
  hidden,
  dirty,
  onOpen,
}: {
  hostId: string;
  root: string;
  path: string;
  hidden: boolean;
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
  selected,
  x,
  y,
  onClose,
  onOpen,
}: {
  hostId: string;
  dir: string;
  hidden: boolean;
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
