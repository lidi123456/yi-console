import { useEffect, useState } from 'react';
import { lsGet, lsSet } from './util';

// UI theme: follow system / light / dark, stored in local localStorage.
// Choosing light or dark sets data-theme on <html>, and styles.css uses it to override the system prefers-color-scheme

export type Theme = 'auto' | 'light' | 'dark';

const KEY = 'ui-theme';
const EVENT = 'ah:theme';
const mq = () => window.matchMedia('(prefers-color-scheme: dark)');

export function getTheme(): Theme {
  return lsGet<Theme>(KEY, 'auto');
}

/** Whether the UI is actually dark right now */
export function isAppDark(): boolean {
  const t = getTheme();
  return t === 'dark' || (t === 'auto' && mq().matches);
}

function apply() {
  const t = getTheme();
  const root = document.documentElement;
  if (t === 'auto') delete root.dataset.theme;
  else root.dataset.theme = t;
  // Also recolor things like the mobile browser's address bar
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

/** Call once at startup (before rendering) to avoid a flash of the system color scheme */
export function initTheme() {
  apply();
  mq().addEventListener('change', () => window.dispatchEvent(new Event(EVENT)));
}

/** Re-render when the theme setting or system color scheme changes; returns [setting, whether actually dark] */
export function useTheme(): [Theme, boolean] {
  const [, bump] = useState(0);
  useEffect(() => {
    const f = () => bump((n) => n + 1);
    window.addEventListener(EVENT, f);
    return () => window.removeEventListener(EVENT, f);
  }, []);
  return [getTheme(), isAppDark()];
}

/** Subscribe to theme changes from outside React (xterm instances) */
export function onThemeChange(f: () => void): () => void {
  window.addEventListener(EVENT, f);
  return () => window.removeEventListener(EVENT, f);
}
