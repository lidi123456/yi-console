// Run via windows-remote pw-run.sh while Vite listens on port 3001.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
await fs.mkdir('out', { recursive: true });
const browser = await chromium.launch({ channel: 'chrome', headless: true });
const context = await browser.newContext({ acceptDownloads: true, permissions: ['clipboard-read', 'clipboard-write'] });
const page = await context.newPage();
const errors = [];
page.on('pageerror', (e) => errors.push(e.message));
await page.goto('http://127.0.0.1:3001/tests/diagrams.html');
await page.waitForFunction(() => window.setMessage);
const source = 'flowchart TD\n A[开始] --> B{成功?}\n B -->|是| C[完成]\n B -->|否| A';
const fence = '```mermaid\n' + source;
const set = (text, streaming = true) => page.evaluate((v) => window.setMessage(v), { text, streaming });
await set(fence);
await page.getByText('Generating diagram…').waitFor();
assert.equal(await page.locator('.mermaid-scroll img').count(), 0);
assert.equal(await page.locator('.mermaid-block pre').count(), 0, 'half-written source stays hidden while streaming');
await page.getByRole('button', { name: 'Show source', exact: true }).click();
assert.equal(await page.locator('.mermaid-block pre').count(), 1, 'the Source tab still shows streaming code');
await page.getByRole('button', { name: 'Show diagram', exact: true }).click();
await set(fence + '\n```\n');
await page.locator('.mermaid-scroll img').waitFor();
await page.waitForFunction(() => document.querySelector('.mermaid-scroll img')?.naturalWidth > 0);
const image = await page.locator('.mermaid-scroll img').getAttribute('src');
assert.ok((await page.locator('.mermaid-toolbar-inline').boundingBox()).height <= 34, 'desktop header should stay compact');
await page.getByRole('button', { name: 'Show source', exact: true }).click();
await set(fence + '\n```\n\nMore streaming text');
assert.equal(await page.getByRole('button', { name: 'Show source', exact: true }).getAttribute('aria-pressed'), 'true');
await page.getByRole('button', { name: 'Show diagram', exact: true }).click();
assert.equal(await page.locator('.mermaid-scroll img').getAttribute('src'), image, 'completed diagram should keep its blob URL');
await page.getByRole('button', { name: 'Copy source', exact: true }).click();
assert.equal(await page.evaluate(async () => (await navigator.clipboard.readText()).replace(/\r\n/g, '\n')), source);
for (const format of ['SVG', 'PNG']) {
  const download = page.waitForEvent('download');
  await page.getByRole('button', { name: 'Export image', exact: true }).click();
  await page.getByRole('button', { name: `Export ${format}`, exact: true }).click();
  const file = await download;
  await file.saveAs(`out/diagram.${format.toLowerCase()}`);
  const bytes = await fs.readFile(`out/diagram.${format.toLowerCase()}`);
  assert.ok(bytes.length > 100);
  if (format === 'PNG') assert.equal(bytes.subarray(1, 4).toString(), 'PNG');
  else assert.match(bytes.toString(), /<svg/);
}
await page.getByRole('button', { name: 'View fullscreen', exact: true }).click();
await page.getByRole('dialog').waitFor();
await page.getByRole('button', { name: 'Zoom in', exact: true }).click();
await page.mouse.move(500, 400);
await page.mouse.down();
await page.mouse.move(580, 440);
await page.mouse.up();
await page.screenshot({ path: 'out/fullscreen.png' });
await page.keyboard.press('Escape');
assert.equal(await page.getByRole('dialog').count(), 0);
await page.evaluate(() => window.setTheme('dark'));
await page.waitForFunction(
  (old) =>
    document.querySelector('.mermaid-scroll img')?.getAttribute('src') !== old &&
    document.querySelector('.mermaid-scroll img')?.naturalWidth > 0,
  image,
);
await page.screenshot({ path: 'out/dark.png' });
await page.setViewportSize({ width: 390, height: 844 });
await page.screenshot({ path: 'out/mobile.png' });
assert.ok((await page.locator('.mermaid-toolbar-inline').boundingBox()).height <= 42, 'mobile header should stay on one line');
await page.getByRole('button', { name: 'Export image', exact: true }).click();
await page.getByRole('button', { name: 'Export SVG', exact: true }).waitFor();
assert.equal(await page.evaluate(() => window.__yiConsoleBack()), true);
assert.equal(await page.locator('.ctx-menu').count(), 0);
assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= 390));
await set('```mermaid\nflowchart TD\nA -->\n```', false);
await page.getByText('Could not render the diagram. Check the source syntax.').waitFor();
await set('```mermaid\nflowchart TD\nA --> B\nclick A "https://example.com"\n```', false);
await page.getByText(/unsupported syntax/).waitFor();
await set(fence, false);
await page.getByText('The diagram code fence is unclosed. Source preserved.').waitFor();
await set(
  '> ```mermaid\n> flowchart LR\n> A --> B\n> ```\n\n- item\n\n  ~~~mermaid\n  sequenceDiagram\n  Alice->>Bob: Hello\n  ~~~',
  false,
);
await page.waitForFunction(() => [...document.querySelectorAll('.mermaid-scroll img')].filter((i) => i.naturalWidth > 0).length === 2);
assert.equal(await page.locator('blockquote .mermaid-block').count(), 1);
await page.evaluate(async () => {
  const cases = [
    'flowchart LR\n A-->B',
    'sequenceDiagram\n A->>B: Hello',
    'stateDiagram-v2\n [*] --> Active',
    'classDiagram\n Animal <|-- Duck',
    'erDiagram\n CUSTOMER ||--o{ ORDER : places',
    'gantt\n title Work\n section Phase\n Task :a, 2026-01-01, 1d',
    'pie\n "A" : 1',
    'mindmap\n root((Root))\n  Child',
    'timeline\n 2026 : Start',
    'gitGraph\n commit',
    'flowchart TB\r\n subgraph L1["用户层"]\r\n  U1["Web"]\r\n end\r\n U1 -. 调用 .-> S1\r\n classDef user fill:#E8F1FF,stroke:#4A78C2,color:#1F3B6D\r\n class U1 user\r\n style S1 fill:#FFF4E5,stroke-width:2px,stroke-dasharray: 5 5\r\n linkStyle 0 stroke:#888',
    'stateDiagram-v2\n [*] --> A\n classDef hot fill:#f00,font-weight:bold\n class A hot',
  ];
  for (const source of cases) {
    const d = await window.renderDiagram(source, false, 'system-ui', 14);
    const doc = new DOMParser().parseFromString(d.svg, 'image/svg+xml');
    if (!doc.querySelector('text, tspan')) throw new Error('Missing text labels: ' + source);
    const blob = await window.diagramPng(d);
    if (!blob.size) throw new Error('Empty PNG: ' + source);
  }
  for (const bad of [
    '---\nconfig: {}\n---\nflowchart LR\nA-->B',
    'flowchart LR\nA@{img: "https://example.com/x"}',
    'flowchart LR\nclassDef x fill:url(#p)',
    'flowchart LR\nclassDef x fill:red,background-image:none',
    'flowchart LR\nstyle A fill:red}svg{display:none',
    'flowchart LR\nstyle A fill:"red"',
    'flowchart LR\nlinkStyle 0 stroke:red !important',
    'flowchart LR\nstyle',
    'flowchart LR\n%%{init: {}}%%',
    'flowchart LR\n' + 'x'.repeat(30001),
  ]) {
    let rejected = false;
    try {
      window.validateDiagram(bad);
    } catch {
      rejected = true;
    }
    if (!rejected) throw new Error('Unsafe source accepted');
  }
});
assert.deepEqual(errors, []);
console.log(
  'PASS: streaming, stable portals, source/copy, SVG/PNG downloads, fullscreen zoom/pan/Escape, theme, mobile, errors, nested fences, 10 diagram types, colour styles and resource/config/CSS limits.',
);
await browser.close();
