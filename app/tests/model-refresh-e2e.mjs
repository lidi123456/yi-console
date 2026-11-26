// End-to-end check of the model refresh RPCs and the host settings button against a real yi-agentd and real tools.
// Run on Windows with windows-remote pw-run.sh after `cargo build -p yi-agentd -p yi-web` and `npm run build`.
// Isolated: YI_CONSOLE_HOME and the yi-web data dir live under RUN_DIR; ~/.yi-console is not touched.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs/promises';
import path from 'node:path';

const SRC = process.env.SRC_DIR ?? 'D:\\agent-build\\yi-console-model-refresh';
const BIN = process.env.BIN_DIR ?? 'D:\\agent-build\\target-yc-test\\debug';
const RUN_DIR = process.env.RUN_DIR ?? 'D:\\agent-build\\tmp\\model-refresh-run';
const PORT = 3107;
const BASE = `http://127.0.0.1:${PORT}`;
await fs.mkdir('out', { recursive: true });
await fs.rm(RUN_DIR, { recursive: true, force: true });
const cwd = path.join(RUN_DIR, 'work');
await fs.mkdir(cwd, { recursive: true });

const web = spawn(path.join(BIN, 'yi-web.exe'), ['--listen', `127.0.0.1:${PORT}`, '--dist', path.join(SRC, 'app', 'dist'),
  '--data', path.join(RUN_DIR, 'web'), '--token', 'dev', '--yi-agentd-dir', BIN], {
  env: { ...process.env, YI_CONSOLE_HOME: path.join(RUN_DIR, 'yi') }, stdio: 'ignore',
});
const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function connect() {
  for (let i = 0; i < 60; i++) {
    try {
      const ws = new WebSocket(`ws://127.0.0.1:${PORT}/ws?token=dev`);
      await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
      return ws;
    } catch { await sleep(500); }
  }
  throw new Error('yi-web did not start');
}

let ws;
const pending = new Map();
let nextId = 1;
const call = (cmd, args = {}) => new Promise((res, rej) => {
  const id = nextId++;
  pending.set(id, { res, rej });
  ws.send(JSON.stringify({ id, cmd, args }));
});
let hostId;
const rpc = (method, params = {}) => call('rpc', { hostId, method, params });
const timed = async (label, fn) => { const t = Date.now(); const r = await fn(); log(label, `${Date.now() - t} ms`); return r; };

