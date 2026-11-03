// Scrollbar sliders show only while an element is scrolling (styles.css: `[data-scrolling]::-webkit-scrollbar-thumb`).
// Chromium doesn't repaint ::-webkit-scrollbar on :hover, but it does on attribute changes, so mark the scrolling element
// and clear the mark shortly after the last scroll event. A data attribute rather than a class: React rewrites className on render.
const HIDE_AFTER = 800;
const timers = new WeakMap<Element, number>();

document.addEventListener(
  'scroll',
  (e) => {
    const el = e.target === document ? document.scrollingElement : e.target;
    if (!(el instanceof Element)) return;
    if (!el.hasAttribute('data-scrolling')) el.setAttribute('data-scrolling', '');
    clearTimeout(timers.get(el));
    timers.set(el, window.setTimeout(() => el.removeAttribute('data-scrolling'), HIDE_AFTER));
  },
  { capture: true, passive: true },
);
