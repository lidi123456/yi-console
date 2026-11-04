import { useEffect, useState } from 'react';
import { isTauri } from './transport';
import { isMobileApp } from './components/PortsView';
import { lsGet, lsSet } from './util';

// Font sizes and UI zoom, per device (localStorage `ui-font`), like VS Code's separate chat.fontSize / editor.fontSize
// plus window zoom. A size of 0 means the stylesheet default (chat text is a little larger on phones).
// Chat size drives the reply text, headings, user bubbles and input box (--fs-body / --fs-chat / --fs-input) and
// --fs-code (code blocks in replies); editor size drives --fs-editor (CodeMirror, diffs, Markdown preview).
// Font families (chat.fontFamily / editor.fontFamily / terminal.integrated.fontFamily in VS Code) are comma lists the
// user types; they go in front of the built-in stacks, which stay as fallbacks, so an unknown or missing font only
// falls back to today's look. Empty means the default. Chat font: --font-chat; editor font: --font-editor (CodeMirror,
// diffs, code in replies and the Markdown preview); terminal font: xterm's fontFamily.
// Zoom is the desktop app's page zoom.

export type FontPrefs = {
  chat: number;
  editor: number;
  zoom: number;
  chatFont: string;
  editorFont: string;
  termFont: string;
};

const KEY = 'ui-font';
const EVENT = 'ah:font';
export const CHAT_SIZES = [12, 13, 14, 15, 16, 17, 18, 20];
export const EDITOR_SIZES = [10, 11, 12, 13, 14, 15, 16, 18, 20];
/** VS Code's zoom levels: each step is 1.2x */
export const ZOOM_MIN = -3;
export const ZOOM_MAX = 6;
/** Zoom is the desktop app's own; the browser (yi-web) has its own Ctrl+= and phones pinch */
export const canZoom = isTauri && !isMobileApp;

export function getFontPrefs(): FontPrefs {
  return { chat: 0, editor: 0, zoom: 0, chatFont: '', editorFont: '', termFont: '', ...lsGet<Partial<FontPrefs>>(KEY, {}) };
}

const GENERIC = new Set(['serif', 'sans-serif', 'monospace', 'cursive', 'fantasy', 'system-ui', 'ui-monospace', 'ui-sans-serif', 'ui-serif']);

/** "JetBrains Mono, 'Sarasa Mono SC'" -> `"JetBrains Mono", "Sarasa Mono SC"`; names are quoted so nothing else in the
 * input can reach the CSS value. Empty input gives "". */
export function fontList(input: string): string {
  return input
    .split(',')
    .map((f) => f.trim().replace(/^["']|["']$/g, '').trim())
    .filter(Boolean)
    .map((f) => (GENERIC.has(f.toLowerCase()) ? f.toLowerCase() : `"${f.replace(/["\\]/g, '')}"`))
    .join(', ');
}

// Like Windows Terminal, prefer Cascadia Mono, with CJK falling back to YaHei / PingFang
const TERM_FONT = '"Cascadia Mono", "Cascadia Code", Consolas, "SF Mono", Menlo, "JetBrains Mono", "Microsoft YaHei UI", "PingFang SC", monospace';

/** xterm's fontFamily: the user's fonts, then the built-in stack */
export function termFontFamily(): string {
  const u = fontList(getFontPrefs().termFont);
  return u ? `${u}, ${TERM_FONT}` : TERM_FONT;
}

export const zoomFactor = (level: number) => Math.pow(1.2, level);

function apply() {
  const p = getFontPrefs();
  const s = document.documentElement.style;
  // --fs-body: classic reply style; --fs-chat: minimal style, headings and user bubbles; --fs-input: the input box
  const chatVars = ['--fs-body', '--fs-chat', '--fs-input'];
  if (p.chat) {
    for (const v of chatVars) s.setProperty(v, `${p.chat}px`);
    // Code in replies stays two sizes smaller than the text, as at the default 14 / 12
    s.setProperty('--fs-code', `${Math.max(10, p.chat - 2)}px`);
  } else {
    for (const v of [...chatVars, '--fs-code']) s.removeProperty(v);
  }
  // Whole-pixel line height at the default 12 / 20 ratio; fractional heights blur CodeMirror's lines
  if (p.editor) {
    s.setProperty('--fs-editor', `${p.editor}px`);
    s.setProperty('--lh-editor', `${Math.round((p.editor * 5) / 3)}px`);
  } else {
    s.removeProperty('--fs-editor');
    s.removeProperty('--lh-editor');
  }
  for (const [v, list, base] of [
    ['--font-chat', p.chatFont, 'var(--font)'],
    ['--font-editor', p.editorFont, 'var(--mono)'],
  ] as const) {
    const u = fontList(list);
    if (u) s.setProperty(v, `${u}, ${base}`);
    else s.removeProperty(v);
  }
  if (canZoom) {
    import('@tauri-apps/api/webview')
      .then((m) => m.getCurrentWebview().setZoom(zoomFactor(p.zoom)))
      .catch(() => {});
  }
}

export function setFontPrefs(p: Partial<FontPrefs>) {
  const next = { ...getFontPrefs(), ...p };
  next.zoom = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, next.zoom));
  lsSet(KEY, next);
  apply();
  window.dispatchEvent(new Event(EVENT));
  // CodeMirror measures line heights on window resize; a CSS variable change alone doesn't tell it
  window.dispatchEvent(new Event('resize'));
}

/** Call once at startup, before rendering */
export function initFontPrefs() {
  apply();
  if (!canZoom) return;
  // Ctrl/Cmd + = / - / 0, like VS Code's zoom in / out / reset. Capture phase, so the terminal and editor don't get them first
  window.addEventListener(
    'keydown',
    (e) => {
      if (!(e.ctrlKey || e.metaKey) || e.altKey) return;
      const z = getFontPrefs().zoom;
      let next: number;
      if (e.code === 'Equal' || e.code === 'NumpadAdd' || e.key === '+' || e.key === '=') next = z + 1;
      else if (e.code === 'Minus' || e.code === 'NumpadSubtract' || e.key === '-') next = z - 1;
      else if (e.code === 'Digit0' || e.code === 'Numpad0') next = 0;
      else return;
      e.preventDefault();
      e.stopPropagation();
      setFontPrefs({ zoom: next });
    },
    true,
  );
}

export function useFontPrefs(): FontPrefs {
  const [, bump] = useState(0);
  useEffect(() => {
    const f = () => bump((n) => n + 1);
    window.addEventListener(EVENT, f);
    return () => window.removeEventListener(EVENT, f);
  }, []);
  return getFontPrefs();
}
