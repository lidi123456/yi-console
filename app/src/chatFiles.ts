// File links in chat replies: renderMarkdown renders [name](/path/file.rs:12) as <a class="md-file" data-path data-line> (no href),
// this adds the file icon and opens the file in the session's file panel on click / Enter.
import type { ImgCtx } from './chatImages';
import { filePath } from './chatImages';
import { fileGlyph } from './components/FilePanel';
import { openFileInPanel } from './components/SessionActions';
import { basename } from './util';

const SVG = 'http://www.w3.org/2000/svg';

function icon(name: string) {
  const [d, color] = fileGlyph(name);
  const svg = document.createElementNS(SVG, 'svg');
  for (const [k, v] of Object.entries({ width: '14', height: '14', viewBox: '0 0 16 16', fill: 'none', stroke: color, 'stroke-width': '1.4', 'stroke-linecap': 'round', 'stroke-linejoin': 'round', 'aria-hidden': 'true' }))
    svg.setAttribute(k, v);
  const path = document.createElementNS(SVG, 'path');
  path.setAttribute('d', d);
  svg.append(path);
  return svg;
}

/** Fill in icons of file links in root not handled yet; without a session to open them in they stay plain chips */
export function hydrateFileLinks(root: HTMLElement, ctx: ImgCtx | null) {
  for (const el of root.querySelectorAll<HTMLElement>('a.md-file:not([data-done])')) {
    el.dataset.done = '1';
    el.querySelector('.md-file-icon')?.replaceChildren(icon(basename(el.dataset.path ?? '')));
    if (!ctx?.sessionId) {
      el.classList.add('static');
      el.removeAttribute('tabindex');
    }
  }
}

/** Click / keyboard handler for the Markdown container: true when it was a file link */
export function openFileLink(e: { target: EventTarget | null; preventDefault(): void; stopPropagation(): void }, ctx: ImgCtx | null) {
  const el = (e.target as Element | null)?.closest?.<HTMLElement>('a.md-file');
  if (!el) return false;
  e.preventDefault();
  e.stopPropagation();
  const raw = el.dataset.path;
  if (!raw || !ctx?.sessionId) return true;
  const line = Number(el.dataset.line) || undefined;
  openFileInPanel(ctx.hostId, ctx.sessionId, filePath(raw, ctx.cwd), line);
  return true;
}
