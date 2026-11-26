// Mockup screenshots for the manual model refresh buttons. Run with windows-remote pw-run.sh;
// APP_DIR is the synced source copy on Windows.
import { chromium } from 'playwright';
import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs/promises';

await fs.mkdir('out', { recursive: true });
// Also copy the screenshots there when set
const SHOT_DIR = process.env.SHOT_DIR;
const PORT = 4183;
const APP_DIR = process.env.APP_DIR ?? 'D:\\agent-build\\yi-console-model-refresh\\app';
const BASE = `http://127.0.0.1:${PORT}/tests/model-refresh.html`;
const vite = spawn('cmd.exe', ['/c', `npx vite --host 127.0.0.1 --port ${PORT} --strictPort`], { cwd: APP_DIR, stdio: 'ignore' });

async function waitServer(url, timeoutMs = 40000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try { if ((await fetch(url)).ok) return; } catch { /* wait */ }
    await new Promise((r) => setTimeout(r, 300));
  }
  throw new Error(`Vite did not start on ${url}`);
}

async function clipShot(page, selector, name, pad = 14) {
  await page.mouse.move(2, 2);
  await page.waitForTimeout(200);
  const boxes = await page.locator(selector).evaluateAll((nodes) => nodes.map((n) => {
    const r = n.getBoundingClientRect(); return { x: r.x, y: r.y, right: r.right, bottom: r.bottom };
  }));
  const size = page.viewportSize();
  const x = Math.max(0, Math.floor(Math.min(...boxes.map((b) => b.x)) - pad));
  const y = Math.max(0, Math.floor(Math.min(...boxes.map((b) => b.y)) - pad));
  const right = Math.min(size.width, Math.ceil(Math.max(...boxes.map((b) => b.right)) + pad));
  const bottom = Math.min(size.height, Math.ceil(Math.max(...boxes.map((b) => b.bottom)) + pad));
  await page.screenshot({ path: `out/${name}.png`, clip: { x, y, width: right - x, height: bottom - y } });
}

const errors = [];
try {
  await waitServer(BASE);
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    // Desktop: session model picker
    for (const dark of [false, true]) {
      const page = await browser.newPage({ viewport: { width: 1100, height: 760 }, deviceScaleFactor: 2 });
      page.on('pageerror', (e) => errors.push(e.message));
      await page.goto(BASE + (dark ? '?dark' : ''));
      await page.locator('.model-btn').click();
      await page.getByRole('button', { name: '模型', exact: true }).click();
      await page.locator('.model-refresh').waitFor();
      const sel = '.model-btn, .model-menu, .model-submenu';
      const tag = dark ? '-dark' : '';
      await clipShot(page, sel, `1-picker-idle${tag}`);
      await page.locator('.model-refresh').click();
      await page.getByText('正在向工具重新读取模型…').waitFor();
      await clipShot(page, sel, `2-picker-refreshing${tag}`);
      await page.getByText('已刷新，新增 2 个模型').waitFor();
      await clipShot(page, sel, `3-picker-done${tag}`);
      await page.close();
    }
    // Phone: bottom sheet
    {
      const page = await browser.newPage({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 3, isMobile: true, hasTouch: true });
      page.on('pageerror', (e) => errors.push(e.message));
      await page.goto(BASE);
      await page.locator('.model-btn').tap();
      await page.getByRole('button', { name: '模型', exact: true }).tap();
      await page.locator('.model-refresh').waitFor();
      await page.locator('.model-refresh').tap();
      await page.getByText('已刷新，新增 2 个模型').waitFor();
      await page.screenshot({ path: 'out/4-phone-done.png' });
    }
    // Host settings: tools list
    {
      const page = await browser.newPage({ viewport: { width: 920, height: 560 }, deviceScaleFactor: 2 });
      page.on('pageerror', (e) => errors.push(e.message));
      await page.goto(BASE + '?view=host');
      const buttons = page.getByRole('button', { name: '刷新模型' });
      await buttons.first().waitFor();
      await page.getByText('有新版本 0.172.0').waitFor();
      await clipShot(page, '.host-card', '5-host-idle');
      await buttons.nth(1).click();
      await page.getByText('正在刷新模型…').waitFor();
      await clipShot(page, '.host-card', '6-host-refreshing');
      await page.getByText('Codex：已刷新模型，共 6 个，新增 2 个').waitFor();
      await page.screenshot({ path: 'out/7-host-done.png' });
    }
  } finally {
    await browser.close();
  }
  if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  if (SHOT_DIR) await fs.cp('out', SHOT_DIR, { recursive: true });
  console.log('OK');
} finally {
  try { execFileSync('taskkill', ['/T', '/F', '/PID', String(vite.pid)], { stdio: 'ignore' }); } catch { /* already gone */ }
}
