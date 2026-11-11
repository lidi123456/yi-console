// File links in chat replies: renderMarkdown renders [name](/path/file.rs:12) as <a class="md-file" data-path data-line> (no href),
// this adds the file icon and opens the file in the session's file panel on click / Enter.
import type { ImgCtx } from './chatImages';
import { filePath } from './chatImages';
import { fileGlyph } from './components/FilePanel';
import { openFileInPanel } from './components/SessionActions';
import { getT } from './i18n';
import { rpc } from './transport';
import { basename } from './util';

const t = getT('chatView');

const SVG = 'http://www.w3.org/2000/svg';

/** Folder links (written with a trailing slash) get a folder icon; ones without one only show as folders when clicked */
const FOLDER: [string, string] = ['M1.5 3.5h4.5l1.5 1.5h7v8h-13z', 'var(--text-3)'];

function icon(target: string) {
  const [d, color] = /[\\/]$/.test(target) ? FOLDER : fileGlyph(basename(target));
  const svg = document.createElementNS(SVG, 'svg');
  for (const [k, v] of Object.entries({ width: '14', height: '14', viewBox: '0 0 16 16', fill: 'none', stroke: color, 'stroke-width': '1.4', 'stroke-linecap': 'round', 'stroke-linejoin': 'round', 'aria-hidden': 'true' }))
    svg.setAttribute(k, v);
  const path = document.createElementNS(SVG, 'path');
  path.setAttribute('d', d);
  svg.append(path);
  return svg;
}

/** Fill in icons of file links in root not handled yet; without a session to open them in they stay plain chips */
export function hydrateFileLinks(root: HTMLElement, ctx: ImgCtx | null) {
  for (const el of root.querySelectorAll<HTMLElement>('a.md-file:not([data-done])')) {
    el.dataset.done = '1';
    el.querySelector('.md-file-icon')?.replaceChildren(icon(el.dataset.path ?? ''));
    if (!ctx?.sessionId) {
      el.classList.add('static');
      el.removeAttribute('tabindex');
    }
  }
}

// ---------- Path-like inline code (`src/main.py:331`): a link only once the file is known to exist ----------

type Listing = Map<string, boolean>; // entry name (lowercase on Windows drives) -> is a folder
/** hostId + folder -> its listing; entries expire so files the agent creates during a turn show up */
const listings = new Map<string, { at: number; p: Promise<Listing | null>; v?: Listing | null }>();
const LISTING_TTL = 20_000;
/** At most this many folders are listed per redraw, so a reply full of paths doesn't flood a remote host */
const MAX_DIRS = 30;

const winPath = (p: string) => /^[a-zA-Z]:[\\/]/.test(p);
const nameKey = (name: string, path: string) => (winPath(path) ? name.toLowerCase() : name);
function splitPath(p: string): { dir: string; name: string } | null {
  const q = p.replace(/[\\/]+$/, '');
  const i = Math.max(q.lastIndexOf('/'), q.lastIndexOf('\\'));
  if (i < 0) return null;
  return { dir: q.slice(0, i) || q.slice(0, i + 1) || '/', name: q.slice(i + 1) };
}

function listing(hostId: string, dir: string) {
  const k = `${hostId}\n${dir}`;
  const c = listings.get(k);
  if (c && Date.now() - c.at < LISTING_TTL) return c;
  const e: { at: number; p: Promise<Listing | null>; v?: Listing | null } = {
    at: Date.now(),
    p: rpc<{ entries: { name: string; isDir: boolean }[] }>(hostId, 'fs.list', { path: dir, showHidden: true })
      .then((r) => new Map(r.entries.map((x) => [nameKey(x.name, dir), x.isDir])))
      .catch(() => null),
  };
  e.p.then((v) => (e.v = v));
  listings.set(k, e);
  return e;
}

/** Agents often write paths relative to a subfolder (`services/thread/a.py` for src/app/services/…) or just a file name.
 *  When the path isn't under cwd, look the name up in the workspace (fs.search, git ls-files on the host) and take the
 *  one file whose path ends with what the agent wrote; ambiguous names stay plain code */
