// Screenshots of the app for the sessions in out/report.json (run after windows-model-e2e.mjs, same yi-web)
import { chromium } from 'playwright';
import fs from 'node:fs/promises';

const web = process.env.YI_BROWSER_TEST_WEB || 'http://127.0.0.1:8194';
const token = process.env.YI_BROWSER_TEST_TOKEN || 'browser-e2e';
const report = JSON.parse(await fs.readFile('out/report.json', 'utf8'));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const browser = await chromium.launch({ channel: 'chrome', headless: true });
// The host id: ask yi-web
const ws = new WebSocket(`${web.replace('http', 'ws')}/ws?token=${token}`);
await new Promise((r, j) => (ws.addEventListener('open', r, { once: true }), ws.addEventListener('error', j, { once: true })));
const hosts = await new Promise((r) => {
  ws.addEventListener('message', (e) => r(JSON.parse(e.data).result), { once: true });
  ws.send(JSON.stringify({ id: 1, cmd: 'hosts.list', args: {} }));
});
const host = hosts.hosts.find((h) => h.config.kind === 'local').config.id;
ws.close();
for (const m of report.models.filter((m) => m.sessionId && m.order)) {
  const ctx = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
  await ctx.addInitScript(
    ({ host, sid }) => {
      localStorage.setItem('ui-lang', JSON.stringify('zh'));
      localStorage.setItem('ui-theme', JSON.stringify('light'));
      localStorage.setItem('yi-console-view', JSON.stringify({ kind: 'session', hostId: host, sessionId: sid }));
    },
    { host, sid: m.sessionId },
  );
  const page = await ctx.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  await page.goto(`${web}/?token=${token}`);
  try {
    // Tool calls of a turn are folded under "执行工具 N 次": open the folds first
    await page.getByText(/执行工具 \d+ 次/).first().waitFor({ timeout: 30000 });
    for (const fold of await page.getByText(/执行工具 \d+ 次/).all()) await fold.click().catch(() => {});
    await page.getByText('browser_screenshot').first().waitFor({ timeout: 10000 });
  } catch {
    await page.screenshot({ path: `out/${m.tool}-debug.png` });
    console.log(m.tool, 'no tool rows; body:', (await page.locator('body').innerText()).slice(0, 600));
    await ctx.close();
    continue;
  }
  await sleep(1000);
  // Expand the screenshot rows so their images show
  for (const head of await page.locator('button').filter({ hasText: 'browser_screenshot' }).all()) await head.click().catch(() => {});
  const ok = await page
    .waitForFunction(() => [...document.querySelectorAll('.computer-use-images img')].some((img) => img.complete && img.naturalWidth > 0), null, { timeout: 15000 })
    .then(() => true, () => false);
  await sleep(500);
  await page.screenshot({ path: `out/${m.tool}-chat.png`, fullPage: true });
  const btn = page.locator('.computer-use-btn[aria-label="AI 浏览器"]');
  if (await btn.count()) {
    await btn.click();
    await page.locator('.computer-use-setup').waitFor();
    await sleep(400);
    await page.screenshot({ path: `out/${m.tool}-dialog.png` });
  }
  console.log(m.tool, 'shot', { image: ok, errors });
  await ctx.close();
}
await browser.close();
// The e2e run left the AI browsers on for these shots
const ws2 = new WebSocket(`${web.replace('http', 'ws')}/ws?token=${token}`);
await new Promise((r) => ws2.addEventListener('open', r, { once: true }));
let n = 100;
for (const m of report.models.filter((m) => m.sessionId)) {
  ws2.send(JSON.stringify({ id: ++n, cmd: 'rpc', args: { id: host, method: 'session.browserUse', params: { sessionId: m.sessionId, action: 'stop' } } }));
}
await sleep(2000);
ws2.close();
