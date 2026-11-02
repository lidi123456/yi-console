import { createRoot } from 'react-dom/client';
import { App } from './App';
import { errorToast, loadClientSettings, refreshHosts, restoreView, startEventLoop } from './store';
import { initTransport } from './transport';
import './styles.css';
import './back';
import { initTheme } from './theme';
import { initLang } from './i18n';
import { isMobileApp, openUrl, openUrlExternal } from './components/PortsView';
import { openMenu } from './components/Overlay';
import { writeClipboard } from './clipboard';
import { tr } from './i18n';

// 页面里的链接（AI 回复、Markdown 预览等）一律不在应用窗口里跳转：外部网址用系统浏览器打开，
// 其他（相对路径、file: 等）直接拦下，否则整个应用会被换成那个页面。
// 组件自己处理了的点击（preventDefault 过，比如 Markdown 预览里的相对链接）不管；带 download 的是下载
function guardLink(e: MouseEvent) {
  if (e.defaultPrevented || (e.type === 'auxclick' && e.button !== 1)) return;
  const a = (e.target as Element | null)?.closest?.('a[href]') as HTMLAnchorElement | null;
  if (!a || a.hasAttribute('download')) return;
  const href = a.getAttribute('href') ?? '';
  if (href.startsWith('#')) return;
  e.preventDefault();
  if (/^(https?|mailto):/i.test(a.href)) openUrl(a.href);
}
document.addEventListener('click', guardLink);
document.addEventListener('auxclick', guardLink);

// 链接上右键（手机上是长按）：打开、换别的浏览器打开、复制链接。先于全局的编辑菜单处理
function linkMenu(e: MouseEvent) {
  if (e.defaultPrevented) return;
  const a = (e.target as Element | null)?.closest?.('a[href]') as HTMLAnchorElement | null;
  if (!a || a.hasAttribute('download') || !/^https?:/i.test(a.href)) return;
  const url = a.href;
  const sel = window.getSelection()?.toString() ?? '';
  openMenu(e, [
    { label: isMobileApp ? tr('在应用内打开', 'Open in app') : tr('在浏览器中打开', 'Open in browser'), icon: 'globe', run: () => openUrl(url) },
    ...(isMobileApp ? [{ label: tr('用其他浏览器打开', 'Open in another browser'), icon: 'external', run: () => openUrlExternal(url) }] : []),
    { label: tr('复制链接', 'Copy link'), icon: 'copy', run: () => writeClipboard(url) },
    ...(sel.trim() ? ['-' as const, { label: tr('复制', 'Copy'), icon: 'copy', run: () => writeClipboard(sel) }] : []),
  ]);
}
document.addEventListener('contextmenu', linkMenu, true);

initTheme();
initLang();
startEventLoop();
restoreView();
createRoot(document.getElementById('root')!).render(<App />);

initTransport()
  .then(() => {
    loadClientSettings();
    return refreshHosts();
  })
  .catch(errorToast);
