// Images in chat replies: renderMarkdown renders ![](…) and raw <img> as <span class="md-rimg">, and this fills in the content based on the source.
// - Files on the host: the local host reads and shows them directly; remote hosts (SSH / relay / forwarded) show a placeholder first, and on click ask for the size, then download
// - Public URLs: always show a placeholder first; the current device only loads them on click (opening a chat never requests third parties automatically)
// Images that were opened / confirmed are remembered for this run and shown directly when they reappear (streaming redraws, switching away and back).
import { getT } from './i18n';
import { rpc } from './transport';
import { askConfirm } from './components/Overlay';
import { resolvePath } from './components/FilePreview';
import { basename, fmtSize } from './util';

const t = getT('chatImages');

export interface ImgCtx {
  hostId: string;
  hostName: string;
  /** Local host: load images on the host directly */
  local: boolean;
  /** Session working directory; relative paths resolve against it */
  cwd?: string;
}

/** Host files: hostId + path -> data URL */
const fileCache = new Map<string, Promise<string>>();
/** Public URLs the user clicked to load */
const webAllowed = new Set<string>();

const isAbs = (p: string) => /^([a-zA-Z]:)?[\\/]/.test(p) || p.startsWith('~');

function filePath(src: string, cwd?: string) {
  return isAbs(src) || !cwd ? src : resolvePath(cwd, src);
}

function loadFile(hostId: string, path: string) {
  const key = `${hostId}\n${path}`;
  let p = fileCache.get(key);
  if (!p) {
    p = rpc<{ mimeType?: string; base64: string }>(hostId, 'fs.read', { path, image: true, strict: true }).then(
      (r) => `data:${r.mimeType || 'image/png'};base64,${r.base64}`,
    );
    p.catch(() => fileCache.delete(key));
    fileCache.set(key, p);
  }
  return p;
}

function errText(e: unknown) {
  const m = e instanceof Error ? e.message : String(e ?? '');
  return m.replace(/^Error:\s*/, '');
}

function showImg(el: HTMLElement, src: string, alt: string, onError?: () => void) {
  const img = document.createElement('img');
  img.alt = alt;
  img.title = el.dataset.title || alt;
  img.decoding = 'async';
  // Web images (loaded on click) don't tell the site which page showed them
  img.referrerPolicy = 'no-referrer';
  if (onError) img.onerror = onError;
  img.src = src;
  el.className = 'md-rimg ok';
  el.replaceChildren(img);
}

function showError(el: HTMLElement, msg: string, link?: string) {
  el.className = 'md-rimg err';
  const box = document.createElement('span');
  box.className = 'md-rimg-err';
  box.textContent = msg;
  if (link) {
    const a = document.createElement('a');
    a.href = link;
    a.textContent = link;
    a.title = t('openInBrowser');
    box.append(' ', a);
  }
  el.replaceChildren(box);
}

function showLoading(el: HTMLElement) {
  el.className = 'md-rimg loading';
  const s = document.createElement('span');
  s.className = 'md-rimg-ph';
  s.textContent = t('loadingImage');
  el.replaceChildren(s);
}

/** Placeholder button: icon + name + source; runs onClick when clicked */
function showPlaceholder(el: HTMLElement, label: string, hint: string, onClick: () => void) {
  el.className = 'md-rimg idle';
  const b = document.createElement('button');
  b.type = 'button';
  b.className = 'md-rimg-ph';
  const icon = document.createElement('span');
  icon.className = 'md-rimg-icon';
  icon.textContent = '🖼';
  const name = document.createElement('span');
  name.className = 'md-rimg-name';
  name.textContent = label;
  const h = document.createElement('span');
  h.className = 'md-rimg-hint';
  h.textContent = hint;
  b.append(icon, name, h);
  b.onclick = (e) => {
    // The image may be wrapped in a link
    e.preventDefault();
    e.stopPropagation();
    onClick();
  };
  el.replaceChildren(b);
}

function fillFile(el: HTMLElement, ctx: ImgCtx, path: string, alt: string) {
  const key = `${ctx.hostId}\n${path}`;
  const load = () => {
    showLoading(el);
    loadFile(ctx.hostId, path)
      .then((src) => el.isConnected && showImg(el, src, alt))
      .catch((e) => el.isConnected && showError(el, t('imageUnavailable', { path, v: errText(e) })));
  };
  if (ctx.local || fileCache.has(key)) return load();
  const name = basename(path) || path;
  showPlaceholder(el, alt || name, t('clickToLoad'), async () => {
    showLoading(el);
    let size = -1;
    try {
      const r = await rpc<{ size: number; tooBig?: boolean }>(ctx.hostId, 'fs.read', { path, image: true, strict: true, probe: true });
      size = typeof r.size === 'number' ? r.size : -1;
      if (r.tooBig) {
        showError(el, t('imageTooLargeOver10mb', { v: fmtSize(size), path }));
        return;
      }
    } catch (e) {
      showError(el, t('imageUnavailable', { path, v: errText(e) }));
      return;
    }
    const sz = size >= 0 ? (t('text') + fmtSize(size) + t('text2')) : '';
    const ok = await askConfirm({
      title: t('downloadImage'),
      message: t('downloadImageFrom', { hostName: ctx.hostName, name, sz, path }),
      okText: t('download'),
    });
    if (!el.isConnected) return;
    if (!ok) return fillFile(el, ctx, path, alt);
    load();
  });
}

function fillWeb(el: HTMLElement, url: string, alt: string) {
  const load = () => {
    webAllowed.add(url);
    showLoading(el);
    const probe = new Image();
    probe.referrerPolicy = 'no-referrer';
    probe.onload = () => el.isConnected && showImg(el, url, alt);
    probe.onerror = () => el.isConnected && showError(el, t('imageFailedToLoad'), url);
    probe.src = url;
  };
  if (webAllowed.has(url)) return load();
  let host = url;
  try {
    host = new URL(url).host;
  } catch {
    /* leave as is */
  }
  showPlaceholder(el, alt || t('image'), `${host} · ${t('clickToLoad')}`, load);
}

/** Fill in image placeholders in root that haven't been handled yet. Called after every Markdown redraw (streaming) */
export function hydrateImages(root: HTMLElement, ctx: ImgCtx | null) {
  for (const el of root.querySelectorAll<HTMLElement>('span.md-rimg:not([data-done])')) {
    el.dataset.done = '1';
    const src = el.dataset.src ?? '';
    const alt = el.dataset.alt ?? '';
    if (el.dataset.kind === 'web') fillWeb(el, src, alt);
    else if (!ctx) showError(el, t('image2', { src }));
    else fillFile(el, ctx, filePath(src, ctx.cwd), alt);
  }
}
