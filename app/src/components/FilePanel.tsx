import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useBack } from '../back';
import { tr, trErr, useLang } from '../i18n';
import { errorToast, skey, toast, useSessionData, useStore } from '../store';
import { isTauri, rpc } from '../transport';
import type { DirEntry, FileDiff } from '../types';
import { fmtSize, lsGet, lsSet } from '../util';
import { writeClipboard } from '../clipboard';
import { Icon } from './Icon';
import { askConfirm, askText, openMenu, type MenuItem } from './Overlay';
import { CodeEditor } from './CodeEditor';
import { PaneDiff } from './DiffView';
import { CsvView, ImageView, MdPreview, PdfView } from './FilePreview';
import { GrepPanel } from './GrepPanel';
import { QuickOpen, type QuickMode } from './QuickOpen';
import { OPEN_CHANGES } from './SessionActions';
import { BrowserPane, isWebTab, WEB_PREFIX, webTabFile, webTabTitle } from './Browser';
import { GitHistory, isCommitTab, parseCommitTab } from './GitHistory';

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
  commit?: { hash: string; rel: string; subject: string; author: string; time: number; status: string; diff: string };
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
  if (code === 'U') return tr('新文件（未跟踪）', 'Untracked');
  if (code === 'A') return tr('新增', 'Added');
  if (code === 'D') return tr('已删除', 'Deleted');
  if (code === 'R') return tr('改名', 'Renamed');
  if (code === 'M') return tr('已修改', 'Modified');
  if (code === '!') return tr('有冲突', 'Conflict');
  return code;
}

type SaveState = 'saving' | 'conflict' | 'error' | null;