const lookups = new Map<string, { at: number; p: Promise<string | null>; v?: string | null }>();
function lookup(hostId: string, cwd: string, rel: string) {
  const k = `${hostId}\n${cwd}\n${rel}`;
  const c = lookups.get(k);
  if (c && Date.now() - c.at < LISTING_TTL) return c;
  const want = rel.replace(/\\/g, '/').replace(/^\.\//, '');
  const name = want.slice(want.lastIndexOf('/') + 1);
  const fold = winPath(cwd) ? (x: string) => x.toLowerCase() : (x: string) => x;
  const p = rpc<{ path: string; isDir: boolean }[]>(hostId, 'fs.search', { root: cwd, query: name, limit: 200 })
    .then((r) => {
      const hits = r.filter((x) => !x.isDir && (fold(x.path) === fold(want) || fold(x.path).endsWith('/' + fold(want))));
      return hits.length === 1 ? filePath(hits[0].path, cwd) : null;
    })
    .catch(() => null);
  const e: { at: number; p: Promise<string | null>; v?: string | null } = { at: Date.now(), p };
  p.then((v) => (e.v = v));
  lookups.set(k, e);
  return e;
}

function makeLink(el: HTMLElement, abs: string, isDir: boolean) {
  el.classList.add('md-path');
  el.setAttribute('role', 'link');
  el.tabIndex = 0;
  el.dataset.abs = abs + (isDir && !/[\\/]$/.test(abs) ? '/' : '');
  const line = el.dataset.line ? `:${el.dataset.line}` : '';
  el.title = t('pathClickToOpen', { v: abs + line });
}

/** Check path-like inline code against the host and make the existing ones clickable. Called after every redraw;
 *  listings already fetched apply synchronously, so streaming text doesn't flicker */
export function hydratePaths(root: HTMLElement, ctx: ImgCtx | null) {
  if (!ctx?.sessionId) return;
  const host = ctx.hostId;
  const dirs = new Set<string>();
  for (const el of root.querySelectorAll<HTMLElement>('code.md-pathcand:not([data-checked])')) {
    // Inside a file link or a link of any kind the link already decides what a click does
    if (el.closest('a')) {
      el.dataset.checked = '1';
      continue;
    }
    const abs = filePath(el.dataset.path ?? '', ctx.cwd);
    const sp = splitPath(abs);
    if (!sp) {
      el.dataset.checked = '1';
      continue;
    }
    const entry = listings.get(`${host}\n${sp.dir}`);
    const known = entry && Date.now() - entry.at < LISTING_TTL && entry.v !== undefined ? entry : null;
    if (!known && !dirs.has(sp.dir) && dirs.size >= MAX_DIRS) continue;
    const e = known ?? listing(host, sp.dir);
    dirs.add(sp.dir);
    const raw = el.dataset.path ?? '';
    const relative = !!ctx.cwd && !/^([a-zA-Z]:)?[\\/]/.test(raw) && !raw.startsWith('~') && !/[\\/]$/.test(raw);
    const apply = (v: Listing | null | undefined) => {
      el.dataset.checked = '1';
      const isDir = v?.get(nameKey(sp.name, abs));
      if (isDir !== undefined) return el.isConnected && makeLink(el, abs, isDir);
      if (!relative) return;
      // Already looked up: apply right away so a streaming redraw doesn't flicker
      const l = lookup(host, ctx.cwd!, raw);
      const done = (found: string | null | undefined) => found && el.isConnected && makeLink(el, found, false);
      if (l.v !== undefined) done(l.v);
      else l.p.then(done);
    };
    if (e.v !== undefined) apply(e.v);
    else e.p.then(apply);
  }
}

/** Click / keyboard handler for the Markdown container: true when it was a file link */
export function openFileLink(e: { target: EventTarget | null; preventDefault(): void; stopPropagation(): void }, ctx: ImgCtx | null) {
  const el = (e.target as Element | null)?.closest?.<HTMLElement>('a.md-file, code.md-path');
  if (!el) return false;
  e.preventDefault();
  e.stopPropagation();
  const raw = el.dataset.abs ?? el.dataset.path;
  if (!raw || !ctx?.sessionId) return true;
  const line = Number(el.dataset.line) || undefined;
  let path = filePath(raw, ctx.cwd);
  // Windows hosts list paths with backslashes; links say D:/repo/a.rs. Match them, or the tab doesn't line up with
  // the tree (a second tab for the same file, absolute instead of relative paths in "add to chat")
  if (/^[a-zA-Z]:[\\/]/.test(path)) path = path.replace(/\//g, '\\');
  openFileInPanel(ctx.hostId, ctx.sessionId, path, line);
  return true;
}
