// Real model + real browser validation of yi_browser, run on Windows against an isolated yi-web / yi-agentd.
// A local fixture records what the page really received; the model must fill, select, check, click, accept a
// confirm dialog and read a random order code that exists only on the page. Meanwhile the app is open on the
// session: the AI browser tab must open by itself and show the page live while the AI works (out/<tool>-live-N.png).
// Browsers stay on afterwards for windows-shots.mjs, which turns them off.
//   YI_BROWSER_TEST_ROOT=<isolated dir> [YI_BROWSER_TEST_TOOLS=claude,codex,kiro] node windows-model-e2e.mjs
import fs from 'node:fs/promises';
import http from 'node:http';
import assert from 'node:assert/strict';
import { win32 } from 'node:path';
import { chromium } from 'playwright';

const root = process.env.YI_BROWSER_TEST_ROOT;
if (!root) throw new Error('Set YI_BROWSER_TEST_ROOT to the isolated test directory');
const web = process.env.YI_BROWSER_TEST_WEB || 'http://127.0.0.1:8194';
const token = process.env.YI_BROWSER_TEST_TOKEN || 'browser-e2e';
await fs.mkdir('out', { recursive: true });
const report = { models: [], errors: [] };

// Fixture: the order form. The code is made on the server when the order arrives, so only the page shows it
const orders = new Map();
// What a person did in the live view, as the page reported it
const person = new Map();
const personPage = (tool) => `<!doctype html><html><head><meta charset="utf-8"><title>Person check</title></head><body style="margin:0;font:15px system-ui">
<button id="b" style="position:fixed;left:20px;top:20px;width:240px;height:60px" onclick="fetch('/person?tool=${tool}&ev=click',{method:'POST'});this.textContent='clicked by a person'">Person button</button>
<input id="t" style="position:fixed;left:20px;top:110px;width:240px;height:30px" placeholder="type here" oninput="fetch('/person?tool=${tool}&ev=type&v='+encodeURIComponent(this.value),{method:'POST'})">
</body></html>`;
const page = (tool) => `<!doctype html><html><head><meta charset="utf-8"><title>Fixture shop</title>
<style>body{font:15px system-ui;margin:40px;max-width:560px}label{display:block;margin:12px 0}#cookie{position:fixed;left:0;right:0;bottom:0;background:#fde68a;padding:14px}
#code{font:bold 28px monospace;color:#047857}</style></head><body>
<h1>Order a test item</h1>
<form id="f" onsubmit="return false">
<label>Name <input id="name" required></label>
<label>Quantity <select id="qty"><option>1</option><option>2</option><option>3</option><option>4</option></select></label>
<label><input type="checkbox" id="agree"> I agree to the test terms</label>
<button id="submit">Submit order</button>
</form>
<p id="status">No order yet.</p><div id="code"></div>
<div id="cookie">We use cookies. <button onclick="this.parentNode.remove()">Accept cookies</button></div>
<script>
document.getElementById('submit').onclick = async () => {
  const name = document.getElementById('name').value, qty = document.getElementById('qty').value, agree = document.getElementById('agree').checked;
  if (!agree) { document.getElementById('status').textContent = 'Please agree to the terms first.'; console.warn('terms not accepted'); return; }
  if (!confirm('Place the order for ' + name + ' (' + qty + ')?')) { document.getElementById('status').textContent = 'Order cancelled.'; return; }
  const r = await fetch('/order?tool=${tool}', {method:'POST', headers:{'content-type':'application/json'}, body: JSON.stringify({name, qty, agree})});
  const v = await r.json();
  document.getElementById('status').textContent = 'Order placed for ' + v.name + ', quantity ' + v.qty + '.';
  document.getElementById('code').textContent = 'Order code: ' + v.code;
  console.log('order placed', v.code);
};
</script></body></html>`;
const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://x');
  const tool = url.searchParams.get('tool') || url.pathname.slice(1) || 'unknown';
  if (req.method === 'POST' && url.pathname === '/person') {
    const p = person.get(tool) || {};
    if (url.searchParams.get('ev') === 'click') p.clicked = true;
    else p.typed = url.searchParams.get('v');
    person.set(tool, p);
    res.writeHead(204);
    return res.end();
  }
  if (url.pathname === '/person') {
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
    return res.end(personPage(tool));
  }
  if (req.method === 'POST' && url.pathname === '/order') {
    let body = '';
    for await (const c of req) body += c;
    const v = JSON.parse(body);
    const code = String(Math.floor(100000 + Math.random() * 900000));
    orders.set(tool, { ...v, code });
    res.writeHead(200, { 'content-type': 'application/json' });
    return res.end(JSON.stringify({ ...v, code }));
  }
  res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
  res.end(page(tool));
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const port = server.address().port;

