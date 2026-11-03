import { createRoot } from 'react-dom/client';
import { App } from './App';
import { errorToast, loadClientSettings, refreshHosts, restoreView, startEventLoop } from './store';
import { initTransport } from './transport';
import './styles.css';
import './back';
import './scrollbars';
import { initTheme } from './theme';
import { initLang } from './i18n';
import { isMobileApp, openUrl, openUrlExternal } from './components/PortsView';
import { openMenu } from './components/Overlay';
import { writeClipboard } from './clipboard';
import { tr } from './i18n';
import { startUpdateChecks } from './update';

// Links in the page (AI replies, Markdown preview, etc.) never navigate inside the app window: external URLs open in the system browser,
// anything else (relative paths, file:, etc.) is blocked, otherwise the whole app would be replaced by that page.
// Clicks components already handled (preventDefault, e.g. relative links in the Markdown preview) are left alone; links with download are downloads
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

// Right-click on a link (long press on mobile): open, open in another browser, copy link. Handled before the global edit menu
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
startUpdateChecks();
createRoot(document.getElementById('root')!).render(<App />);

initTransport()
  .then(() => {
    loadClientSettings();
    return refreshHosts();
  })
  .catch(errorToast);