function saveStateText(state: string): string {
  if (state === 'dirty') return tr('已修改', 'Modified');
  if (state === 'saving') return tr('正在保存', 'Saving');
  if (state === 'conflict') return tr('磁盘内容已变化', 'Changed on disk');
  if (state === 'error') return tr('保存失败', 'Save failed');
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
          data-dir={r.isDir ? '1' : undefined}
          className={`wp-row ${selected === r.path ? 'sel' : ''} ${mark ? mark[1] : ''} ${dirChanged ? 'g-dir' : ''} ${
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
              title={tr('更多操作', 'More actions')}
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
      else out.push({ key: e.path + '/…', path: '', name: tr('加载中…', 'Loading…'), depth: depth + 1, isDir: false, loading: true });
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
  const [root, setRoot] = useState<string | null>(null);
  const [kids, setKids] = useState<Record<string, DirEntry[]>>({});
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [hidden, setHidden] = useState(() => lsGet('wp-hidden', true));
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
  // Make the editor jump to a line (search results, outline, : line number)
  const [goto, setGoto] = useState<{ line: number; col?: number; len?: number; n: number } | null>(null);
  const [quick, setQuick] = useState<QuickMode | null>(null);
  // Another file opened side by side on the right (read-only); compare diffs the current file against another
  const [second, setSecond] = useState<OpenFile | null>(null);
  const [compare, setCompare] = useState<{ path: string; text: string } | null>(null);
  const [dropDir, setDropDir] = useState<string | null>(null);
  const [uploading, setUploading] = useState(0);
  const isLocal = useStore((st) => st.hosts.find((h) => h.config.id === hostId)?.config.kind === 'local');
  // "Changes": only what this session changed
  const [onlyMine, setOnlyMine] = useState(true);
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

  const load = useCallback(
    async (dir: string) => {
      try {
        const r = await rpc<{ path: string; entries: DirEntry[] }>(hostId, 'fs.list', { path: dir, showHidden: hidden });
        setKids((k) => ({ ...k, [dir]: r.entries }));
        return r.path;
      } catch (e) {
        errorToast(e);
        return null;
      }
    },
    [hostId, hidden],
  );

  // Root directory: fs.list normalizes ~ and relative forms into absolute paths
  useEffect(() => {
    setKids({});
    setExpanded(new Set());
    rpc<{ path: string; entries: DirEntry[] }>(hostId, 'fs.list', { path: cwd, showHidden: hidden })
      .then((r) => {
        setRoot(r.path);
        setKids({ [r.path]: r.entries });
      })
      .catch(errorToast);
  }, [hostId, cwd, hidden]);

  // Until git status comes back we don't know whether this is a repo, so the snippets recorded in the session can't be shown as a diff yet
  const [gitReady, setGitReady] = useState(() => gitCache.has(gitKey(hostId, cwd)));
  // Diff of a single file (git.changes only lists statuses; the diff is fetched when the file is opened), keyed by norm(absolute path)
  const [fileDiffs, setFileDiffs] = useState<Record<string, string | null>>({});
  // Summary of the last git status: periodic refreshes leave the UI alone if nothing changed, so an open diff doesn't flicker
  const gitSig = useRef(gitCache.get(gitKey(hostId, cwd))?.sig ?? '');
  /** quiet: background refresh (timer / window refocus); if the status didn't change, just quietly refetch the current file's diff */
  const loadGit = useCallback((quiet = false) => {
    // Without diffs: in a big repo with many changed files, running git diff for each takes ages, so fetch only the file that gets opened
    rpc<{ repo: string | null; branch?: string; head?: string | null; files: { path: string; status: string; diff: string; added?: number | null; deleted?: number | null }[] }>(hostId, 'git.changes', { cwd, diff: false })
      .then((r) => {
        setGitReady(true);
        const sig = `${r.repo}|${r.branch}|${r.head}|${r.files.map((f) => `${f.status} ${f.path} ${f.added ?? ''}/${f.deleted ?? ''}`).join('|')}`;
        if (quiet && sig === gitSig.current) return refreshCurDiff.current();
        gitSig.current = sig;
        setFileDiffs({});
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
        const info = { repo: r.repo, branch: r.branch ?? '', head: r.head, files, dirs };
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
        const commit = { hash: r.hash, rel: ct.rel, subject: r.message.split('\n')[0], author: r.author, time: r.time, status: f?.status ?? '', diff: f?.diff ?? '' };
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
      title: tr(`放弃“${baseName(file!.path)}”的更改？`, `Discard changes to "${baseName(file!.path)}"?`),
      message: tr('未保存的编辑会被丢弃。', 'Unsaved edits will be discarded.'),
      okText: tr('放弃更改', 'Discard'),
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
      { label: tr('关闭其他', 'Close others'), disabled: tabs.length < 2, run: () => keepTabs(t, [t]) },
      { label: tr('关闭左侧', 'Close to the left'), disabled: i <= 0, run: () => keepTabs(t, tabs.slice(i)) },
      { label: tr('关闭右侧', 'Close to the right'), disabled: i < 0 || i >= tabs.length - 1, run: () => keepTabs(t, tabs.slice(0, i + 1)) },
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
      if (window.matchMedia('(max-width: 760px)').matches) setTreeOpen(false);
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
      toast(tr(`已读取磁盘上的 ${baseName(f.path)}`, `Reloaded ${baseName(f.path)} from disk`));
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
      toast(tr(`已保存 ${baseName(f.path)}`, `Saved ${baseName(f.path)}`));
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
    const name = await askText({ title: kind === 'file' ? tr('新建文件', 'New file') : tr('新建文件夹', 'New folder'), label: dir, placeholder: tr('名称', 'Name') });
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
    const name = await askText({ title: tr('重命名', 'Rename'), value: old });
    if (!name?.trim() || name.trim() === old) return;
    const to = join(parentOf(path), name.trim());
    try {
      await rpc(hostId, 'fs.rename', { from: path, to });
      if (file?.path === path) setFile({ ...file, path: to });
      setTabs((t) => t.map((x) => (x === path ? to : x)));
      await load(parentOf(path));
    } catch (err) {
      errorToast(err);
    }
  };

  const remove = async (path: string, isDir: boolean) => {
    const name = baseName(path);
    const ok = await askConfirm({
      title: isDir ? tr(`删除文件夹 ${name}？`, `Delete folder ${name}?`) : tr(`删除 ${name}？`, `Delete ${name}?`),
      message: isDir
        ? tr(`${path}\n删除后无法恢复（只能删除空文件夹）`, `${path}\nThis cannot be undone (empty folders only)`)
        : tr(`${path}\n删除后无法恢复`, `${path}\nThis cannot be undone`),
      okText: tr('删除', 'Delete'),
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
  const copy = async (text: string, ok = tr('路径已复制', 'Path copied')) => toast((await writeClipboard(text)) ? ok : tr('无法复制', 'Could not copy'));
  const copyRef = async (path: string, isDir: boolean) => {
    const r = rel(path) + (isDir ? '/' : '');
    if (!(await writeClipboard(r))) return toast(tr('无法复制', 'Could not copy'));
    copiedRef = { plain: r, ref: `@${r} ` };
    toast(standalone ? tr('已复制相对路径', 'Relative path copied') : tr('已复制，在输入框里粘贴就是引用这个文件', 'Copied. Paste in the input to reference this file'));
  };
  // The row last clicked / right-clicked in the tree; Ctrl+C copies it
  const lastRow = useRef<{ path: string; isDir: boolean } | null>(null);
  const addToChat = (path: string, isDir: boolean) => {
    window.dispatchEvent(new CustomEvent(INSERT_COMPOSER, { detail: { sessionId, text: `@${rel(path)}${isDir ? '/' : ''} ` } }));
    if (window.matchMedia('(max-width: 760px)').matches) {
      onClose();
      toast(tr('已添加到输入框', 'Added to input'));
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
          if (await invoke<boolean>('save_file', { name, data: r.base64 })) toast(tr(`已保存 ${name}`, `Saved ${name}`));
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
      errorToast(String((e as Error)?.message ?? e).includes('未知方法') ? tr('守护进程版本太旧，重启守护进程后可用', 'Daemon is too old. Restart it to use this.') : e),
    );
  const nativeItems = (path: string): MenuItem[] =>
    isLocal
      ? [
          { label: tr('用系统默认程序打开', 'Open with default app'), icon: 'external', run: () => openNative(path, false) },
          { label: tr('在文件管理器中显示', 'Show in file manager'), icon: 'folder', run: () => openNative(path, true) },
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
    if (file.binary) return toast(tr('当前文件不是文本，不能比较', 'Current file is not text and cannot be compared'));
    const f = await readFile(path);
    if (!f) return;
    if (f.binary) return toast(tr(`${baseName(path)} 不是文本文件，不能比较`, `${baseName(path)} is not a text file and cannot be compared`));
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
        if (f.size > UPLOAD_MAX) throw new Error(tr(`${f.name} 超过 50MB`, `${f.name} exceeds 50MB`));
        const data = toB64(await f.arrayBuffer());
        const path = join(dir, f.name);
        try {
          await rpc(hostId, 'fs.upload', { path, data });
        } catch (e) {
          if (!String((e as Error)?.message ?? e).includes('已经存在')) throw e;
          const yes = await askConfirm({
            title: tr(`覆盖 ${f.name}？`, `Overwrite ${f.name}?`),
            message: tr(`${dir} 里已经有同名文件。`, `${dir} already has a file with this name.`),
            okText: tr('覆盖', 'Overwrite'),
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
    if (ok) toast(tr(`已上传 ${ok} 个文件到 ${rel(dir)}`, `Uploaded ${ok} files to ${rel(dir)}`));
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

  const rowMenu = (ev: React.MouseEvent, r: TreeRow) => {
    if (r.loading) return;
    const dir = r.isDir ? r.path : parentOf(r.path);
    const canSide = !r.isDir && !!file && !file.web && !file.commit && file.path !== r.path && wide();
    openMenu(ev, [
      r.isDir
        ? { label: r.expanded ? tr('收起', 'Collapse') : tr('展开', 'Expand'), icon: 'folder', run: () => toggle(r.path) }
        : { label: tr('打开', 'Open'), icon: 'file', run: () => openFile(r.path) },
      ...(!r.isDir && isHtml(r.path) ? [{ label: tr('在浏览器标签中打开', 'Open in browser tab'), icon: 'globe', run: () => openWeb(`file:${r.path}`) }] : []),
      ...(canSide
        ? [
            { label: tr('在右侧打开', 'Open to the side'), icon: 'columns', run: () => openSecond(r.path) },
            { label: tr(`与 ${baseName(file!.path)} 比较`, `Compare with ${baseName(file!.path)}`), icon: 'compare', run: () => compareWith(r.path) },
          ]
        : []),
      ...(standalone ? [] : [{ label: tr('添加到对话', 'Add to chat'), icon: 'chat', run: () => addToChat(r.path, r.isDir) }]),
      { label: tr('复制', 'Copy'), icon: 'copy', hint: `${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}C`, run: () => copyRef(r.path, r.isDir) },
      '-',
      ...(r.isDir ? [] : [{ label: tr('下载', 'Download'), icon: 'download', run: () => download(r.path) }]),
      ...nativeItems(r.path),
      { label: tr('复制路径', 'Copy path'), icon: 'copy', run: () => copy(r.path) },
      { label: tr('复制相对路径', 'Copy relative path'), run: () => copy(rel(r.path)) },
      '-',
      { label: tr('新建文件', 'New file'), icon: 'plus', run: () => create(dir, 'file') },
      { label: tr('新建文件夹', 'New folder'), icon: 'folder', run: () => create(dir, 'dir') },
      '-',
      { label: tr('重命名', 'Rename'), icon: 'edit', run: () => rename(r.path) },
      { label: tr('删除', 'Delete'), icon: 'trash', danger: true, run: () => remove(r.path, r.isDir) },
    ]);
  };

  const fileMenu = (e: React.MouseEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    const items: (MenuItem | '-')[] = file
      ? [
          { label: tr('跳到符号…', 'Go to symbol…'), icon: 'search', hint: `${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}Shift+O`, run: () => setQuick('@') },
          { label: tr('跳到行…', 'Go to line…'), hint: `${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}G`, run: () => setQuick(':') },
          '-',
          { label: tr('下载', 'Download'), icon: 'download', disabled: !!file.deleted, run: () => download(file.path) },
          ...nativeItems(file.path),
          { label: tr('复制路径', 'Copy path'), icon: 'copy', run: () => copy(file.path) },
          { label: tr('复制相对路径', 'Copy relative path'), run: () => copy(rel(file.path)) },
          { label: tr('复制文件内容', 'Copy file contents'), icon: 'copy', disabled: !!file.image || file.binary, run: () => copy(file.draft, tr('已复制文件内容', 'File contents copied')) },
          ...(standalone
            ? []
            : [
                { label: tr('添加到对话', 'Add to chat'), icon: 'chat', run: () => addToChat(file.path, false) },
                { label: tr('复制（粘贴到输入框即引用）', 'Copy (paste in input to reference)'), run: () => copyRef(file.path, false) },
              ]),
          '-',
          { label: wrap ? tr('关闭自动换行', 'Disable word wrap') : tr('启用自动换行', 'Enable word wrap'), icon: 'wrap', run: () => (lsSet('wp-wrap', !wrap), setWrap(!wrap)) },
          dirty
            ? { label: tr('放弃更改并重新加载', 'Discard and reload'), icon: 'refresh', danger: true, run: async () => (await confirmDiscard()) && reload() }
            : { label: tr('重新加载', 'Reload'), icon: 'refresh', run: reload },
          '-',
          { label: tr('重命名', 'Rename'), icon: 'edit', run: () => rename(file.path) },
        ]
      : [
          { label: tr('复制目录路径', 'Copy directory path'), icon: 'copy', run: () => copy(root ?? cwd) },
          { label: hidden ? tr('不显示隐藏文件', 'Hide hidden files') : tr('显示隐藏文件', 'Show hidden files'), icon: 'eye', run: () => (lsSet('wp-hidden', !hidden), setHidden(!hidden)) },
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
      { label: tr('读取磁盘版本（丢弃当前编辑）', 'Load from disk (discard edits)'), icon: 'refresh', run: reload },
      { label: tr('用当前内容覆盖磁盘', 'Overwrite disk with current contents'), icon: 'save', danger: true, run: () => save(true) },
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
  // When the session recorded changed files, list only those by default (what ChangesView used to call "only files changed in this session")
  const mineFilter = onlyMine && touched.size > 0;
  const allChanged = useMemo(() => {
    if (!root) return [];
    const keys = git ? [...git.files.keys()] : gitReady ? [...touched.keys()] : [];
    const r = norm(root);
    return keys.filter((k) => k.startsWith(r + '/'));
  }, [git, gitReady, touched, root]);
  const changedKeys = useMemo(() => allChanged.filter((k) => !git || !mineFilter || touched.has(k)), [allChanged, git, mineFilter, touched]);
  const changed = useMemo(() => {
    if (!root) return [];
    const r = norm(root);
    // Keys are normalized (lowercased on Windows drives): take the original spelling from git status when there is one
    return changedKeys.map((k) => (git?.files.get(k)?.path.replace(/\\/g, '/') ?? k).slice(r.length + 1));
  }, [changedKeys, root, git]);
  const otherCount = allChanged.length - changedKeys.length;
  // Totals at the top of the Changes view: lines added/removed across the listed files (files that can't be computed are skipped)
  const totals = useMemo(() => {
    let a = 0, d = 0, any = false;
    for (const k of changedKeys) {
      const f = git?.files.get(k);
      if (f?.added != null || f?.deleted != null) any = true;
      a += f?.added ?? 0;
      d += f?.deleted ?? 0;
    }
    return any ? { a, d } : null;
  }, [changedKeys, git]);
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
  // "View changes" from the session menu
  useEffect(() => {
    const on = (e: Event) => {
      const d = (e as CustomEvent<{ hostId: string; sessionId: string }>).detail;
      if (d.hostId !== hostId || d.sessionId !== sessionId) return;
      pickView('changes');
      setTreeOpen(true);
      loadGit();
    };
    window.addEventListener(OPEN_CHANGES, on);
    return () => window.removeEventListener(OPEN_CHANGES, on);
  }, [hostId, sessionId, loadGit]);
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

  const toggleHidden = () => (lsSet('wp-hidden', !hidden), setHidden(!hidden));
  const newMenu = (e: React.MouseEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    openMenu({ clientX: r.right - 160, clientY: r.bottom + 4 }, [
      { label: tr('新建文件', 'New file'), icon: 'file', run: () => root && create(root, 'file') },
      { label: tr('新建文件夹', 'New folder'), icon: 'folder', run: () => root && create(root, 'dir') },
      '-',
      { label: tr('打开网页…', 'Open web page…'), icon: 'globe', run: () => openWeb() },
    ]);
  };
  const panelBtns = standalone ? null : (
    <>
      <button className="icon-btn hide-mobile" title={fullscreen ? tr('退出全屏', 'Exit full screen') : tr('全屏', 'Full screen')} onClick={onFullscreen}>
        <Icon name={fullscreen ? 'minimize' : 'maximize'} size={14} />
      </button>
      <button className="icon-btn" title={tr('关闭', 'Close')} onClick={closePanel}>
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
        <div className="wp-views" role="tablist" aria-label={tr('文件视图', 'File views')}>
          {viewTab('all', tr('文件', 'Files'))}
          {viewTab(
            'changes',
            <>
              {tr('改动', 'Changes')}
              {changed.length > 0 && <span className="wp-count">{changed.length}</span>}
            </>,
            {
              // Usable while git status is still loading (the commit history below doesn't wait for it)
              disabled: gitReady && !git && touched.size === 0,
              title: git || !gitReady
                ? tr('工作区里未提交的改动', 'Uncommitted changes in the workspace')
                : touched.size
                  ? tr('这个会话里工具改过的文件', 'Files changed by tools in this session')
                  : tr('不是 git 仓库，这个会话也还没改过文件', 'Not a git repo, and this session has not changed any files'),
            },
          )}
          {viewTab('search', tr('搜索', 'Search'), { title: tr('在文件内容里搜索', 'Search in file contents') })}
        </div>
        <div className="grow" />
        <button className="icon-btn" title={tr(`快速打开（${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}P）`, `Quick open (${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}P)`)} onClick={() => setQuick('')}>
          <Icon name="zoom" size={14} />
        </button>
        <button className="icon-btn" title={tr('新建', 'New')} disabled={!root} onClick={newMenu}>
          <Icon name="plus" size={15} />
        </button>
        <button className="icon-btn" title={tr('刷新', 'Refresh')} onClick={refreshTree}>
          <Icon name="refresh" size={14} />
        </button>
      </div>
      {view === 'search' && root ? (
        <GrepPanel hostId={hostId} root={root} onOpen={(r, line, col, len) => openFile(relOpen(r), false, { line, col, len })} />
      ) : (
        <>
          {view === 'all' && (
            <div className="wp-filter">
              <Icon name="search" size={13} />
              <input
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
                placeholder={tr('按文件名筛选…', 'Filter by file name…')}
                aria-label={tr('按文件名筛选', 'Filter by file name')}
                onKeyDown={(e) => e.key === 'Escape' && setFilter('')}
              />
              {filter && (
                <button className="icon-btn" title={tr('清除', 'Clear')} onClick={() => setFilter('')}>
                  <Icon name="x" size={12} />
                </button>
              )}
            </div>
          )}
          <div
            className={`wp-tree ${dropDir === root ? 'drop' : ''} ${histGrow ? 'shrink' : ''}`}
            role="tree"
            aria-label={tr('文件树', 'File tree')}
            tabIndex={-1}
            {...treeDrop}
            onPointerDownCapture={(e) => {
              const row = (e.target as Element).closest<HTMLElement>('[data-path]');
              if (row) lastRow.current = { path: row.dataset.path!, isDir: row.dataset.dir === '1' };
            }}
            onKeyDown={(e) => {
              if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'c' && lastRow.current && !window.getSelection()?.toString()) {
                e.preventDefault();
                copyRef(lastRow.current.path, lastRow.current.isDir);
              }
            }}
            onContextMenu={(e) =>
              root &&
              !q &&
              openMenu(e, [
                { label: tr('新建文件', 'New file'), icon: 'plus', run: () => create(root, 'file') },
                { label: tr('新建文件夹', 'New folder'), icon: 'folder', run: () => create(root, 'dir') },
                '-',
                { label: hidden ? tr('不显示隐藏文件', 'Hide hidden files') : tr('显示隐藏文件', 'Show hidden files'), icon: 'eye', run: toggleHidden },
                { label: tr('复制目录路径', 'Copy directory path'), icon: 'copy', run: () => copy(root) },
                ...(isLocal ? [{ label: tr('在文件管理器中显示', 'Show in file manager'), icon: 'folder', run: () => openNative(root, false) }] : []),
              ])
            }
          >
            {view === 'all' && q ? (
              !search || search.q !== q ? (
                <div className="wp-tip">{tr('正在查找…', 'Searching…')}</div>
              ) : search.error ? (
                <div className="wp-tip">
                  <b>{tr('暂时无法查找文件', 'Cannot search files right now')}</b>
                  {tr('请稍后重试，或清除筛选词继续浏览。', 'Try again later, or clear the filter to keep browsing.')}
                </div>
              ) : sRows.length === 0 ? (
                <div className="wp-tip">
                  <b>{tr('没有匹配的文件', 'No matching files')}</b>
                  {tr('换个关键词，或切到「搜索」在文件内容里找。', 'Try another term, or switch to Search to look inside files.')}
                </div>
              ) : (
                <>
                  <Rows
                    rows={sRows}
                    git={git}
                    selected={file?.path}
                    onToggle={toggleCollapsed}
                    onOpen={(r) => openFile(r.path)}
                    onMenu={rowMenu}
                    onDragRow={dragRow}
                  />
                  {search.truncated && <div className="wp-limit">{tr(`只显示前 ${search.paths.length} 个结果，请缩小范围。`, `Showing the first ${search.paths.length} results. Narrow the search.`)}</div>}
                </>
              )
            ) : view === 'changes' && !gitReady ? (
              <div className="wp-tip">{tr('正在读取改动…', 'Reading changes…')}</div>
            ) : view === 'changes' && (git || touched.size > 0) ? (
              <>
                {git && touched.size > 0 && (
                  <label className="wp-mine">
                    <input type="checkbox" checked={onlyMine} onChange={(e) => setOnlyMine(e.target.checked)} />
                    {tr('只看这个会话改的', 'Only this session')}
                    {mineFilter && otherCount > 0 && <span className="muted">{tr(`（另有 ${otherCount} 个）`, ` (${otherCount} more)`)}</span>}
                  </label>
                )}
                {!git && <div className="wp-limit">{tr('不是 git 仓库：下面是这个会话里工具改过的文件，只能查看。', 'Not a git repo. Files below were changed by tools in this session and are view-only.')}</div>}
                {totals && changed.length > 0 && (
                  <div className="wp-totals">
                    {tr(`${changed.length} 个文件`, `${changed.length} files`)}
                    <LineStat a={totals.a} d={totals.d} />
                  </div>
                )}
                {cRows.length ? (
                  <Rows
                    rows={cRows}
                    stats
                    git={git}
                    selected={file?.path}
                    onToggle={toggleCollapsed}
                    onOpen={(r) => openFile(r.path, true)}
                    onMenu={rowMenu}
                    onDragRow={dragRow}
                  />
                ) : (
                  <div className="wp-tip">
                    <b>{tr('没有改动', 'No changes')}</b>
                    {otherCount > 0
                      ? tr('这个会话改的文件都已经提交或撤销了。', 'Files changed in this session are already committed or reverted.')
                      : tr('工作区和上次提交一致。', 'Working tree matches the last commit.')}
                  </div>
                )}
              </>
            ) : root ? (
              rows.length ? (
                <Rows
                  rows={rows}
                  git={git}
                  selected={file?.path}
                  onToggle={(r) => toggle(r.path)}
                  onOpen={(r) => openFile(r.path)}
                  onMenu={rowMenu}
                  onDragRow={dragRow}
                  dropDir={dropDir}
                  onDropOver={(d) => dropDir !== d && setDropDir(d)}
                />
              ) : (
                <div className="wp-tip">{tr('这个目录为空。', 'This folder is empty.')}</div>
              )
            ) : (
              <div className="wp-tip">{tr('正在加载…', 'Loading…')}</div>
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
                  <span className="spinner" /> {tr(`正在上传 ${uploading} 个文件…`, `Uploading ${uploading} files…`)}
                </>
              ) : (
                <>
                  <Icon name="upload" size={13} /> {tr(`松开上传到 ${rel(dropDir!)}`, `Drop to upload to ${rel(dropDir!)}`)}
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
      value={file.content}
      filename={baseName(file.path)}
      readOnly={file.truncated}
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
    file.commit.diff ? (
      <div className="wp-diff">
        <PaneDiff diff={{ path: file.commit.rel, unified: file.commit.diff.slice(Math.max(0, file.commit.diff.indexOf('@@'))) }} layout={layout} />
      </div>
    ) : (
      <div className="wp-empty">
        <div className="wp-empty-title">{tr('没有可显示的改动', 'No changes to show')}</div>
        <div className="muted small">{tr('文件太大、是二进制文件，或只改了权限。', 'File is too large, binary, or only its mode changed.')}</div>
      </div>
    )
  ) : file.web ? (
    <BrowserPane hostId={hostId} root={root ?? cwd} tab={file.path} onNavigate={(t) => navigateWeb(file.path, t)} />
  ) : html && preview && viewing === 'file' ? (
    <BrowserPane hostId={hostId} root={root ?? cwd} tab={`${WEB_PREFIX}file:${file.path}`} reload={file.modified} onNavigate={(t) => openFile(t)} />
  ) : viewing === 'compare' ? (
    <div className="wp-diff">
      <PaneDiff diff={{ path: rel(file.path), oldText: file.draft, newText: compare!.text }} layout={layout} />
    </div>
  ) : viewing === 'diff' ? (
    fileDiff ? (
      <div className="wp-diff">
        <PaneDiff diff={{ ...fileDiff, path: rel(file.path) }} layout={layout} />
      </div>
    ) : diffPending ? (
      <div className="wp-empty">
        <div className="muted small">
          <span className="spinner" /> {tr('正在读取改动…', 'Reading changes…')}
        </div>
      </div>
    ) : fileSessionDiffs.length ? (
      <div className="wp-diff">
        <div className="wp-limit">{tr(`不是 git 仓库：下面是这个会话里工具对这个文件做的 ${fileSessionDiffs.length} 次修改，每次只显示改动的那一段。`, `Not a git repo. Below are ${fileSessionDiffs.length} edits tools made to this file in this session; each shows only the changed part.`)}</div>
        {fileSessionDiffs.map((d, i) => (
          <PaneDiff key={i} diff={{ ...d, path: rel(file.path) }} layout={layout} />
        ))}
      </div>
    ) : (
      <div className="wp-empty">
        <div className="wp-empty-title">{tr('没有可显示的改动', 'No changes to show')}</div>
        <div className="muted small">{tr('文件太大或是二进制文件。', 'File is too large or binary.')}</div>
      </div>
    )
  ) : file.image ? (
    <ImageView key={`${file.path}#${file.rev}`} src={file.image} name={baseName(file.path)} size={fmtSize(file.size)} />
  ) : file.pdf ? (
    <PdfView base64={file.pdf} onDownload={() => download(file.path)} />
  ) : file.binary ? (
    <div className="wp-empty">
      <div className="wp-empty-title">{tr('无法预览这种文件', 'Cannot preview this file')}</div>
      <div className="muted small">{tr(`暂不支持这种文件的格式、编码或大小（${fmtSize(file.size)}）。`, `Format, encoding, or size (${fmtSize(file.size)}) is not supported.`)}</div>
      <div className="row wp-empty-acts">
        <button className="btn small" onClick={() => download(file.path)}>
          <Icon name="download" size={13} /> {tr('下载', 'Download')}
        </button>
        {isLocal && (
          <button className="btn small" onClick={() => openNative(file.path, false)}>
            {tr('用系统程序打开', 'Open with system app')}
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
        <span className="muted small">{tr('只读', 'Read-only')}</span>
        <div className="grow" />
        <button className="icon-btn" title={tr(`与 ${baseName(file.path)} 比较`, `Compare with ${baseName(file.path)}`)} onClick={() => setCompare({ path: second.path, text: second.content })}>
          <Icon name="compare" size={13} />
        </button>
        <button className="icon-btn" title={tr('在左边编辑', 'Edit on the left')} onClick={() => (setSecond(null), openFile(second.path))}>
          <Icon name="back" size={13} />
        </button>
        <button className="icon-btn" title={tr('关闭', 'Close')} onClick={() => setSecond(null)}>
          <Icon name="x" size={13} />
        </button>
      </div>
      {second.binary ? (
        <div className="wp-empty">
          <div className="wp-empty-title">{tr('不是文本文件', 'Not a text file')}</div>
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
            <span className="wp-branch truncate" title={tr(`git 分支 ${git.branch}`, `git branch ${git.branch}`)}>
              {git.branch}
            </span>
          )}
          <div className="grow" />
          <button className="icon-btn" title={tr('打开网页（新的浏览器标签）', 'Open web page (new browser tab)')} onClick={() => openWeb()}>
            <Icon name="globe" size={14} />
          </button>
          {panelBtns}
        </div>
      ) : (
        <div className="wp-top wp-tabbar">
          <div
            className="wp-filetabs"
            role="tablist"
            aria-label={tr('打开的文件', 'Open files')}
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
                    title={`${ct.rel}\n${tr('提交', 'Commit')} ${ct.hash.slice(0, 10)}${cur && file.commit ? ` · ${file.commit.subject}` : ''}`}
                    onClick={() => openFile(t)}
                    onAuxClick={(e) => e.button === 1 && closeTab(t)}
                    onContextMenu={(e) =>
                      openMenu(e, [
                        { label: tr('关闭', 'Close'), icon: 'x', run: () => closeTab(t) },
                        ...closeItems(t),
                        '-',
                        { label: tr('复制提交号', 'Copy commit hash'), icon: 'copy', run: () => copy(ct.hash, tr('提交号已复制', 'Commit hash copied')) },
                      ])
                    }
                  >
                    <Icon name="diff" size={13} />
                    <span className="truncate">{baseName(ct.rel)}</span>
                    <span className="wp-ftab-hash">{ct.hash.slice(0, 7)}</span>
                    <button
                      className="wp-ftab-x"
                      title={tr('关闭', 'Close')}
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
                    title={t.slice(WEB_PREFIX.length) || tr('新标签', 'New tab')}
                    onClick={() => openFile(t)}
                    onAuxClick={(e) => e.button === 1 && closeTab(t)}
                    onContextMenu={(e) =>
                      openMenu(e, [
                        { label: tr('关闭', 'Close'), icon: 'x', run: () => closeTab(t) },
                        ...closeItems(t),
                      ])
                    }
                  >
                    <Icon name="globe" size={13} />
                    <span className="truncate">{webTabTitle(t)}</span>
                    <button
                      className="wp-ftab-x"
                      title={tr('关闭', 'Close')}
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
                            { label: tr('在右侧打开', 'Open to the side'), icon: 'columns', run: () => openSecond(t) },
                            { label: tr(`与 ${baseName(file.path)} 比较`, `Compare with ${baseName(file.path)}`), icon: 'compare', run: () => compareWith(t) },
                            '-' as const,
                          ]
                        : []),
                      { label: tr('关闭', 'Close'), icon: 'x', run: () => closeTab(t) },
                      ...closeItems(t),
                      '-',
                      { label: tr('下载', 'Download'), icon: 'download', run: () => download(t) },
                      { label: tr('复制（粘贴到输入框即引用）', 'Copy (paste in input to reference)'), icon: 'copy', run: () => copyRef(t, false) },
                      { label: tr('复制相对路径', 'Copy relative path'), run: () => copy(rel(t)) },
                    ])
                  }
                >
                  <FileIcon name={baseName(t)} size={13} />
                  <span className="truncate">{baseName(t)}</span>
                  <button
                    className={`wp-ftab-x ${cur && dirty ? 'dirty' : ''}`}
                    title={tr('关闭', 'Close')}
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
            <button className="icon-btn wp-tabs-more" title={tr(`全部打开的文件（${tabs.length}）`, `All open files (${tabs.length})`)} onClick={tabsMenu}>
              <Icon name="down" size={14} />
            </button>
          )}
          <button className="icon-btn" title={tr('打开网页（新的浏览器标签）', 'Open web page (new browser tab)')} onClick={() => openWeb()}>
            <Icon name="globe" size={14} />
          </button>
          <button
            className={`icon-btn ${treeOpen ? 'active' : ''}`}
            title={treeOpen ? tr('收起文件树', 'Hide file tree') : tr('显示文件树', 'Show file tree')}
            onClick={() => (lsSet('wp-tree', !treeOpen), setTreeOpen(!treeOpen))}
          >
            <Icon name="fileTree" size={14} />
          </button>
          {panelBtns}
        </div>
      )}

      {file?.commit && (
        <div className="wp-head">
          <div className="wp-commit-title truncate" title={`${file.commit.hash}\n${file.commit.author} · ${new Date(file.commit.time * 1000).toLocaleString()}\n\n${file.commit.subject}\n${file.commit.rel}`}>
            <span className="pd-tag">{file.commit.hash.slice(0, 7)}</span>
            <span className="truncate">{file.commit.subject}</span>
            <span className="muted truncate">{file.commit.rel}</span>
          </div>
          <div className="wp-actions">
            <span className="hide-mobile">{tr('只读', 'Read-only')}</span>
            {wide() && (
              <button className="icon-btn" title={layout === 'split' ? tr('改成上下对照', 'Switch to stacked') : tr('改成左右并排', 'Switch to side by side')} onClick={flipLayout}>
                <Icon name={layout === 'split' ? 'rows' : 'columns'} size={14} />
              </button>
            )}
            {git && !gitMark(file.commit.status)[0].includes('D') && (
              <button
                className="icon-btn"
                title={tr('打开工作区里的这个文件', 'Open this file in the workspace')}
                onClick={() => openFile(join(git.repo, file.commit!.rel.split('/').join(sepOf(git.repo))))}
              >
                <Icon name="file" size={14} />
              </button>
            )}
          </div>
        </div>
      )}
      {file && !file.web && !file.commit && (
        <div className="wp-head">
          {compare ? (
            <div className="wp-compare-title truncate" title={`${file.path}\n${compare.path}`}>
              <span className="pd-tag del">{baseName(file.path)}</span>
              <Icon name="forward" size={12} />
              <span className="pd-tag add">{rel(compare.path)}</span>
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
            {file.truncated && <span>{tr('只读 · 前 1MB', 'Read-only · first 1MB')}</span>}
            {dirty &&
              (saveState === 'conflict' ? (
                <button className="btn small primary" onClick={conflictMenu}>
                  {tr('解决冲突', 'Resolve conflict')}
                </button>
              ) : (
                <button className="btn small primary" disabled={saveState === 'saving'} onClick={() => save()} title={tr('保存（Ctrl+S）', 'Save (Ctrl+S)')}>
                  <Icon name="save" size={13} /> {tr('保存', 'Save')}
                </button>
              ))}
            {compare && (
              <button className="btn small" onClick={() => setCompare(null)}>
                {tr('结束比较', 'End compare')}
              </button>
            )}
            {hasChange(file.path) && !file.deleted && !compare && (
              <div className="seg wp-seg">
                <button className={!showDiff ? 'active' : ''} onClick={() => setShowDiff(false)}>
                  {tr('文件', 'File')}
                </button>
                <button className={showDiff ? 'active' : ''} onClick={() => setShowDiff(true)} title={tr('和上次提交相比改了什么', 'Changes since the last commit')}>
                  {tr('改动', 'Changes')}
                </button>
              </div>
            )}
            {viewing !== 'file' && wide() && (
              <button className="icon-btn" title={layout === 'split' ? tr('改成上下对照', 'Switch to stacked') : tr('改成左右并排', 'Switch to side by side')} onClick={flipLayout}>
                <Icon name={layout === 'split' ? 'rows' : 'columns'} size={14} />
              </button>
            )}
            {(csv || html) && viewing === 'file' && (
              <div className="seg wp-seg">
                <button className={!preview ? 'active' : ''} onClick={() => setPreview(false)}>
                  {tr('源码', 'Source')}
                </button>
                <button
                  className={preview ? 'active' : ''}
                  onClick={() => setPreview(true)}
                  title={html ? tr('按保存在磁盘上的内容显示，保存后自动刷新', 'Shows the saved file. Refreshes after save.') : undefined}
                >
                  {csv ? tr('表格', 'Table') : tr('预览', 'Preview')}
                </button>
              </div>
            )}
            {md && viewing === 'file' && (
              <div className="seg wp-seg">
                <button className={!preview && !(mdSplit && wide()) ? 'active' : ''} onClick={() => pickMdMode('source')}>
                  {tr('源码', 'Source')}
                </button>
                {wide() && (
                  <button className={!preview && mdSplit ? 'active' : ''} onClick={() => pickMdMode('split')} title={tr('左边编辑，右边实时预览', 'Edit on the left, live preview on the right')}>
                    {tr('分栏', 'Split')}
                  </button>
                )}
                <button className={preview ? 'active' : ''} onClick={() => pickMdMode('preview')}>
                  {tr('预览', 'Preview')}
                </button>
              </div>
            )}
            {viewing === 'file' && !file.binary && (!preview || md) && (
              <button className="icon-btn" title={tr(`大纲：跳到函数、类、标题（${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}Shift+O）`, `Outline: go to a function, class, or heading (${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}Shift+O)`)} onClick={() => setQuick('@')}>
                <Icon name="outline" size={14} />
              </button>
            )}
            <button className="icon-btn" title={tr('更多文件操作', 'More file actions')} onClick={fileMenu}>
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
                ? tr('在右侧选一个改动的文件或提交查看差异', 'Pick a changed file or a commit on the right to see the diff')
                : tr('在右侧文件树里选一个文件打开', 'Pick a file in the tree on the right')}
            </div>
            <div className="muted small">
              {tr(`${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}P 按文件名快速打开`, `${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl+'}P to open a file by name`)}
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
    <div className="wp-crumbs" ref={box} title={path} aria-label={tr('文件路径', 'File path')}>
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
                {dirty && <span className="wp-dot" title={tr('有未保存的修改', 'Unsaved changes')}>•</span>}
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
    <div className="wp-pop" ref={box} style={{ left: x, top: y }} role="tree" aria-label={tr('目录内容', 'Folder contents')}>
      {!kids[dir] ? (
        <div className="wp-tip">{tr('正在加载文件…', 'Loading files…')}</div>
      ) : rows.length === 0 ? (
        <div className="wp-tip">{tr('这个目录为空。', 'This folder is empty.')}</div>
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
