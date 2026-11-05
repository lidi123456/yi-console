// Run with windows-remote/pw-run.sh after the isolated interactive fixture is ready.
// Real daemon, real MCP proxy, real native window, and the normal application frontend; no mocks.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import { spawn } from 'node:child_process';
import readline from 'node:readline';

const root = process.env.YI_CU_TEST_ROOT;
if (!root) throw new Error('Set YI_CU_TEST_ROOT to the isolated test directory');
const profile = `${root}/profile`;
const base = 'http://127.0.0.1:8193';
await fs.mkdir('out', { recursive: true });
const report = { checks: [], sessions: [], errors: [] };
const parse = (text) => JSON.parse(text.replace(/^\uFEFF/, ''));
const waitFor = async (check, timeout = 30000) => {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    const value = await check();
    if (value) return value;
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error('Timed out waiting for test state');
};
const ws = new WebSocket(`${base.replace('http:', 'ws:')}/ws?token=cu-validation`);
await new Promise((resolve, reject) => {
  ws.addEventListener('open', resolve, { once: true });
  ws.addEventListener('error', reject, { once: true });
});
let next = 0;
const pending = new Map();
ws.addEventListener('message', (e) => {
  const v = JSON.parse(e.data);
  const p = pending.get(v.id);
  if (p) {
    pending.delete(v.id);
    clearTimeout(p.timer);
    v.error ? p.reject(new Error(String(v.error))) : p.resolve(v.result);
  }
});
const hub = (cmd, args = {}) =>
  new Promise((resolve, reject) => {
    const id = ++next;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`RPC timed out: ${cmd} ${args.method || ''}`));
    }, 90000);
    pending.set(id, { resolve, reject, timer });
    ws.send(JSON.stringify({ id, cmd, args }));
  });
const hosts = await hub('hosts.list');
const local = hosts.hosts.find((h) => h.config.kind === 'local');
const host = local.config.id;
if (!['connected', 'connecting'].includes(local.status.status)) await hub('hosts.connect', { id: host });
const rpc = (method, params = {}) => hub('rpc', { id: host, method, params });
await waitFor(async () => {
  try {
    return await rpc('hello');
  } catch (e) {
    if (!/主机未连接|连接/.test(String(e))) throw e;
  }
}, 45000);
const sidOf = (s) => s.id;
const subscribe = (sessionId) => rpc('session.subscribe', { sessionId, sinceSeq: 0 });
const desktop = (sessionId, action, target) => rpc('session.computerUse', { sessionId, action, ...(target ? { target } : {}) });
const structured = (r) =>
  r.structuredContent ??
  (() => {
    for (const b of r.content ?? []) {
      try {
        return JSON.parse(b.text);
      } catch {}
    }
    return r;
  })();
