// End-to-end Playwright verification for banner-free user message re-edit with toast draft recovery across desktop & mobile.
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

const PORT = 4187;
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
  await waitServer(`${BASE}/tests/user-reedit.html`);
  const browser = await chromium.launch({ channel: 'chrome', headless: true });

  try {
    // 1. Desktop flow (Modern Light -> Stashed Draft Toast -> Switch -> Restore Draft -> Empty Fill Toast -> Dark -> EN -> Classic -> Send)
    const ctx = await browser.newContext({
      viewport: { width: 1280, height: 860 },
      deviceScaleFactor: 2,
      permissions: ['clipboard-read', 'clipboard-write'],
    });
    const page = await ctx.newPage();
    const errors = [];
    page.on('pageerror', (e) => errors.push(e.message));

    await page.goto(`${BASE}/tests/user-reedit.html`);
    const msg1 = page.locator('.msg-user[data-prompt-seq="1"]');
    const msg2 = page.locator('.msg-user[data-prompt-seq="4"]');
    await msg1.waitFor();

    // Hover user bubble #1 -> Re-edit and Copy buttons work cleanly, Copy shows checkmark feedback
    await msg1.hover();
    const reeditBtn1 = msg1.locator('button[aria-label="重新编辑（填入输入框）"]');
    const copyBtn1 = msg1.locator('button[aria-label="复制消息"]');
    await reeditBtn1.waitFor();
    await copyBtn1.click();
    await msg1.locator('button[aria-label="已复制"]').waitFor();
    const copiedText = await page.evaluate(() => navigator.clipboard.readText());
    assert.match(copiedText, /重新编辑按钮/);

    // Type an active draft in composer before re-editing
    const ta = page.locator('.composer textarea');
    await ta.fill('未发送的原草稿内容');

    // Click Re-edit on message #1 -> no top sheet or linked bubble; fills text + ref + image and shows toast with Restore draft
    await reeditBtn1.click();
    await page.locator('.composer .attach img').waitFor();
    await page.locator('.toast:has-text("原草稿已暂存")').waitFor();
    await page.waitForTimeout(80);

    assert.equal(await page.locator('.reedit-sheet').count(), 0, 'no top reedit-sheet banner');
    assert.equal(await page.locator('.msg-user.reedit-linked').count(), 0, 'no persistent bubble highlight');
    assert.match(await ta.inputValue(), /记得保留引用的代码片段和图片/);
    assert.equal(await page.locator('.composer .cref-chip').count(), 1);
    assert.equal(await page.locator('.composer .attach img').count(), 1);
    assert.equal(await page.locator('.toast button:has-text("恢复原草稿")').count(), 1);

    await saveShot(page, '01-reedit-light.png');

    // Switch directly to re-edit message #2 -> replaces previous toast cleanly and keeps Restore draft action
    await msg2.hover();
    await msg2.locator('button[aria-label="重新编辑（填入输入框）"]').click();
    assert.match(await ta.inputValue(), /暗色模式和手机端/);
    assert.equal(await page.locator('.composer .attach img').count(), 0);
    assert.equal(await page.locator('.toast button:has-text("恢复原草稿")').count(), 1);

    // Click Restore draft in toast -> restores original draft text, focuses composer, and shows confirmation toast
    await page.locator('.toast button:has-text("恢复原草稿")').click();
    assert.equal(await ta.inputValue(), '未发送的原草稿内容');
    await page.locator('.toast:has-text("已恢复原草稿")').waitFor();
    await page.waitForFunction(() => document.activeElement === document.querySelector('.composer textarea'));

    // Clear composer -> Re-edit on message #1 shows simple toast without Restore draft button
    await ta.fill('');
    assert.equal(await ta.inputValue(), '');
    await msg1.hover();
    await msg1.locator('button[aria-label="重新编辑（填入输入框）"]').click();
    await page.locator('.composer .attach img').waitFor();
    await page.locator('.toast:has-text("已填入输入框")').waitFor();
    assert.equal(await page.locator('.toast button:has-text("恢复原草稿")').count(), 0);

    // Dark theme screenshot
    await page.evaluate(() => window.setTheme('dark'));
    await page.waitForTimeout(150);
    await saveShot(page, '02-reedit-dark.png');

    // English UI check for stashed-draft toast
    await page.evaluate(() => window.setLangPref('en'));
    await ta.fill('unsent english draft');
    await msg1.hover();
    await msg1.locator('button[aria-label="Re-edit (refill into input)"]').click();
    await page.locator('.composer .attach img').waitFor();
    await page.locator('.toast:has-text("Refilled into input; previous draft stashed")').waitFor();
    assert.equal(await page.locator('.toast button:has-text("Restore draft")').count(), 1);

    // Classic style screenshot (hovering user message #1 so action icons are visible)
    await page.evaluate(() => {
      window.setLangPref('zh');
      window.setTheme('light');
      window.setChatStyle('claude');
    });
    await page.waitForTimeout(120);
    assert.equal(await page.locator('.msg-user-actions.classic').count(), 2);
    await ta.fill('经典模式草稿');
    await msg1.hover();
    await msg1.locator('button[aria-label="重新编辑（填入输入框）"]').click();
    await page.locator('.composer .attach img').waitFor();
    await page.locator('.toast button:has-text("恢复原草稿")').waitFor();
    await msg1.hover();
    await page.waitForTimeout(80);
    await saveShot(page, '04-reedit-classic.png');

    // Switch back to modern and send edited message -> sends context + image + edited text and closes re-edit toast
    await page.evaluate(() => window.setChatStyle('modern'));
    await ta.fill('帮我把用户消息气泡下面加一个重新编辑按钮（已修改重发）。');
    await page.locator('.send-btn').click();
    await page.waitForTimeout(100);
    assert.equal(await page.locator('.toast button:has-text("恢复原草稿")').count(), 0);
    const sendCall = await page.evaluate(() => window.fixtureCalls.find((c) => c.method === 'session.send'));
    assert.ok(sendCall, 'session.send was called');
    assert.match(String(sendCall.params.text), /已修改重发/);
    assert.match(String(sendCall.params.text), /<yi-context/);
    assert.equal(sendCall.params.images.length, 1);

    assert.deepEqual(errors, []);
    await ctx.close();

    // 2. Mobile viewport verification (hover: none, persistent 28x28 buttons on single row, clean composer + toast)
    const mobCtx = await browser.newContext({
      viewport: { width: 390, height: 844 },
      deviceScaleFactor: 2,
      hasTouch: true,
    });
    const mobPage = await mobCtx.newPage();
    const mobErrors = [];
    mobPage.on('pageerror', (e) => mobErrors.push(e.message));

    await mobPage.goto(`${BASE}/tests/user-reedit.html`);
    const mobMsg1 = mobPage.locator('.msg-user[data-prompt-seq="1"]');
    await mobMsg1.waitFor();

    const ys = await mobMsg1.locator('.msg-user-actions .m-act-btn').evaluateAll((els) =>
      els.map((el) => Math.round(el.getBoundingClientRect().top)),
    );
    assert.equal(ys.length, 3);
    assert.ok(Math.max(...ys) - Math.min(...ys) <= 2, `mobile action buttons wrapped: ${JSON.stringify(ys)}`);

    await mobPage.locator('.composer textarea').fill('手机草稿');
    await mobMsg1.locator('button[aria-label="重新编辑（填入输入框）"]').click();
    await mobPage.locator('.composer .attach img').waitFor();
    await mobPage.locator('.toast button:has-text("恢复原草稿")').waitFor();
    assert.equal(await mobPage.locator('.reedit-sheet').count(), 0);
    await mobPage.waitForTimeout(80);
    await saveShot(mobPage, '03-reedit-mobile.png');

    assert.deepEqual(mobErrors, []);
    await mobCtx.close();

    console.log('PASS: user-reedit E2E across desktop (light/dark/en/classic/send) & mobile.');
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
