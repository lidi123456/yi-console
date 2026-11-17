import { isTauri } from './transport';

// In the app, use Tauri's clipboard plugin: the WebView's own clipboard API may show a permission prompt or just fail when reading
export async function readClipboard(trim = true): Promise<string> {
  try {
    const t = isTauri
      ? ((await (await import('@tauri-apps/plugin-clipboard-manager')).readText()) ?? '')
      : ((await navigator.clipboard?.readText()) ?? '');
    return trim ? t.trim() : t;
  } catch {
    return '';
  }
}

export async function writeClipboard(text: string): Promise<boolean> {
  try {
    if (isTauri) await (await import('@tauri-apps/plugin-clipboard-manager')).writeText(text);
    else await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

/** Write both plain text (e.g. GFM Markdown) and rich HTML (e.g. a `<table>`) when supported; falls back to plain text */
export async function writeRichClipboard(text: string, html?: string): Promise<boolean> {
  if (html) {
    try {
      if (isTauri) {
        const mod = await import('@tauri-apps/plugin-clipboard-manager');
        if (typeof mod.writeHtml === 'function') {
          await mod.writeHtml(html, text);
          return true;
        }
      } else if (typeof ClipboardItem !== 'undefined' && navigator.clipboard?.write) {
        await navigator.clipboard.write([
          new ClipboardItem({
            'text/plain': new Blob([text], { type: 'text/plain' }),
            'text/html': new Blob([html], { type: 'text/html' }),
          }),
        ]);
        return true;
      }
    } catch {
      /* rich write unsupported on this platform (e.g. Android clipboard plugin); fall back to plain text */
    }
  }
  return writeClipboard(text);
}
