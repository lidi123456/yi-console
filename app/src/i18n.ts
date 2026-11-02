import { useEffect, useState } from 'react';
import { lsGet, lsSet } from './util';

// 界面语言：跟随系统 / 中文 / English，存在本机 localStorage。

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

/** 语言设置变化时重新渲染；返回 [设置值, 实际语言] */
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

/** 双语取值：根据当前语言返回中文或英文 */
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
];

/** 翻译后端或运行时的中文错误消息（英文界面下生效） */
export function trErr(msg: string): string {
  if (!isEn() || !msg) return msg;
  for (const [re, rep] of ERR_PATTERNS) {
    if (re.test(msg)) {
      return typeof rep === 'function' ? msg.replace(re, rep) : msg.replace(re, rep);
    }
  }
  return msg;
}
