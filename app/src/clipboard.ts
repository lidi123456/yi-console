import { isTauri } from './transport';

// App 里走 Tauri 的剪贴板插件：WebView 自带的剪贴板接口读取时可能弹权限框或直接失败
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
