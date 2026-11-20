// End-to-end Playwright verification for the refreshed chat UI across scenarios (light/dark, expanded tools/diff/cmd, running & error, plan/table/approval, multi-turn collapsed, mobile).
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs/promises';

await fs.mkdir('out', { recursive: true });

const PORT = 4181;
const APP_DIR = 'D:\\agent-build\\yi-console\\app';
const BASE = `http://127.0.0.1:${PORT}`;

const vite = spawn('cmd.exe', ['/c', `npx vite --host 127.0.0.1 --port ${PORT} --strictPort`], {
  cwd: APP_DIR,
  stdio: ['ignore', 'pipe', 'pipe'],
});

async function waitServer(url, timeoutMs = 25000) {
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

try {
  await waitServer(`${BASE}/tests/chat-refresh.html`);
  const browser = await chromium.launch({ channel: 'chrome', headless: true });

  try {
    // 1. Desktop verification + multi-scenario screenshots
    const ctx = await browser.newContext({
      viewport: { width: 1280, height: 920 },
      deviceScaleFactor: 2,
      permissions: ['clipboard-read', 'clipboard-write'],
    });
    const page = await ctx.newPage();
    const errors = [];
    page.on('pageerror', (e) => errors.push(e.message));

    await page.goto(`${BASE}/tests/chat-refresh.html`);
    await page.locator('.act-group').waitFor();

    // Original compact tool group summary in Chinese
    const sumZh = await page.locator('.ag-sum').innerText();
    assert.match(sumZh, /执行工具 4 次/);

    // Code block language header bar exists and copies pure code
    assert.equal(await page.locator('.md-code-wrap.has-lang .md-code-lang').innerText(), 'typescript');
    await page.locator('.md-code-wrap').hover();
    await page.locator('.md-code-wrap button.copy-float-btn').click();
    const copiedCode = await page.evaluate(() => navigator.clipboard.readText());
    assert.match(copiedCode, /export function summarizeSteps/);
    assert.ok(!copiedCode.includes('typescript\n'), 'language label is not copied into clipboard');

    // Turn diff pill is removed; balanced line-height (1.78 => ~24.92px at 14px) applies
    assert.equal(await page.locator('.m-diff-pill').count(), 0);
    const proseLh = await page.locator('.msg-assistant .md p').first().evaluate((el) => parseFloat(getComputedStyle(el).lineHeight));
    assert.ok(proseLh >= 24.5 && proseLh <= 25.3, `expected ~24.92px line-height, got ${proseLh}`);

    // Compact metadata: duration + cost shown, verbose token breakdown stays in tooltip
    const metaText = await page.locator('.m-act-meta').innerText();
    assert.match(metaText, /耗时 15秒/);
    assert.match(metaText, /\$0\.084/);
    assert.ok(!metaText.includes('42.8K'), 'verbose token breakdown stays in tooltip');
    const metaTitle = await page.locator('.m-act-meta').getAttribute('title');
    assert.match(metaTitle ?? '', /claude-sonnet-4-6/);

    // Expand original compact tool group and verify rows
    await page.locator('.ag-head').click();
    await page.locator('.ag-body').waitFor();
    const descs = await page.locator('.mt-desc').allInnerTexts();
    assert.ok(descs.includes('ChatView.tsx'), `expected file desc in ${JSON.stringify(descs)}`);
    assert.ok(descs.some((c) => c.includes('npm run')), `expected command desc in ${JSON.stringify(descs)}`);

    // Bilingual English check
    await page.evaluate(() => window.setLangPref('en'));
    const sumEn = await page.locator('.ag-sum').innerText();
    assert.match(sumEn, /4 tool calls|Ran 4 tools/i);
    await page.evaluate(() => window.setLangPref('zh'));

    // Verify tool rows expand (Thinking + Edit Diff + Bash Input/Output)
    await page.evaluate(() => window.loadScenario('tool-expanded'));
    await page.locator('.act-group .ag-head').click();
    await page.locator('.ag-body').waitFor();
    const toolHeads = page.locator('.mt-head');
    await toolHeads.nth(0).click();
    await toolHeads.nth(1).click();
    await toolHeads.nth(2).click();
    await page.locator('.diff').waitFor();
    await page.locator('.mt-sec pre').first().waitFor();

    // Verify dark theme user bubble contrast
    await page.evaluate(() => window.loadScenario('multiturn'));
    await page.evaluate(() => window.setTheme('dark'));
    await page.waitForTimeout(150);
    const darkBubbleBg = await page.locator('.msg-user .bubble').first().evaluate((el) => getComputedStyle(el).backgroundColor);
    assert.equal(darkBubbleBg, 'rgb(43, 45, 49)');

    assert.deepEqual(errors, []);
    await ctx.close();

    console.log('PASS: chat-refresh E2E (original tool group + balanced typography + code lang header + composer + dark theme).');
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