const ws = new WebSocket(`${web.replace('http', 'ws')}/ws?token=${token}`);
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
    await sleep(500);
  }
  throw new Error('Timed out waiting for model state');
};
await wait(async () => {
  try {
    return await rpc('hello');
  } catch (e) {
    if (!/主机未连接|连接/.test(String(e))) throw e;
  }
});
const app = await chromium.launch({ channel: 'chrome', headless: true });
/** The app on a session page, as a person would have it open while the AI works */
const openApp = async (sid) => {
  const ctx = await app.newContext({ viewport: { width: 1440, height: 1000 } });
  await ctx.addInitScript(
    ({ host, sid }) => {
      localStorage.setItem('ui-lang', JSON.stringify('zh'));
      localStorage.setItem('ui-theme', JSON.stringify('light'));
      localStorage.setItem('yi-console-view', JSON.stringify({ kind: 'session', hostId: host, sessionId: sid }));
    },
    { host, sid },
  );
  const page = await ctx.newPage();
  page.errors = [];
  page.on('pageerror', (e) => page.errors.push(e.message));
  await page.goto(`${web}/?token=${token}`);
  await page.getByText('已开启 AI 浏览器').first().waitFor({ timeout: 30000 });
  return { ctx, page };
};
const status = await rpc('browserUse.status', {});
console.log('browser on host:', status.browserPath);
assert.ok(status.browserPath, 'no Chrome / Edge on this host');

