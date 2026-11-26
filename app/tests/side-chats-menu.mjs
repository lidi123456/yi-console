// End-to-end Playwright verification for topbar SideChatsMenu quick-delete with inline confirmation, dark mode, and mobile touch layout.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs/promises';
import path from 'node:path';

const OUT_DIR = path.resolve('out');
const DOWNLOAD_DIR = process.env.YI_DOWNLOAD_DIR || '';
await fs.mkdir(OUT_DIR, { recursive: true });
if (DOWNLOAD_DIR) await fs.mkdir(DOWNLOAD_DIR, { recursive: true });

const PORT = 4189;
const APP_DIR = process.env.YI_APP_DIR || 'D:\\agent-build\\yi-console\\app';
const BASE = `http://127.0.0.1:${PORT}`;

const vite = spawn('cmd.exe', ['/c', `npx vite --host 127.0.0.1 --port ${PORT} --strictPort`], {
  cwd: APP_DIR,
  stdio: ['ignore', 'pipe', 'pipe'],
});

async function waitServer(url, timeoutMs = 30000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const r = await fetch(url);
      if (r.ok) return;
    } catch {
      /* wait */
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`Vite server did not start on ${url}`);
}

const manifest = [];
async function saveShot(page, name) {
  const outPath = path.join(OUT_DIR, name);
  await page.screenshot({ path: outPath });
  const buf = await fs.readFile(outPath);
  const sha256 = crypto.createHash('sha256').update(buf).digest('hex');
  let dlPath = '';
  if (DOWNLOAD_DIR) {
    dlPath = path.join(DOWNLOAD_DIR, name);
    await fs.copyFile(outPath, dlPath);
  }
  manifest.push({ name, bytes: buf.length, sha256, ...(dlPath ? { dlPath } : {}) });
}

