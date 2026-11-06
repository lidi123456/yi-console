// Run with windows-remote pw-run.sh while Vite serves the app on port 3001.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
await fs.mkdir('out', { recursive: true });
const browser = await chromium.launch({ channel: 'chrome', headless: true });
const errors = [];
const url = 'http://127.0.0.1:3001/tests/cursor-model-picker.html';
async function fixture(query = '', options = {}) {
  const page = await browser.newPage({ viewport: { width: 1100, height: 850 }, ...options });
  page.on('pageerror', (error) => errors.push(error.message));
  await page.goto(url + query);
  await page.locator('.model-btn').click();
  await page.getByRole('switch', { name: 'Fast', exact: true }).waitFor();
  return page;
}
const row = (page, name) => page.getByRole('button', { name, exact: true });
const choice = (page, name) => page.getByRole('menuitemradio', { name, exact: true });
const requests = (page, method) => page.evaluate((m) => window.fixtureCalls.filter((r) => r.method === m), method);
const capture = async (page, name) => {
  const boxes = await page.locator('.model-btn, .model-menu, .model-submenu, .model-sheet').evaluateAll((nodes) => nodes.map((node) => {
    const r = node.getBoundingClientRect(); return { x: r.x, y: r.y, right: r.right, bottom: r.bottom };
  }));
  const size = page.viewportSize();
  const x = Math.max(0, Math.floor(Math.min(...boxes.map((b) => b.x)) - 12));
  const y = Math.max(0, Math.floor(Math.min(...boxes.map((b) => b.y)) - 12));
  const right = Math.min(size.width, Math.ceil(Math.max(...boxes.map((b) => b.right)) + 12));
  const bottom = Math.min(size.height, Math.ceil(Math.max(...boxes.map((b) => b.bottom)) + 12));
  await page.screenshot({ path: `out/${name}.png`, clip: { x, y, width: right - x, height: bottom - y } });
};
const insideViewport = async (page) => {
  const box = await page.locator('.model-submenu').boundingBox();
  const size = page.viewportSize();
  assert.ok(box.x >= 8 && box.y >= 8 && box.x + box.width <= size.width - 8 && box.y + box.height <= size.height - 8, JSON.stringify(box));
};
try {
  const page = await fixture('?reasoningId=reasoning_effort');
  assert.equal(await row(page, 'Auto').count(), 0, 'the root menu does not contain the full model list');
  assert.ok((await page.getByRole('dialog').boundingBox()).height < 210, 'compact settings root');
  await page.getByRole('switch', { name: 'Fast', exact: true }).click();
  await page.waitForFunction(() => document.querySelector('[role=switch]')?.getAttribute('aria-checked') === 'true');
  await row(page, 'Reasoning Effort').click();
  await choice(page, 'High').click();
  await page.locator('.model-submenu').waitFor({ state: 'hidden' });
  assert.match(await row(page, 'Reasoning Effort').innerText(), /High/);
  assert.match(await page.locator('.model-btn').innerText(), /High.*Fast/s);
  await row(page, 'Context Window').click();
  await choice(page, '500K').click();
  await page.locator('.model-submenu').waitFor({ state: 'hidden' });
  assert.match(await row(page, 'Context Window').innerText(), /500K/);
  assert.deepEqual((await requests(page, 'session.setModelConfig')).map((r) => [r.params.configId, r.params.value]),
    [['fast', 'true'], ['reasoning_effort', 'high'], ['context', '500k']]);
  await capture(page, 'model-settings-desktop');
  await row(page, 'Reasoning Effort').hover();
  await choice(page, 'High').waitFor();
  await insideViewport(page);
  assert.ok((await page.locator('.model-submenu').boundingBox()).x < (await page.getByRole('dialog').boundingBox()).x, 'submenu flips to the left');
  await capture(page, 'model-effort-desktop');
  await choice(page, 'High').focus();
  await page.keyboard.press('Escape');
  await page.locator('.model-submenu').waitFor({ state: 'hidden' });
  await page.getByRole('dialog').waitFor();
  await row(page, 'Model').focus();
  await page.keyboard.press('ArrowRight');
  await page.getByRole('textbox', { name: 'Search models' }).waitFor();
  await insideViewport(page);
  await capture(page, 'model-list-desktop');
  await row(page, 'Reasoning Effort').hover();
  await choice(page, 'High').waitFor();
  await page.keyboard.press('Escape');
  await page.locator('.model-submenu').waitFor({ state: 'hidden' });
  await row(page, 'Model').click();
  await page.getByRole('textbox', { name: 'Search models' }).fill('no-such-model');
  await page.getByText('No matching models', { exact: true }).waitFor();
  await page.getByRole('textbox', { name: 'Search models' }).fill('Auto');
  await page.keyboard.press('ArrowLeft');
  await page.getByRole('textbox', { name: 'Search models' }).waitFor();
  await row(page, 'Auto').click();
  await page.getByRole('dialog').waitFor({ state: 'hidden' });
  await page.locator('.model-btn').click();
  await page.locator('.model-list').getByRole('button', { name: 'Auto', exact: true }).waitFor();
  assert.equal(await page.getByRole('switch').count(), 0, 'unsupported model hides parameter controls');
  await page.keyboard.press('Escape');
  await page.getByRole('dialog').waitFor({ state: 'hidden' });

  for (const id of ['reasoning', 'effort', 'future_reasoning_id']) {
    const variant = await fixture(`?reasoningId=${id}${id === 'future_reasoning_id' ? '&category=thought_level' : ''}`);
    await row(variant, 'Reasoning Effort').click();
    await choice(variant, 'High').click();
    assert.deepEqual((await requests(variant, 'session.setModelConfig')).map((r) => [r.params.configId, r.params.value]), [[id, 'high']]);
    await variant.close();
  }
  const right = await fixture('?left&dark');
  await row(right, 'Context Window').hover();
  await choice(right, '256K').waitFor();
  await insideViewport(right);
  assert.ok((await right.locator('.model-submenu').boundingBox()).x > (await right.getByRole('dialog').boundingBox()).x, 'submenu opens to the right when space permits');
  await capture(right, 'model-context-dark');
  await right.mouse.click(50, 50);
  await right.getByRole('dialog').waitFor({ state: 'hidden' });

  const resized = await fixture();
  await row(resized, 'Model').click();
  await resized.setViewportSize({ width: 390, height: 844 });
  await resized.locator('.model-sheet').waitFor();
  await row(resized, 'Reasoning Effort').waitFor();
  await resized.setViewportSize({ width: 800, height: 450 });
  await resized.locator('.model-sheet').waitFor({ state: 'hidden' });
  await row(resized, 'Model').click();
  await resized.getByRole('textbox', { name: 'Search models' }).waitFor();
  await insideViewport(resized);
  await resized.close();
  const mobile = await fixture('?reasoningId=reasoning_effort', { viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
  await mobile.locator('.model-sheet').waitFor();
  await row(mobile, 'Reasoning Effort').click();
  assert.equal(await mobile.locator('.model-submenu').count(), 0, 'mobile navigates inside the sheet');
  await choice(mobile, 'High').click();
  await row(mobile, 'Model').waitFor();
  await row(mobile, 'Model').click();
  await mobile.getByRole('textbox', { name: 'Search models' }).waitFor();
  await capture(mobile, 'model-list-mobile');
  await mobile.evaluate(() => window.fixtureBack());
  await row(mobile, 'Model').waitFor();
  await mobile.evaluate(() => window.fixtureLang('zh'));
  await row(mobile, '上下文窗口').click();
  await choice(mobile, '256K').waitFor();
  await row(mobile, '返回模型设置').click();
  await capture(mobile, 'model-settings-mobile');
  await mobile.evaluate(() => window.fixtureBack());
  await mobile.getByRole('dialog').waitFor({ state: 'hidden' });

  for (const capabilities of ['none', 'single']) {
    const plain = await browser.newPage();
    await plain.goto(url + `?capabilities=${capabilities}`);
    await plain.locator('.model-btn').click();
    await row(plain, 'Auto').waitFor();
    assert.equal(await plain.getByRole('switch').count(), 0);
    assert.equal(await row(plain, 'Reasoning Effort').count(), 0);
    await plain.close();
  }
  for (const tool of ['claude', 'codex']) {
    const generic = await browser.newPage();
    await generic.goto(url + '?tool=' + tool);
    await generic.locator('.model-btn').click();
    await row(generic, 'Reasoning Effort').click();
    await choice(generic, 'High').click();
    assert.deepEqual((await requests(generic, 'session.setEffort')).map((r) => r.params.effort), ['high']);
    await generic.close();
  }
  const failure = await fixture('?failFast');
  await failure.getByRole('switch', { name: 'Fast' }).click();
  await failure.waitForFunction(() => !document.querySelector('[role=switch]')?.disabled);
  assert.equal(await failure.getByRole('switch', { name: 'Fast' }).getAttribute('aria-checked'), 'false');
  const delayed = await fixture('?delayed');
  const select = async (name) => {
    await row(delayed, 'Model').click();
    await delayed.locator('.model-list').getByRole('button', { name, exact: true }).click();
    await delayed.locator('.model-btn').click();
  };
  const loading = 'Reading this model’s parameters…';
  await select('Auto');
  await delayed.getByText(loading, { exact: true }).waitFor();
  assert.equal(await row(delayed, 'Reasoning Effort').count(), 0, 'cold models do not show previous parameters');
  assert.equal(await row(delayed, 'Model').isDisabled(), false, 'another model can be chosen while parameters load');
  assert.equal(await delayed.locator('.model-btn .spinner').count(), 0, 'the trigger never shows a loading spinner');
  await capture(delayed, 'model-cache-cold-loading');
  await delayed.getByText(loading, { exact: true }).waitFor({ state: 'hidden' });
  // Auto has no parameters, so the menu is the model list itself
  await delayed.getByRole('textbox', { name: 'Search models' }).waitFor();
  await delayed.locator('.model-list').getByRole('button', { name: 'Grok 4.7', exact: true }).click();
  await delayed.locator('.model-btn').click();
  // Grok's parameters were cached when the fixture opened on it: usable at once
  await row(delayed, 'Reasoning Effort').waitFor();
  assert.equal(await delayed.getByText(loading, { exact: true }).count(), 0);
  assert.equal(await row(delayed, 'Reasoning Effort').isDisabled(), false, 'cached controls are usable at once');
  await capture(delayed, 'model-cache-warm');
  await delayed.close();
  assert.deepEqual(errors, []);
} finally {
  await browser.close();
}
console.log('PASS: compact settings, original RPC ids, Fast/context/effort, model search, submenu placement, keyboard/back, mobile, aliases, unsupported models, Claude/Codex and rejected changes');
