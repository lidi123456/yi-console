// Mobile back key / back gesture: Android's MainActivity intercepts it and calls window.__yiConsoleBack(),
// which returns true if it was handled here; on false the app exits.
// Order: first close the topmost open thing (dialog, menu, image viewer, file panel, etc.; last opened closes first), then the command palette;
// if the left navigation is open, only collapse it; finally go back to the previous page.
import { useEffect, useRef } from 'react';
import { tr } from './i18n';
import { getState, goBack, setState, toast } from './store';

const stack: (() => void)[] = [];

/** While open, register "how to close me on back"; nothing is registered when active is false */
export function useBack(active: boolean, close: () => void) {
  const ref = useRef(close);
  ref.current = close;
  useEffect(() => {
    if (!active) return;
    const fn = () => ref.current();
    stack.push(fn);
    return () => {
      const i = stack.lastIndexOf(fn);
      if (i >= 0) stack.splice(i, 1);
    };
  }, [active]);
}

export function handleBack(): boolean {
  const top = stack.pop();
  if (top) {
    top();
    return true;
  }
  const s = getState();
  // Command palette and the left navigation on mobile: just collapse, stay on the current page
  if (s.palette || s.sidebarOpen) {
    setState({ palette: false, sidebarOpen: false });
    return true;
  }
  if (s.canBack) {
    goBack();
    return true;
  }
  // Nowhere left to go back to: require a second press within 2 s to exit, to avoid accidental exits
  const now = Date.now();
  if (now - lastTry < 2000) return false;
  lastTry = now;
  toast(tr('再按一次退出', 'Press back again to exit'));
  return true;
}

let lastTry = 0;

(window as unknown as { __yiConsoleBack?: () => boolean }).__yiConsoleBack = handleBack;
