// Run with windows-remote pw-run.sh while Vite serves the app on port 1429 (or passed via TEST_URL).
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';

await fs.mkdir('out', { recursive: true });
const baseUrl = process.env.TEST_URL || 'http://127.0.0.1:1429/tests/editor-search.html';
const browser = await chromium.launch({ channel: 'chrome', headless: true });
const errors = [];

try {
  // 1. Light mode: Find, live count, click-through, options, invalid regex, replace, modal z-index isolation
  const page = await browser.newPage({ viewport: { width: 900, height: 560 }, deviceScaleFactor: 2 });
  page.on('pageerror', (e) => errors.push(e.message));
  await page.goto(baseUrl, { waitUntil: 'networkidle' });
  await page.locator('.cm-content').waitFor();

  const line1Before = await page.locator('.cm-line').first().boundingBox();

  // Focus editor and press Ctrl+F
  await page.locator('.cm-line').first().click();
  await page.keyboard.press('Control+f');
  const findBox = page.locator('.ed-find');
  await findBox.waitFor();

  // Floating overlay must NOT push line 1 down
  const line1After = await page.locator('.cm-line').first().boundingBox();
  assert.equal(Math.round(line1After.y), Math.round(line1Before.y), 'line 1 must not shift when find panel opens');

  // Type search query -> live match count (case-insensitive finds 4 matches)
  const searchInput = page.locator('.ed-find input[name="search"]');
  await searchInput.fill('upstream_keepalive_pool_size');
  await page.waitForFunction(() => document.querySelector('.ed-count')?.textContent?.trim() === '1 / 4');

  // Stepping via Enter / Shift+Enter updates counter and does not pop up selection chip
  await page.keyboard.press('Enter');
  await page.waitForFunction(() => document.querySelector('.ed-count')?.textContent?.trim() === '2 / 4');
  assert.equal(await page.locator('.cm-sel-act').count(), 0, 'selection chip stays hidden during search navigation');
  await page.keyboard.press('Shift+Enter');
  await page.waitForFunction(() => document.querySelector('.ed-count')?.textContent?.trim() === '1 / 4');

  // Screenshot: compact find (light)
  await page.screenshot({ path: 'out/01-light-find.png' });

  // Click-through test: clicking left of the floating bar (same Y) reaches code lines
  const fb = await findBox.boundingBox();
  const hitLeft = await page.evaluate(({ x, y }) => {
    const el = document.elementFromPoint(x, y);
    return !!el?.closest('.cm-content, .cm-scroller');
  }, { x: Math.max(80, fb.x - 100), y: fb.y + fb.height / 2 });
  assert.ok(hitLeft, 'clicks to the left of floating find bar must pass through to editor');

  // Toggle Match Case (Aa) -> excludes UPSTREAM_KEEPALIVE_POOL_SIZE (4 -> 3)
  await page.locator('.ed-opt', { hasText: 'Aa' }).click();
  await page.waitForFunction(() => document.querySelector('.ed-count')?.textContent?.trim() === '1 / 3');

  // Toggle Whole Word (ab) -> excludes upstream_keepalive_pool_size_max (3 -> 2)
  await page.locator('.ed-opt', { hasText: 'ab' }).click();
  await page.waitForFunction(() => document.querySelector('.ed-count')?.textContent?.trim() === '1 / 2');

  // Invalid regex state
  await page.locator('.ed-opt', { hasText: '.*' }).click();
  await searchInput.fill('keepalive_[invalid');
  await page.waitForFunction(() => document.querySelector('.ed-count')?.textContent?.trim() === '正则无效');
  assert.ok(await page.locator('.ed-find-field').first().evaluate((el) => el.classList.contains('error')));
  assert.ok(await page.locator('.ed-ibtn[name="next"]').isDisabled());

  // Restore valid query and expand Replace with Ctrl+H
  await page.locator('.ed-opt', { hasText: '.*' }).click();
  await searchInput.fill('upstream_keepalive_pool_size');
  await page.waitForFunction(() => document.querySelector('.ed-count')?.textContent?.trim() === '1 / 2');
  await page.keyboard.press('Control+h');
  const repInput = page.locator('.ed-find input[name="replace"]');
  await repInput.waitFor();
  await repInput.fill('upstream_keepalive_pool');
  await page.screenshot({ path: 'out/02-light-replace.png' });

  // Replace current match -> 1 match remaining
  await page.locator('.ed-rbtn[name="replace"]').click();
  await page.waitForFunction(() => document.querySelector('.ed-count')?.textContent?.trim() === '1 / 1');

  // Modal z-index isolation test (the original bug!)
  await page.locator('#toggle-modal').click();
  await page.locator('#test-modal-bg').waitFor();
  const fb2 = await findBox.boundingBox();
  const topElAtFind = await page.evaluate(({ x, y }) => {
    const el = document.elementFromPoint(x, y);
    return { id: el?.id || '', cls: el?.className || '', inModalBg: !!el?.closest('.modal-bg') };
  }, { x: fb2.x + fb2.width / 2, y: fb2.y + fb2.height / 2 });
  assert.ok(topElAtFind.inModalBg, `modal backdrop must sit above search bar, got: ${JSON.stringify(topElAtFind)}`);
  await page.screenshot({ path: 'out/03-modal-over-find.png' });
  await page.locator('#toggle-modal').click({ force: true });

  // Read-only mode hides replace toggle and row
  await page.locator('#toggle-ro').click();
  await page.locator('.ed-find-toggle').waitFor({ state: 'hidden' });
  assert.ok(await page.locator('.ed-find input[name="replace"]').isHidden(), 'replace input must hide in read-only mode');

  // Escape closes search panel
  await searchInput.focus();
  await page.keyboard.press('Escape');
  await findBox.waitFor({ state: 'hidden' });

  // 2. Dark mode screenshot
  const darkPage = await browser.newPage({ viewport: { width: 900, height: 520 }, deviceScaleFactor: 2 });
  darkPage.on('pageerror', (e) => errors.push(e.message));
  await darkPage.goto(`${baseUrl}?theme=dark`, { waitUntil: 'networkidle' });
  await darkPage.locator('.cm-line').first().click();
  await darkPage.keyboard.press('Control+h');
  await darkPage.locator('.ed-find input[name="search"]').fill('upstream_keepalive_pool_size');
  await darkPage.waitForFunction(() => document.querySelector('.ed-count')?.textContent?.trim() === '1 / 4');
  await darkPage.screenshot({ path: 'out/04-dark-replace.png' });

  assert.deepEqual(errors, [], `unexpected page errors: ${errors.join('; ')}`);
  console.log('PASS: all editor search panel & modal z-index assertions passed');
} finally {
  await browser.close();
}
