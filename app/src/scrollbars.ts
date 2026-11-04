// Scrollbar sliders show like VS Code's (scrollableElement.ts): while the mouse is over a scrollable element, and while it
// scrolls; they fade out shortly after the mouse leaves or the scrolling stops (styles.css: `[data-scrolling]` /
// `[data-scroll-hover]` ::-webkit-scrollbar-thumb).
// Chromium doesn't repaint ::-webkit-scrollbar on :hover, but it does on attribute changes, so the elements are marked
// from JS. Data attributes rather than classes: React rewrites className on render.
const HIDE_AFTER_SCROLL = 800;
/** VS Code's HIDE_TIMEOUT */
const HIDE_AFTER_LEAVE = 500;
const scrollTimers = new WeakMap<Element, number>();
const leaveTimers = new WeakMap<Element, number>();

document.addEventListener(
  'scroll',
  (e) => {
    const el = e.target === document ? document.scrollingElement : e.target;
    if (!(el instanceof Element)) return;
    if (!el.hasAttribute('data-scrolling')) el.setAttribute('data-scrolling', '');
    clearTimeout(scrollTimers.get(el));
    scrollTimers.set(el, window.setTimeout(() => el.removeAttribute('data-scrolling'), HIDE_AFTER_SCROLL));
  },
  { capture: true, passive: true },
);

const SCROLLS = /(auto|scroll)/;
/** Elements under the pointer that can actually scroll; nested ones all count, as each VS Code scrollable reveals on its own */
function scrollablesFrom(target: EventTarget | null): Set<Element> {
  const out = new Set<Element>();
  for (let el = target instanceof Element ? target : null; el && el !== document.documentElement; el = el.parentElement) {
    const v = el.scrollHeight > el.clientHeight;
    const h = el.scrollWidth > el.clientWidth;
    if (!v && !h) continue;
    const s = getComputedStyle(el);
    if ((v && SCROLLS.test(s.overflowY)) || (h && SCROLLS.test(s.overflowX))) out.add(el);
  }
  return out;
}

let hovered = new Set<Element>();
function setHovered(next: Set<Element>) {
  for (const el of next) {
    clearTimeout(leaveTimers.get(el));
    if (!el.hasAttribute('data-scroll-hover')) el.setAttribute('data-scroll-hover', '');
  }
  for (const el of hovered) {
    if (next.has(el)) continue;
    clearTimeout(leaveTimers.get(el));
    leaveTimers.set(el, window.setTimeout(() => el.removeAttribute('data-scroll-hover'), HIDE_AFTER_LEAVE));
  }
  hovered = next;
}

// Mouse only: touch screens keep the system's overlay scrollbars (the styles are scoped to fine pointers too)
document.addEventListener(
  'pointerover',
  (e) => {
    if (e.pointerType === 'mouse') setHovered(scrollablesFrom(e.target));
  },
  { capture: true, passive: true },
);
// Pointer left the window
document.documentElement.addEventListener('pointerleave', () => setHovered(new Set()));