try {
  // Entries are `tool` or `tool=<accountId>` (a configured account of that tool); `tag` names files and fixture paths
  const entries = (process.env.YI_BROWSER_TEST_TOOLS || 'claude,codex,kiro').split(',').map((x, i) => {
    const [tool, accountId] = x.split('=');
    return { tool, accountId, tag: accountId ? `${tool}-${i + 1}` : tool };
  });
  for (const { tool: kind, accountId, tag: tool } of entries) {
   try {
    console.log('== real model browser validation:', tool);
    const name = `Yi Console ${tool} 测试`;
    const info = await rpc('session.create', {
      tool: kind,
      ...(accountId ? { accountId } : {}),
      cwd: root,
      mode: 'structured',
      title: `AI 浏览器 · ${tool} 实测`,
      permissionMode: 'default',
      ...(kind === 'kiro' ? { model: process.env.YI_BROWSER_TEST_MODEL || 'claude-opus-5.5' } : {}),
    });
    const sid = info.id;
    report.models.push({ tool, kind, accountId, sessionId: sid, ok: false });
    const sub = () => rpc('session.subscribe', { sessionId: sid, sinceSeq: 0 });
    await wait(async () => ['idle', 'exited', 'error'].includes((await sub()).info.status), 90000);
    await rpc('session.browserUse', { sessionId: sid, action: 'enable' });
    await wait(async () => ['idle', 'exited', 'error'].includes((await sub()).info.status), 90000);
    const kiroNote =
      kind === 'kiro'
        ? ' Kiro cannot see MCP images: you may read only the exact screenshot file path that browser_screenshot returns, with your built-in image reader. Do not read other files.'
        : '';
    const prompt =
      `Use only the yi_browser MCP tools (no shell, no other tools). Open http://127.0.0.1:${port}/${tool} . ` +
      `Fill the Name field with exactly "${name}", choose quantity 3, tick the terms checkbox, then click "Submit order" and accept the confirmation dialog. ` +
      `A cookie banner may cover parts of the page; dismiss it if it gets in the way. ` +
      `Then take a screenshot of the page and tell me the order code shown on it. The code is random and only on the page.${kiroNote}`;
    const baseline = (await sub()).info.lastSeq;
    const handled = new Set();
    const approvals = [];
    const { ctx, page: view } = await openApp(sid);
    const live = [];
    let lastShot = 0;
    const started = Date.now();
    await rpc('session.send', { sessionId: sid, text: prompt, images: [], queue: false });
    const finished = await wait(async () => {
      const d = await sub();
      // While the AI acts: the live view should be open on its own and drawing the page
      const mine = d.events.filter((e) => e.seq > baseline && e.kind === 'browser_use');
      const acted = mine.filter((e) => e.status === 'completed').length;
      // A shot while a call runs, once the page is on screen: glowing frame, stop button, status bar
      // (the frame stays lit 1.5 s after each step, so a poll between fast steps still sees it)
      if (acted >= 2 && !live.some((x) => x.acting)) {
        const acting = await view.locator('.aiweb-stage.glow .aiweb-canvas').isVisible().catch(() => false);
        if (acting) {
          const file = `out/${tool}-acting.png`;
          await view.screenshot({ path: file });
          live.push({
            file,
            acting: true,
            canvas: (await view.locator('.aiweb-stage:not(.hidden) .aiweb-canvas').count()) > 0,
            glow: await view.locator('.aiweb-stage.glow').isVisible(),
            stop: await view.locator('.aiweb-stop').isVisible(),
            status: await view.locator('.aiweb-status').innerText(),
          });
        }
      }
      if (acted > live.filter((x) => !x.acting).length && Date.now() - lastShot > 2500 && live.length < 7) {
        lastShot = Date.now();
        const canvas = await view.locator('.aiweb-stage:not(.hidden) .aiweb-canvas').count();
        const cursor = await view.locator('.aiweb-cursor svg').isVisible().catch(() => false);
        const status = (await view.locator('.aiweb-status').innerText().catch(() => '')).trim();
        const file = `out/${tool}-live-${live.length + 1}.png`;
        await view.screenshot({ path: file });
        live.push({ file, after: acted, canvas: canvas > 0, cursor, status });
      }
      for (const p of d.events.filter((e) => e.seq > baseline && e.kind === 'permission_request' && !handled.has(e.requestId))) {
        // Only Kiro reading the exact screenshot artifacts of this session may be approved
        const paths = [];
        const collect = (value, key = '') => {
          if (typeof value === 'string' && ['path', 'paths', 'file_path', 'filePath'].includes(key)) paths.push(value);
          else if (Array.isArray(value)) value.forEach((item) => collect(item, key));
          else if (value && typeof value === 'object') Object.entries(value).forEach(([k, v]) => collect(v, k));
        };
        collect(p.input);
        const norm = (x) => win32.normalize(x).toLowerCase();
        const allowed = new Set(
          d.events.flatMap((e) => (e.kind === 'browser_use' ? (e.images || []).map((i) => norm(`${root}/home/uploads/${sid}/${i.id}`)) : [])),
        );
        const once = p.options.find((o) => o.kind === 'allow_once');
        // yi_browser calls are answered by the adapter while the browser is on; only Kiro reading the saved
        // screenshot may reach the user
        const screenshotRead = kind === 'kiro' && paths.length && paths.every((x) => allowed.has(norm(x)));
        if (!once || !screenshotRead) throw new Error(`${tool}: unexpected approval: ${p.toolName} ${p.title || ''} ${JSON.stringify(p.input)}`);
        handled.add(p.requestId);
        approvals.push(`${p.toolName} ${p.title || ''}`.trim());
        await rpc('session.permission', { sessionId: sid, requestId: p.requestId, optionId: once.id });
      }
      if (d.events.some((e) => e.seq > baseline && e.kind === 'turn_end') || ['exited', 'error'].includes(d.info.status)) return d;
    }, 300000);
    const answer = finished.events.filter((e) => e.kind === 'assistant_text').map((e) => e.text).join('\n');
    const actions = finished.events.filter((e) => e.kind === 'browser_use' && e.status !== 'control' && e.status !== 'running');
    const order = orders.get(tool);
    const record = {
      tool,
      sessionId: sid,
      model: finished.info.model,
      seconds: Math.round((Date.now() - started) / 1000),
      tools: actions.map((e) => `${e.tool}${e.message ? `(${e.message})` : ''}:${e.status}`),
      images: actions.reduce((n, e) => n + (e.images?.length || 0), 0),
      order,
      answer,
      errors: finished.events.filter((e) => e.kind === 'error').map((e) => e.message),
      approvals,
      live,
      appErrors: view.errors,
    };
    Object.assign(report.models[report.models.length - 1], record);
    console.log(JSON.stringify(record, null, 2));
    assert.ok(order, `${tool}: the page never placed the order`);
    assert.equal(order.name, name, `${tool}: name typed into the page`);
    assert.equal(order.qty, '3');
    assert.equal(order.agree, true);
    assert.ok(record.images > 0, `${tool}: a real screenshot artifact`);
    assert.ok(answer.includes(order.code), `${tool}: the model must read the random code from the page`);
    assert.ok(live.some((x) => x.canvas), `${tool}: the AI browser tab opened by itself and showed the page while the AI worked`);
    assert.ok(live.some((x) => x.acting && x.glow && x.stop), `${tool}: the page glows with a stop button while the AI acts`);
    assert.ok(live.some((x) => x.cursor), `${tool}: the AI's cursor shows on the picture after a click`);

    // A person takes over in the same view: the address bar opens a page in the AI's browser, then a click and
    // typing (Latin keys and committed IME text) on the picture reach that page
    const target = `http://127.0.0.1:${port}/person?tool=${tool}`;
    await view.locator('.aiweb .wb-addr input').fill(target);
    await view.locator('.aiweb .wb-addr input').press('Enter');
    await view.waitForFunction((u) => document.querySelector('.aiweb .wb-addr input')?.value === u, target, { timeout: 20000 });
    await sleep(1500);
    const box = await view.locator('.aiweb-canvas').boundingBox();
    // The page's CSS width over the drawn width (the frame may be scaled down)
    const k = box.width / 1280;
    await view.mouse.click(box.x + 140 * k, box.y + 50 * k);
    await view.mouse.click(box.x + 140 * k, box.y + 125 * k);
    await view.keyboard.type('ok ');
    await view.locator('.aiweb-keys').dispatchEvent('compositionstart');
    await view.locator('.aiweb-keys').evaluate((el) => el.dispatchEvent(new CompositionEvent('compositionend', { data: '你好', bubbles: true })));
    await wait(async () => person.get(tool)?.clicked && person.get(tool)?.typed === 'ok 你好', 20000);
    await sleep(1200);
    await view.screenshot({ path: `out/${tool}-takeover.png` });
    report.models[report.models.length - 1].takeover = person.get(tool);
    await ctx.close();
    report.models[report.models.length - 1].ok = true;
    console.log(`PASS ${tool}: order ${order.code} placed through the real page and read back by the model`);
   } catch (e) {
    console.log(`FAIL ${tool}: ${e.message}`);
    const m = report.models.find((x) => x.tool === tool);
    if (m) m.error = String(e.message);
    else report.models.push({ tool, ok: false, error: String(e.message) });
   }
  }
  if (report.models.some((m) => !m.ok)) process.exitCode = 1;
  await fs.writeFile('out/report.json', JSON.stringify({ ...report, ok: report.models.every((m) => m.ok) }, null, 2));
} catch (e) {
  await fs.writeFile('out/report.json', JSON.stringify({ ...report, ok: false, error: String(e) }, null, 2));
  throw e;
} finally {
  await app.close();
  ws.close();
  // Browsers stay on for windows-shots.mjs, which turns them off; their keep-alive connections would hold the
  // fixture server (and this process) open
  server.closeAllConnections();
  server.close();
  setTimeout(() => process.exit(), 1000).unref();
}
