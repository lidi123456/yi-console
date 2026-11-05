import DOMPurify from 'dompurify';

export type Diagram = { svg: string; width: number; height: number; background: string };
const cache = new Map<string, Promise<Diagram>>();
let queue: Promise<unknown> = Promise.resolve();
let serial = 0;

/** Sources are untrusted. Keep configuration and active/resource-bearing syntax out of the renderer. */
export function validateDiagram(source: string) {
  if (source.length > 30000) throw new Error('limit');
  if (
    !/^\s*(?:flowchart|graph|sequenceDiagram|stateDiagram(?:-v2)?|classDiagram|erDiagram|gantt|pie|mindmap|timeline|gitGraph)\b/.test(
      source,
    )
  )
    throw new Error('unsupported');
  if (
    /%%\s*\{|^\s*---|(?:https?|file|data|javascript):|(?:src|href)\s*=|<\s*\/?[a-z]|\b(?:image|icon|img)\s*:|@\{|(?:^|[;\n])\s*(?:click|style|classDef|linkStyle)\b/im.test(
      source,
    )
  )
    throw new Error('unsafe');
}

export function renderDiagram(source: string, dark: boolean, font: string, fontSize: number): Promise<Diagram> {
  const key = JSON.stringify([source, dark, font, fontSize]);
  const existing = cache.get(key);
  if (existing) return existing;
  const task = queue.then(async () => {
    validateDiagram(source);
    const { default: mermaid } = await import('mermaid');
    mermaid.initialize({
      startOnLoad: false,
      securityLevel: 'strict',
      suppressErrorRendering: true,
      maxTextSize: 30000,
      maxEdges: 300,
      theme: dark ? 'dark' : 'default',
      fontFamily: font,
      themeVariables: { fontFamily: font, fontSize: `${fontSize}px` },
      htmlLabels: false,
      flowchart: { htmlLabels: false },
      secure: [
        'secure',
        'securityLevel',
        'startOnLoad',
        'maxTextSize',
        'maxEdges',
        'suppressErrorRendering',
        'htmlLabels',
        'theme',
        'themeVariables',
        'fontFamily',
      ],
    });
    const host = document.createElement('div');
    host.style.cssText = 'position:fixed;left:-100000px;top:0;visibility:hidden;pointer-events:none';
    document.body.append(host);
    try {
      const { svg } = await mermaid.render(`yiDiagram${++serial}`, source, host);
      const clean = DOMPurify.sanitize(svg, {
        USE_PROFILES: { svg: true, svgFilters: true },
        FORBID_TAGS: ['foreignObject', 'image', 'a', 'script', 'animate', 'set'],
        FORBID_ATTR: ['href', 'xlink:href'],
      });
      const doc = new DOMParser().parseFromString(clean, 'image/svg+xml');
      const root = doc.documentElement;
      const viewBox = root
        .getAttribute('viewBox')
        ?.split(/[\s,]+/)
        .map(Number);
      if (
        root.localName !== 'svg' ||
        !viewBox ||
        viewBox.length !== 4 ||
        !viewBox.every(Number.isFinite) ||
        viewBox[2] <= 0 ||
        viewBox[3] <= 0
      )
        throw new Error('svg');
      const width = Math.ceil(viewBox[2]),
        height = Math.ceil(viewBox[3]);
      const background = dark ? '#1e1e1e' : '#ffffff';
      root.setAttribute('xmlns', 'http://www.w3.org/2000/svg');
      root.setAttribute('width', String(width));
      root.setAttribute('height', String(height));
      root.style.maxWidth = 'none';
      root.style.backgroundColor = background;
      return { svg: new XMLSerializer().serializeToString(root), width, height, background };
    } finally {
      host.remove();
    }
  });
  queue = task.catch(() => {});
  cache.set(key, task);
  // Bound memory as well as count; an individual source is capped before parsing.
  if (cache.size > 32) cache.delete(cache.keys().next().value!);
  void task.catch(() => cache.delete(key));
  return task;
}

export async function diagramPng(diagram: Diagram): Promise<Blob> {
  const scale = Math.min(2, 8192 / diagram.width, 8192 / diagram.height, Math.sqrt(16000000 / (diagram.width * diagram.height)));
  const canvas = document.createElement('canvas');
  canvas.width = Math.max(1, Math.floor(diagram.width * scale));
  canvas.height = Math.max(1, Math.floor(diagram.height * scale));
  const url = URL.createObjectURL(new Blob([diagram.svg], { type: 'image/svg+xml' }));
  try {
    const img = new Image();
    await new Promise<void>((resolve, reject) => {
      img.onload = () => resolve();
      img.onerror = reject;
      img.src = url;
    });
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('canvas');
    ctx.fillStyle = diagram.background;
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    ctx.drawImage(img, 0, 0, canvas.width, canvas.height);
    return await new Promise<Blob>((resolve, reject) =>
      canvas.toBlob((blob) => (blob ? resolve(blob) : reject(new Error('png'))), 'image/png'),
    );
  } finally {
    URL.revokeObjectURL(url);
  }
}
