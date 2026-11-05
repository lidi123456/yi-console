// Real model + real native desktop validation. Run on Windows after windows-e2e.mjs.
import { chromium } from 'playwright';
import fs from 'node:fs/promises';
import assert from 'node:assert/strict';
import { win32 } from 'node:path';
const root = process.env.YI_CU_TEST_ROOT;
if (!root) throw new Error('Set YI_CU_TEST_ROOT to the isolated test directory');
await fs.mkdir('out', { recursive: true });
const report = { models: [], errors: [] };
const ws = new WebSocket('ws://127.0.0.1:8193/ws?token=cu-validation');
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
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const wait = async (check, timeout = 45000) => {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    const v = await check();
    if (v) return v;
    await sleep(250);
  }
  throw new Error('Timed out waiting for model state');
};
const parse = (s) => JSON.parse(s.replace(/^\uFEFF/, ''));
await wait(async () => {
  try {
    return await rpc('hello');
  } catch (e) {
    if (!/主机未连接|连接/.test(String(e))) throw e;
  }
});
const fixture = parse(await fs.readFile(`${root}/fixture-ready.json`, 'utf8'));
const target = (await rpc('computerUse.targets')).find((t) => t.pid === fixture.pid && t.windowId === fixture.windowId);
assert.ok(target);
const browser = await chromium.launch({ channel: 'chrome', headless: true });
try {
  for (const tool of (process.env.YI_CU_TEST_TOOLS || 'claude,codex').split(',')) {
    console.log('Starting real model desktop validation:', tool);
    const value = `Yi Console ${tool} verified`;
    const info = await rpc('session.create', {
      tool,
      cwd: root,
      mode: 'structured',
      title: `Computer Use · ${tool} 模型实测`,
      permissionMode: 'default',
      ...(tool === 'kiro' ? { model: process.env.YI_CU_TEST_MODEL || 'claude-opus-5.5' } : {}),
    });
    const sid = info.id;
    report.models.push({ tool, sessionId: sid, ok: false });
    const sub = () => rpc('session.subscribe', { sessionId: sid, sinceSeq: 0 });
    await wait(async () => ['idle', 'exited', 'error'].includes((await sub()).info.status));
    await rpc('session.computerUse', { sessionId: sid, action: 'enable', target });
    await wait(async () => ['idle', 'exited', 'error'].includes((await sub()).info.status));
    const imageException =
      tool === 'kiro'
        ? ' You may use Kiro built-in read/fs_read image mode only for the exact screenshot artifact files returned by yi_computer_use, because Kiro MCP does not render image blocks. Do not read any other files.'
        : '';
    const prompt = `Use only the yi_computer_use MCP tools to operate the window authorized in Yi Console. Read the window, use its current element token to set the Validation text field to exactly "${value}", invoke Apply test text, then take a fresh screenshot. Read the six-digit Check code displayed in the resulting screenshot, and report that exact code and the visible Verified text. Do not use shell, unrelated files, browser automation, other computer tools, or ask me to perform these steps. The code is generated randomly by the real button handler; it is not supplied in this prompt.${imageException}`;
    const baseline = (await sub()).info.lastSeq;
    const handledPermissions = new Set();
    await rpc('session.send', { sessionId: sid, text: prompt, images: [], queue: false });
    const finished = await wait(async () => {
      const d = await sub();
      for (const permission of d.events.filter(
        (e) => e.seq > baseline && e.kind === 'permission_request' && !handledPermissions.has(e.requestId),
      )) {
        // The tester may approve only the exact screenshot files emitted by this test session.
        const paths = [];
        const collect = (value, key = '') => {
          if (typeof value === 'string' && ['path', 'paths', 'file_path', 'filePath'].includes(key)) paths.push(value);
          else if (Array.isArray(value)) value.forEach((item) => collect(item, key));
          else if (value && typeof value === 'object') Object.entries(value).forEach(([k, v]) => collect(v, k));
        };
        collect(permission.input);
        const normalize = (path) => win32.normalize(path).toLowerCase();
        const allowed = new Set(
          d.events.flatMap((e) =>
            e.kind === 'computer_use' ? (e.images || []).map((image) => normalize(`${root}/profile/uploads/${sid}/${image.id}`)) : [],
          ),
        );
        const once = permission.options.find((option) => option.kind === 'allow_once');
        if (
          tool !== 'kiro' ||
          !['read', 'fs_read', 'read_file', 'read_image'].includes(permission.toolName) ||
          !paths.length ||
          !paths.every((path) => allowed.has(normalize(path))) ||
          !once
        )
          throw new Error(`${tool}: unexpected approval: ${permission.toolName} ${JSON.stringify(permission.input)}`);
        handledPermissions.add(permission.requestId);
        await rpc('session.permission', { sessionId: sid, requestId: permission.requestId, optionId: once.id });
      }
      if (d.events.some((e) => e.seq > baseline && e.kind === 'turn_end') || ['exited', 'error'].includes(d.info.status)) return d;
    }, 240000);
    const proof = parse(await fs.readFile(`${root}/fixture-result.json`, 'utf8'));
    const answer = finished.events
      .filter((e) => e.kind === 'assistant_text')
      .map((e) => e.text)
      .join('\n');
    const actions = finished.events.filter((e) => e.kind === 'computer_use' && e.status !== 'control');
    const providerErrors = finished.events.filter((e) => e.kind === 'error').map((e) => e.message);
    const record = {
      tool,
      sessionId: sid,
      model: finished.info.model,
      actionCount: actions.length,
      imageCount: actions.reduce((n, e) => n + (e.images?.length || 0), 0),
      nativeValue: proof.value,
      expectedCode: proof.code,
      answer,
      providerErrors,
      approvedScreenshotReads: handledPermissions.size,
    };
    Object.assign(report.models[report.models.length - 1], record);
    if (providerErrors.length) throw new Error(`${tool}: ${providerErrors.join('; ')}`);
    assert.equal(proof.value, value, `${tool} must perform actual native input/click`);
    assert.ok(actions.some((e) => e.tool === 'set_value' || e.tool === 'type_text'));
    assert.ok(actions.some((e) => e.tool === 'click'));
    assert.ok(record.imageCount > 0, `${tool} must produce real screenshot artifacts`);
    assert.ok(answer.includes(proof.code), `${tool} must read the random code from the fresh screenshot`);
    const context = await browser.newContext({ viewport: { width: 1440, height: 1050 } });
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
    await page.goto('http://127.0.0.1:8193/?token=cu-validation');
    await page.locator('.computer-use-images img').last().waitFor();
    await page.locator('.computer-use-images img').last().scrollIntoViewIfNeeded();
    await page.waitForFunction(() =>
      [...document.querySelectorAll('.computer-use-images img')].every((img) => img.complete && img.naturalWidth > 0),
    );
    await page.screenshot({ path: `out/${tool}-model-native.png`, fullPage: true });
    await rpc('session.computerUse', { sessionId: sid, action: 'stop' });
    await context.close();
    report.models[report.models.length - 1].ok = true;
    console.log(`PASS ${tool}: real native button callback and random screenshot code verified`);
  }
  await fs.writeFile('out/model-report.json', JSON.stringify({ ...report, ok: true }, null, 2));
} catch (e) {
  await fs.writeFile('out/model-report.json', JSON.stringify({ ...report, ok: false, error: String(e) }, null, 2));
  throw e;
} finally {
  for (const model of report.models) await rpc('session.computerUse', { sessionId: model.sessionId, action: 'stop' }).catch(() => {});
  await browser.close();
  ws.close();
}
