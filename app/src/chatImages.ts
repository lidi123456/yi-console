// Images in chat replies: renderMarkdown renders ![](…) as <span class="md-rimg">, and this fills in the content based on the source.
// - Files on the host: the local host reads and shows them directly; remote hosts (SSH / relay / forwarded) show a placeholder first, and on click ask for the size, then download
// - Public URLs: always show a placeholder first; the current device only loads them on click (opening a chat never requests third parties automatically)
// Images that were opened / confirmed are remembered for this run and shown directly when they reappear (streaming redraws, switching away and back).
import { tr } from './i18n';
import { rpc } from './transport';
import { askConfirm } from './components/Overlay';
import { resolvePath } from './components/FilePreview';
import { basename, fmtSize } from './util';

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
    a.title = tr('用浏览器打开', 'Open in browser');
    box.append(' ', a);
  }
  el.replaceChildren(box);
}

function showLoading(el: HTMLElement) {
  el.className = 'md-rimg loading';
  const s = document.createElement('span');
  s.className = 'md-rimg-ph';
  s.textContent = tr('正在加载图片…', 'Loading image…');
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
      .catch((e) => el.isConnected && showError(el, tr(`图片不可用：${path}（${errText(e)}）`, `Image unavailable: ${path} (${errText(e)})`)));
  };
  if (ctx.local || fileCache.has(key)) return load();
  const name = basename(path) || path;
  showPlaceholder(el, alt || name, tr('点击加载', 'Click to load'), async () => {
    showLoading(el);
    let size = -1;
    try {
      const r = await rpc<{ size: number; tooBig?: boolean }>(ctx.hostId, 'fs.read', { path, image: true, strict: true, probe: true });
      size = typeof r.size === 'number' ? r.size : -1;
      if (r.tooBig) {
        showError(el, tr(`图片太大（${fmtSize(size)}），超过 10MB 不预览：${path}`, `Image too large (${fmtSize(size)}), over 10MB: ${path}`));
        return;
      }
    } catch (e) {
      showError(el, tr(`图片不可用：${path}（${errText(e)}）`, `Image unavailable: ${path} (${errText(e)})`));
      return;
    }
    const sz = size >= 0 ? (tr('（', ' (') + fmtSize(size) + tr('）', ')')) : '';
    const ok = await askConfirm({
      title: tr('下载图片', 'Download image'),
      message: tr(`从「${ctx.hostName}」下载图片 ${name}${sz}？\n${path}`, `Download image ${name}${sz} from "${ctx.hostName}"?\n${path}`),
      okText: tr('下载', 'Download'),
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
    probe.onload = () => el.isConnected && showImg(el, url, alt);
    probe.onerror = () => el.isConnected && showError(el, tr('图片加载失败', 'Image failed to load'), url);
    probe.src = url;
  };
  if (webAllowed.has(url)) return load();
  let host = url;
  try {
    host = new URL(url).host;
  } catch {
    /* leave as is */
  }
  showPlaceholder(el, alt || tr('图片', 'Image'), `${host} · ${tr('点击加载', 'Click to load')}`, load);
}

/** Fill in image placeholders in root that haven't been handled yet. Called after every Markdown redraw (streaming) */
export function hydrateImages(root: HTMLElement, ctx: ImgCtx | null) {
  for (const el of root.querySelectorAll<HTMLElement>('span.md-rimg:not([data-done])')) {
    el.dataset.done = '1';
    const src = el.dataset.src ?? '';
    const alt = el.dataset.alt ?? '';
    if (el.dataset.kind === 'web') fillWeb(el, src, alt);
    else if (!ctx) showError(el, tr(`图片：${src}`, `Image: ${src}`));
    else fillFile(el, ctx, filePath(src, ctx.cwd), alt);
  }
}
