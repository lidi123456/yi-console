// Run with windows-remote pw-run.sh while Vite serves the app on port 3001.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
await fs.mkdir('out', { recursive: true });
const browser = await chromium.launch({ channel: 'chrome', headless: true });
const page = await browser.newPage({ viewport: { width: 1100, height: 850 } });
try {
const errors = [];
page.on('pageerror', (e) => errors.push(e.message));
await page.goto('http://127.0.0.1:3001/tests/cursor-model-picker.html');
await page.locator('.model-btn').click();
await page.getByRole('button', { name: 'On', exact: true }).click();
await page.getByRole('button', { name: 'High', exact: true }).click();
assert.equal(await page.getByRole('button', { name: 'On', exact: true }).getAttribute('class'), 'active');
assert.equal(await page.getByRole('button', { name: 'High', exact: true }).getAttribute('class'), 'active');
assert.match(await page.locator('.model-btn').innerText(), /High.*Fast/s);
const requests = await page.evaluate(() => window.fixtureCalls.filter((v) => v.method === 'session.setModelConfig'));
assert.deepEqual(requests.map((r) => [r.params.configId, r.params.value]), [['fast', 'true'], ['reasoning', 'high']]);
await page.evaluate(() => window.fixtureLang('zh'));
await page.getByRole('button', { name: '开启', exact: true }).waitFor();
await page.screenshot({ path: 'out/cursor-model-desktop.png' });
await page.getByRole('button', { name: 'Model B', exact: true }).click();
await page.locator('.model-btn').click();
assert.equal(await page.getByRole('button', { name: '开启', exact: true }).count(), 0, 'hide parameters absent on the new model');
await page.evaluate(() => window.fixtureBack());
await page.getByRole('dialog').waitFor({ state: 'hidden' });
// Grok uses reasoning_effort, Claude may use effort; categories also support future parameter ids.
for (const id of ['reasoning_effort', 'effort', 'future_reasoning_id']) {
  const variant = await browser.newPage({ viewport: { width: 1100, height: 850 } });
  const category = id === 'future_reasoning_id' ? '&category=thought_level' : '';
  await variant.goto(`http://127.0.0.1:3001/tests/cursor-model-picker.html?reasoningId=${id}${category}`);
  await variant.locator('.model-btn').click();
  await variant.getByRole('button', { name: 'High', exact: true }).click();
  assert.match(await variant.locator('.model-btn').innerText(), /High/);
  const sent = await variant.evaluate(() => window.fixtureCalls.filter((v) => v.method === 'session.setModelConfig'));
  assert.deepEqual(sent.map((r) => [r.params.configId, r.params.value]), [[id, 'high']], 'send the original parameter id');
  if (id === 'reasoning_effort') await variant.screenshot({ path: 'out/cursor-grok-reasoning.png' });
  await variant.close();
}
const mobile = await browser.newPage({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
await mobile.goto('http://127.0.0.1:3001/tests/cursor-model-picker.html?reasoningId=reasoning_effort');
await mobile.locator('.model-btn').click();
await mobile.locator('.model-sheet').waitFor();
await mobile.getByRole('button', { name: 'On', exact: true }).click();
await mobile.screenshot({ path: 'out/cursor-model-mobile.png' });
await mobile.evaluate(() => window.fixtureBack());
await mobile.getByRole('dialog').waitFor({ state: 'hidden' });
assert.deepEqual(errors, []);
} finally {
  await browser.close();
}
console.log('PASS: Fast and reasoning RPC values, badges, translations, unsupported models, desktop/mobile and back handling');