try {
  await waitServer(`${BASE}/tests/side-chats-menu.html`);
  const browser = await chromium.launch({ channel: 'chrome', headless: true });

  try {
    // 1. Desktop light flow
    const ctx = await browser.newContext({
      viewport: { width: 1280, height: 820 },
      deviceScaleFactor: 2,
    });
    const page = await ctx.newPage();
    const errors = [];
    page.on('pageerror', (e) => errors.push(e.message));

    await page.goto(`${BASE}/tests/side-chats-menu.html`);
    const trigger = page.locator('.head-side-btn');
    await trigger.waitFor();
    assert.match(await trigger.innerText(), /旁路 · 3/);

    await trigger.click();
    const menu = page.locator('.side-chats-menu');
    await menu.waitFor();
    assert.equal(await menu.locator('.scm-row').count(), 3);

    // Review hint deduplicates tool/scope and shows 运行中
    const reviewRow = menu.locator('.scm-row').nth(0);
    const reviewHint = await reviewRow.locator('.ctx-hint').innerText();
    assert.equal(reviewHint.trim(), '运行中', `expected deduplicated hint "运行中", got: ${reviewHint}`);

    // Hover delete button on row 2 (used side chat) and capture light hover shot
    const usedRow = menu.locator('.scm-row').nth(1);
    await usedRow.locator('.scm-del').hover();
    await page.waitForTimeout(80);
    await saveShot(page, '01-side-menu-light-hover.png');

    // Empty normal side chat (row 3) deletes immediately without inline confirmation; menu stays open
    const emptyRow = menu.locator('.scm-row').nth(2);
    await emptyRow.locator('.scm-del').click();
    await page.waitForFunction(() => document.querySelectorAll('.side-chats-menu .scm-row').length === 2);
    assert.match(await trigger.innerText(), /旁路 · 2/);
    assert.equal(await menu.isVisible(), true, 'menu remains open after deleting empty side chat');
    let deletes = await page.evaluate(() => window.fixtureCalls.filter((c) => c.method === 'session.delete').map((c) => c.params.sessionId));
    assert.deepEqual(deletes, ['sc-empty-3']);

    // Used normal side chat reveals inline 确认删除 + cancel ×
    await usedRow.locator('.scm-del').click();
    await usedRow.locator('.scm-confirm-btn').waitFor();
    assert.equal((await usedRow.locator('.scm-confirm-btn').innerText()).trim(), '确认删除');
    await saveShot(page, '02-side-menu-inline-confirm.png');

    // Cancel via ×
    await usedRow.locator('.scm-cancel-btn').click();
    await usedRow.locator('.scm-del').waitFor();

    // Escape cancels inline confirm first, then second Escape closes menu
    await usedRow.locator('.scm-del').click();
    await usedRow.locator('.scm-confirm-btn').waitFor();
    await page.keyboard.press('Escape');
    await usedRow.locator('.scm-del').waitFor();
    assert.equal(await menu.isVisible(), true, 'first Escape only cancels inline confirmation');
    await page.keyboard.press('Escape');
    await menu.waitFor({ state: 'hidden' });

    // Reopen and verify window.__yiConsoleBack() cancels confirm first, then second closes menu
    await trigger.click();
    await menu.waitFor();
    await usedRow.locator('.scm-del').click();
    await usedRow.locator('.scm-confirm-btn').waitFor();
    await page.evaluate(() => window.fixtureBack());
    await usedRow.locator('.scm-del').waitFor();
    assert.equal(await menu.isVisible(), true, 'first back call only cancels inline confirmation');
    await page.evaluate(() => window.fixtureBack());
    await menu.waitFor({ state: 'hidden' });

    // Reopen and confirm deleting used normal side chat -> menu stays open with 1 row left
    await trigger.click();
    await menu.waitFor();
    await usedRow.locator('.scm-del').click();
    await usedRow.locator('.scm-confirm-btn').click();
    await page.waitForFunction(() => document.querySelectorAll('.side-chats-menu .scm-row').length === 1);
    assert.match(await trigger.innerText(), /旁路 · 1/);
    assert.equal(await menu.isVisible(), true, 'menu stays open while 1 side chat remains');

    // Running review side chat shows 停止并删除; confirming deletes it, clears active review id, and auto-closes menu
    assert.equal(await page.evaluate(() => window.fixtureGetActiveReviewId()), 'sc-review-1');
    await reviewRow.locator('.scm-del').click();
    await reviewRow.locator('.scm-confirm-btn').waitFor();
    assert.equal((await reviewRow.locator('.scm-confirm-btn').innerText()).trim(), '停止并删除');
    await reviewRow.locator('.scm-confirm-btn').click();
    await menu.waitFor({ state: 'hidden' });
    assert.equal(await trigger.count(), 0, 'trigger hides when sideChats is empty');
    assert.equal(await page.evaluate(() => window.fixtureGetActiveReviewId()), null, 'active review id cleared');
    deletes = await page.evaluate(() => window.fixtureCalls.filter((c) => c.method === 'session.delete').map((c) => c.params.sessionId));
    assert.deepEqual(deletes, ['sc-empty-3', 'sc-used-2', 'sc-review-1']);

    // 2. Dark mode screenshot
    await page.evaluate(() => {
      window.fixtureReset();
      window.fixtureSetTheme('dark');
    });
    await trigger.click();
    await menu.waitFor();
    await menu.locator('.scm-row').nth(1).locator('.scm-del').click();
    await menu.locator('.scm-row').nth(1).locator('.scm-confirm-btn').waitFor();
    await page.waitForTimeout(80);
    await saveShot(page, '03-side-menu-dark.png');

    assert.deepEqual(errors, []);
    await ctx.close();

    // 3. Mobile / touch layout verification
    const mobCtx = await browser.newContext({
      viewport: { width: 390, height: 844 },
      deviceScaleFactor: 2,
      hasTouch: true,
    });
    const mobPage = await mobCtx.newPage();
    const mobErrors = [];
    mobPage.on('pageerror', (e) => mobErrors.push(e.message));

    await mobPage.goto(`${BASE}/tests/side-chats-menu.html`);
    const mobTrigger = mobPage.locator('.head-side-btn');
    await mobTrigger.click();
    const mobMenu = mobPage.locator('.side-chats-menu');
    await mobMenu.waitFor();

    const menuBox = await mobMenu.boundingBox();
    assert.ok(menuBox && menuBox.x >= 4 && menuBox.x + menuBox.width <= 390 - 4, `mobile menu clamped in viewport: ${JSON.stringify(menuBox)}`);

    const rowBox = await mobMenu.locator('.scm-row').nth(0).boundingBox();
    const delBox = await mobMenu.locator('.scm-row').nth(0).locator('.scm-del').boundingBox();
    assert.ok(rowBox && rowBox.height >= 38, `mobile row height >= 38, got ${rowBox?.height}`);
    assert.ok(delBox && Math.round(delBox.width) === 30 && Math.round(delBox.height) === 30, `mobile del button 30x30, got ${JSON.stringify(delBox)}`);

    await mobMenu.locator('.scm-row').nth(1).locator('.scm-del').click();
    await mobMenu.locator('.scm-row').nth(1).locator('.scm-confirm-btn').waitFor();
    await mobPage.waitForTimeout(80);
    await saveShot(mobPage, '04-side-menu-mobile.png');

    assert.deepEqual(mobErrors, []);
    await mobCtx.close();

    console.log('PASS: side-chats-menu quick-delete E2E across desktop (light/inline-confirm/dark) & mobile.');
    console.log('E2E_MANIFEST=' + JSON.stringify(manifest));
  } finally {
    await browser.close();
  }
} finally {
  if (vite.pid) {
    try {
      execFileSync('taskkill', ['/T', '/F', '/PID', String(vite.pid)], { stdio: 'ignore' });
    } catch {
      /* already exited */
    }
  }
}