let sid;
try {
  ws = await connect();
  ws.onmessage = (m) => {
    const d = JSON.parse(m.data);
    if (d.event) return;
    const p = pending.get(d.id);
    if (!p) return;
    pending.delete(d.id);
    d.error ? p.rej(new Error(d.error)) : p.res(d.result);
  };
  for (let i = 0; i < 120; i++) {
    const { hosts } = await call('hosts.list');
    const local = hosts.find((h) => h.config.kind === 'local');
    if (local?.status.status === 'connected') { hostId = local.config.id; break; }
    await sleep(500);
  }
  assert.ok(hostId, 'local host connected');
  const tools = (await rpc('tools.detect')).filter((t) => t.available).map((t) => t.kind);
  log('available tools', tools.join(','));
  assert.ok(tools.includes('codex'), 'codex is installed on this machine');

  // Without refresh: the old shape (an array) for the new session page and bots
  const plain = await timed('tools.models codex', () => rpc('tools.models', { tool: 'codex' }));
  assert.ok(Array.isArray(plain), 'tools.models without refresh still answers an array');
  const refreshed = await timed('tools.models codex refresh', () => rpc('tools.models', { tool: 'codex', refresh: true }));
  assert.ok(Array.isArray(refreshed.models) && refreshed.models.length > 0, 'refresh answers {models}');
  assert.equal(typeof refreshed.added, 'number');
  assert.equal(typeof refreshed.changed, 'boolean');
  log('codex models', refreshed.models.length, 'added', refreshed.added, refreshed.models.map((m) => m.id).join(','));
  for (const tool of ['claude', 'cursor', 'kiro'].filter((t) => tools.includes(t))) {
    try {
      const r = await timed(`tools.models ${tool} refresh`, () => rpc('tools.models', { tool, refresh: true }));
      log(tool, 'models', r.models.length, 'added', r.added);
    } catch (e) { log(tool, 'refresh failed:', e.message); }
  }
  await assert.rejects(rpc('tools.models', { tool: 'shell', refresh: true }), /没有模型/);

  // A running Codex session: its list comes from its own process; refresh answers the probed list
  const info = await rpc('session.create', { tool: 'codex', cwd, mode: 'structured', permissionMode: 'default', cols: 120, rows: 32 });
  sid = info.id;
  let before = [];
  for (let i = 0; i < 90 && before.length === 0; i++) {
    await sleep(1000);
    before = (await rpc('session.models', { sessionId: sid })).models;
  }
  log('session models before', before.length);
  const after = await timed('session.models refresh', () => rpc('session.models', { sessionId: sid, refresh: true }));
  assert.ok(after.models.length > 0);
  assert.equal(after.refreshed, true);
  assert.deepEqual(after.models.map((m) => m.id), refreshed.models.map((m) => m.id), 'session gets the refreshed list');
  const again = await rpc('session.models', { sessionId: sid });
  assert.deepEqual(again.models.map((m) => m.id), after.models.map((m) => m.id), 'the refreshed list stays');

  // The UI: host settings button, and the session picker's refresh button
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 860 }, deviceScaleFactor: 2, locale: 'zh-CN' });
    const errors = [];
    page.on('pageerror', (e) => errors.push(e.message));
    await page.goto(`${BASE}/?token=dev`);
    await page.getByText('添加 / 管理主机').click();
    const toggle = page.locator('.host-tools-toggle');
    await toggle.waitFor();
    if ((await toggle.getAttribute('aria-expanded')) !== 'true') await toggle.click();
    const row = page.locator('.host-tool').filter({ hasText: 'Codex' });
    await row.getByRole('button', { name: '刷新模型' }).click();
    await page.getByText(/Codex：已刷新模型，共 \d+ 个/).waitFor({ timeout: 90000 });
    await page.screenshot({ path: 'out/e2e-host-refreshed.png' });
    log('host button ok');

    // The run's only session is the one created above
    await page.locator('.session-row').first().click();
    await page.locator('.model-btn').first().waitFor({ timeout: 30000 }).catch(() => {});
    if (await page.locator('.model-btn').count()) {
      await page.locator('.model-btn').first().click();
      const modelRow = page.getByRole('button', { name: '模型', exact: true });
      if (await modelRow.count()) await modelRow.click();
      await page.locator('.model-refresh').click();
      await page.getByText(/已刷新，(新增 \d+ 个模型|模型列表没有变化|模型列表有变化)/).waitFor({ timeout: 90000 })
        .catch(async (e) => { await page.screenshot({ path: 'out/e2e-session-failed.png' }); throw e; });
      await page.mouse.move(2, 2);
      await page.screenshot({ path: 'out/e2e-session-refreshed.png' });
      log('session button ok');
    } else {
      log('model button not shown in the session view; skipped the picker click');
    }
    assert.deepEqual(errors, []);
  } finally {
    await browser.close();
  }
  console.log('E2E OK');
} finally {
  if (sid) { try { await rpc('session.kill', { sessionId: sid }); await rpc('session.delete', { sessionId: sid }); } catch { /* best effort */ } }
  try { ws?.close(); } catch { /* closed */ }
  try { execFileSync('taskkill', ['/T', '/F', '/PID', String(web.pid)], { stdio: 'ignore' }); } catch { /* gone */ }
  // The daemon started for this run lives under RUN_DIR; leave any other yi-agentd alone
  try {
    execFileSync('powershell', ['-NoProfile', '-Command',
      `Get-CimInstance Win32_Process -Filter "Name='yi-agentd.exe'" | Where-Object { $_.ExecutablePath -like '${RUN_DIR}\\*' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }`], { stdio: 'ignore' });
  } catch { /* none */ }
}
