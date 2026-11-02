import { useEffect, useState } from 'react';
import { lsGet, lsSet } from './util';

// 界面主题：跟随系统 / 亮色 / 暗色，存在本机 localStorage。
// 选了亮色或暗色就在 <html> 上写 data-theme，styles.css 按它覆盖系统的 prefers-color-scheme

export type Theme = 'auto' | 'light' | 'dark';

const KEY = 'ui-theme';
const EVENT = 'ah:theme';
const mq = () => window.matchMedia('(prefers-color-scheme: dark)');

export function getTheme(): Theme {
  return lsGet<Theme>(KEY, 'auto');
}

/** 当前界面实际是不是暗色 */
export function isAppDark(): boolean {
  const t = getTheme();
  return t === 'dark' || (t === 'auto' && mq().matches);
}

function apply() {
  const t = getTheme();
  const root = document.documentElement;
  if (t === 'auto') delete root.dataset.theme;
  else root.dataset.theme = t;
  // 手机浏览器地址栏等跟着变色
  for (const m of document.querySelectorAll<HTMLMetaElement>('meta[name="theme-color"]')) {
    if (t === 'auto') m.media = m.dataset.media ?? m.media;
    else {
      m.dataset.media ??= m.media;
      m.media = (m.dataset.media ?? '').includes(t) ? 'all' : 'not all';
    }
  }
}

export function setTheme(t: Theme) {
  lsSet(KEY, t);
  apply();
  window.dispatchEvent(new Event(EVENT));
}

/** 启动时（渲染前）调一次，避免先闪一下系统配色 */
export function initTheme() {
  apply();
  mq().addEventListener('change', () => window.dispatchEvent(new Event(EVENT)));
}

/** 主题设置或系统配色变了就重新渲染；返回 [设置值, 实际是否暗色] */
export function useTheme(): [Theme, boolean] {
  const [, bump] = useState(0);
  useEffect(() => {
    const f = () => bump((n) => n + 1);
    window.addEventListener(EVENT, f);
    return () => window.removeEventListener(EVENT, f);
  }, []);
  return [getTheme(), isAppDark()];
}

/** 不在 React 里的地方（xterm 实例）订阅主题变化 */
export function onThemeChange(f: () => void): () => void {
  window.addEventListener(EVENT, f);
  return () => window.removeEventListener(EVENT, f);
}
