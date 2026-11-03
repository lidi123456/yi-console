import { useEffect, useState } from 'react';
import { lsGet, lsSet } from './util';

// UI language: follow system / Chinese / English, stored in local localStorage.

export type LangPref = 'auto' | 'zh' | 'en';
export type Lang = 'zh' | 'en';

const KEY = 'ui-lang';
const EVENT = 'ah:lang';

export function getLangPref(): LangPref {
  return lsGet<LangPref>(KEY, 'auto');
}

export function getLang(): Lang {
  const p = getLangPref();
  if (p === 'zh' || p === 'en') return p;
  const sys = typeof navigator !== 'undefined' ? navigator.language || '' : 'zh';
  return /^zh/i.test(sys) ? 'zh' : 'en';
}

export function isEn(): boolean {
  return getLang() === 'en';
}

function apply() {
  if (typeof document === 'undefined') return;
  document.documentElement.lang = isEn() ? 'en' : 'zh-CN';
}

export function setLangPref(p: LangPref) {
  lsSet(KEY, p);
  apply();
  window.dispatchEvent(new Event(EVENT));
}

export function initLang() {
  apply();
  if (typeof window !== 'undefined') {
    window.addEventListener('languagechange', () => {
      apply();
      window.dispatchEvent(new Event(EVENT));
    });
  }
}

/** Re-render when the language setting changes; returns [setting, effective language] */
export function useLang(): [LangPref, Lang] {
  const [, bump] = useState(0);
  useEffect(() => {
    const f = () => bump((n) => n + 1);
    window.addEventListener(EVENT, f);
    return () => window.removeEventListener(EVENT, f);
  }, []);
  return [getLangPref(), getLang()];
}

export function onLangChange(f: () => void): () => void {
  window.addEventListener(EVENT, f);
  return () => window.removeEventListener(EVENT, f);
}

/** Bilingual value: returns the Chinese or English text for the current language */
export function tr<T = string>(zh: T, en: T): T {
  return isEn() ? en : zh;
}

const ERR_PATTERNS: [RegExp, string | ((...m: string[]) => string)][] = [
  [/^未知方法[:：]\s*(.+)$/, (_, m) => `Unknown method: ${m}`],
  [/未知方法/, 'Unknown method'],
  [/文件在这期间被修改过，请刷新后再试/, 'File was modified on disk, please reload and try again'],
  [/文件已存在|已经存在/, 'File already exists'],
  [/会话不存在/, 'Session not found'],
  [/主机未连接/, 'Host not connected'],
  [/连接已断开/, 'Connection closed'],
  [/无法连接 Yi Console 服务/, 'Cannot connect to Yi Console service'],
  [/认证失败/, 'Authentication failed'],
  [/权限不足|拒绝访问/, 'Permission denied'],
  [/超时/, 'Timed out'],
  [/这个会话没有在等待额度恢复/, 'This session is not waiting for a usage limit to reset'],
  [/^自动继续失败[:：]\s*(.+)$/, (_, m) => `Automatic continue failed: ${m}`],
  [/^未知语言[:：]\s*(.+)$/, (_, m) => `Unknown language: ${m}`],
  [/^未知操作[:：]\s*(.+)$/, (_, m) => `Unknown action: ${m}`],
  // App self-update (updater.rs / MainActivity.kt)
  [/^检查更新失败[:：]\s*(.+)$/, (_, m) => `Update check failed: ${m}`],
  [/^安装更新失败[:：]\s*(.+)$/, (_, m) => `Update install failed: ${m}`],
  [/^下载失败[:：]\s*(.+)$/, (_, m) => `Download failed: ${m}`],
  [/^无法打开安装程序[:：]\s*(.+)$/, (_, m) => `Cannot open the installer: ${m}`],
  [/正在安装更新/, 'An update is being installed'],
  [/没有可安装的更新|请先检查更新/, 'No update to install, check for updates first'],
  [/没有找到发布的版本/, 'No published release found'],
  [/下载的安装包校验失败/, 'The downloaded package failed verification, please try again'],
  [/已经是最新版本/, 'Already up to date'],
  [/这种安装方式不支持自动更新/, 'This kind of install cannot update itself; download the new version from the releases page'],
];

/** Translate Chinese error messages from the backend or runtime (applies in the English UI) */
export function trErr(msg: string): string {
  if (!isEn() || !msg) return msg;
  for (const [re, rep] of ERR_PATTERNS) {
    if (re.test(msg)) {
      return typeof rep === 'function' ? msg.replace(re, rep) : msg.replace(re, rep);
    }
  }
  return msg;
}