const children = [];
function proxy(sessionId) {
  const child = spawn(
    `${profile}/bin/yi-agentd.exe`,
    ['--home', profile, 'computer-use-mcp', '--session', sessionId, '--ticket-file', `${profile}/computer-use/${sessionId}.json`],
    { windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] },
  );
  children.push(child);
  let id = 0;
  const pending = new Map();
  let stderr = '';
  child.stderr.on('data', (b) => (stderr += b));
  child.on('exit', () => {
    for (const p of pending.values()) p.reject(new Error('Proxy exited: ' + stderr));
  });
  readline.createInterface({ input: child.stdout }).on('line', (line) => {
    const v = JSON.parse(line);
    const p = pending.get(v.id);
    if (p) {
      pending.delete(v.id);
      clearTimeout(p.timer);
      v.error ? p.reject(new Error(v.error.message)) : p.resolve(v.result);
    }
  });
  return (method, params = {}) =>
    new Promise((resolve, reject) => {
      const n = ++id;
      const timer = setTimeout(() => {
        pending.delete(n);
        reject(new Error(`MCP timed out: ${method}`));
      }, 60000);
      pending.set(n, { resolve, reject, timer });
      child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: n, method, params }) + '\n');
    });
}
let browser;
try {
  const fixture = parse(await fs.readFile(`${root}/fixture-ready.json`, 'utf8'));
  const targets = await rpc('computerUse.targets');
  const target = targets.find((t) => t.windowId === fixture.windowId && t.pid === fixture.pid);
  assert.ok(target, 'interactive fixture must be visible to the embedded driver');
  console.log('PASS real desktop window discovered', target.pid, target.windowId);
  const session = await rpc('session.create', {
    tool: 'claude',
    cwd: root,
    mode: 'structured',
    title: 'Computer Use · Windows 实机验证',
    permissionMode: 'bypass',
  });
  const sid = sidOf(session);
  report.sessions.push(sid);
  await subscribe(sid);
  await waitFor(async () => {
    const d = await subscribe(sid);
    return ['idle', 'exited', 'error'].includes(d.info.status) && d;
  }, 45000);
  browser = await chromium.launch({ channel: 'chrome', headless: true });
  const context = await browser.newContext({ viewport: { width: 1440, height: 1050 }, acceptDownloads: true });
  await context.addInitScript(
    ({ host, sid }) => {
      localStorage.setItem('ui-lang', JSON.stringify('zh'));
      localStorage.setItem('ui-theme', JSON.stringify('light'));
      localStorage.setItem('yi-console-view', JSON.stringify({ kind: 'session', hostId: host, sessionId: sid }));
    },
    { host, sid },
  );
  const page = await context.newPage();
  page.on('pageerror', (e) => report.errors.push(e.message));
  await page.goto(`${base}/?token=cu-validation`);
  await page.getByRole('button', { name: '操作电脑', exact: true }).waitFor();
  await page.getByRole('button', { name: '操作电脑', exact: true }).click();
  await page.getByRole('button', { name: '刷新窗口', exact: true }).click();
  await page.getByLabel('目标窗口', { exact: true }).selectOption(String(target.windowId));
  await page.screenshot({ path: 'out/01-window-selection.png', fullPage: true });
  await page.getByRole('button', { name: '开启操作电脑', exact: true }).click();
  await page.getByRole('dialog').waitFor({ state: 'hidden' });
  await waitFor(async () => (await rpc('computerUse.status', { sessionId: sid })).enabled);
  report.checks.push('bundled driver and real-window selection through the actual UI');
  console.log('PASS actual UI window selection and authorization');
  const call = proxy(sid);
  await call('initialize', { protocolVersion: '2025-06-18', clientInfo: { name: 'yi-e2e', version: '1' }, capabilities: {} });
  const tools = await call('tools/list');
  assert.ok(tools.tools.some((t) => t.name === 'get_window_state'));
  const readSchema = tools.tools.find((t) => t.name === 'get_window_state').inputSchema;
  assert.ok(!readSchema.properties.pid && !readSchema.properties.window_id);
  assert.ok(!(readSchema.required || []).some((name) => ['pid', 'window_id'].includes(name)));
  assert.ok(
    !tools.tools.some((t) => ['get_desktop_state', 'launch_app', 'kill_app', 'clipboard_read', 'clipboard_write'].includes(t.name)),
  );
  const read = () => call('tools/call', { name: 'get_window_state', arguments: { include_screenshot: true } });
  let snapshot = await read();
  assert.ok(
    snapshot.content.some((b) => b.type === 'image' && b.data.length > 100),
    'the model-facing MCP result must contain the actual image',
  );
  const edit = structured(snapshot).elements.find((e) => e.role === 'Edit' && e.label === 'Validation text');
  assert.ok(edit?.element_token);
  const value = 'Yi Console Computer Use verified';
  const set = await call('tools/call', {
    name: 'set_value',
    arguments: { pid: target.pid, window_id: target.windowId, element_token: edit.element_token, value },
  });
  assert.notEqual(set.isError, true, JSON.stringify(set));
  snapshot = await read();
  const button = structured(snapshot).elements.find((e) => e.label === 'Apply test text');
  const click = await call('tools/call', {
    name: 'click',
    arguments: { pid: target.pid, window_id: target.windowId, element_token: button.element_token },
  });
  assert.notEqual(click.isError, true, JSON.stringify(click));
  const proof = await waitFor(async () => {
    try {
      const p = parse(await fs.readFile(`${root}/fixture-result.json`, 'utf8'));
      return p.value === value && p;
    } catch {
      return false;
    }
  });
  assert.equal(proof.pid, target.pid);
  snapshot = await read();
  assert.ok(
    !JSON.stringify(structured(snapshot)).includes('Check code: ' + proof.code),
    'visual code must not leak into the accessibility snapshot',
  );
  for (const b of snapshot.content ?? [])
    if (b.type === 'image') await fs.writeFile('out/02-native-window.png', Buffer.from(b.data, 'base64'));
  await fs.writeFile('out/native-proof.json', JSON.stringify(proof, null, 2));
  const history = await subscribe(sid);
  assert.ok(history.events.some((e) => e.kind === 'computer_use' && e.images?.length));
  report.checks.push('real MCP image, snapshot element input, native button callback, persisted screenshot references');
  console.log('PASS real native input/button callback and screenshot persistence');
  await page.locator('.computer-use-action .msg-img img').last().waitFor();
  await page.locator('.computer-use-action .msg-img img').last().scrollIntoViewIfNeeded();
  await page.waitForFunction(() =>
    [...document.querySelectorAll('.computer-use-images img')].every((img) => img.complete && img.naturalWidth > 0),
  );
  await page.screenshot({ path: 'out/03-app-native-result.png', fullPage: true });
  await desktop(sid, 'pause');
  const denied = await read();
  assert.equal(denied.isError, true);
  const second = await rpc('session.create', {
    tool: 'codex',
    cwd: root,
    mode: 'structured',
    title: 'Computer Use · 控制权验证',
    permissionMode: 'bypass',
  });
  report.sessions.push(second.id);
  await waitFor(async () => ['idle', 'exited', 'error'].includes((await subscribe(second.id)).info.status), 45000);
  await desktop(second.id, 'enable', target);
  await assert.rejects(() => desktop(sid, 'resume'), /使用|占用/);
  await desktop(second.id, 'stop');
  await desktop(sid, 'resume');
  await read();
  const mismatch = await call('tools/call', { name: 'get_window_state', arguments: { pid: target.pid, window_id: target.windowId + 1 } });
  assert.equal(mismatch.isError, true);
  await desktop(sid, 'stop');
  const stale = await read();
  assert.equal(stale.isError, true);
  report.checks.push('pause, cross-session desktop lease, target rejection, resume and stopped-proxy revocation');
  await page.reload();
  await page.getByRole('button', { name: '操作电脑', exact: true }).waitFor();
  await page.screenshot({ path: 'out/04-app-stopped.png', fullPage: true });
  await fs.writeFile('out/report.json', JSON.stringify({ ...report, ok: true }, null, 2));
  console.log('PASS native Computer Use:', report.checks);
} catch (e) {
  await fs.writeFile('out/report.json', JSON.stringify({ ...report, ok: false, error: String(e) }, null, 2));
  if (browser)
    for (const c of browser.contexts())
      for (const p of c.pages()) await p.screenshot({ path: 'out/failure.png', fullPage: true }).catch(() => {});
  throw e;
} finally {
  for (const sessionId of report.sessions) await desktop(sessionId, 'stop').catch(() => {});
  for (const child of children) child.kill();
  if (browser) await browser.close();
  ws.close();
}
