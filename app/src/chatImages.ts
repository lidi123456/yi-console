// 对话回复里的图片：renderMarkdown 把 ![](…) 渲染成 <span class="md-rimg">，这里按来源填内容。
// - 主机上的文件：本机主机直接读出来显示；远程主机（SSH / 中继 / 转发）先显示占位，点了问一下大小再下载
// - 公网链接：一律先显示占位，点了才由当前设备去加载（打开对话不会自动请求第三方）
// 点开过 / 确认过的图片在本次运行里记住，再出现（流式输出重绘、切走再回来）直接显示。
import { tr } from './i18n';
import { rpc } from './transport';
import { askConfirm } from './components/Overlay';
import { resolvePath } from './components/FilePreview';
import { basename, fmtSize } from './util';

export interface ImgCtx {
  hostId: string;
  hostName: string;
  /** 本机主机：主机上的图片直接加载 */
  local: boolean;
  /** 会话工作目录，相对路径按它解析 */
  cwd?: string;
}

/** 主机文件：hostId + 路径 -> data URL */
const fileCache = new Map<string, Promise<string>>();
/** 用户点过加载的公网地址 */
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

/** 占位按钮：图标 + 名称 + 来源，点了执行 onClick */
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
    // 图片外面可能套着链接
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
    /* 留原样 */
  }
  showPlaceholder(el, alt || tr('图片', 'Image'), `${host} · ${tr('点击加载', 'Click to load')}`, load);
}

/** 填充 root 里还没处理过的图片占位。Markdown 每次重绘（流式输出）后调用 */
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
