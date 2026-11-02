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
