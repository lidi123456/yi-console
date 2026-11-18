// Chat and a native web tab side by side in the desktop app (Windows): while the AI replies in the chat, a
// person clicks around in the web tab and types in the composer. Screen shots of the real window as evidence.
// Same setup as windows-webtab-e2e.mjs (debug app, test identifier, own YI_CONSOLE_HOME, WebView2 CDP).
//
//   YI_WT_APP=<yi-console.exe> YI_WT_OUT=<dir> [YI_WT_TOOL=kiro] [YI_WT_MODEL=claude-opus-5.5] node windows-webtab-chat.mjs
import { chromium } from 'playwright';
import { execFile, spawn } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';

const OUT = process.env.YI_WT_OUT || path.resolve('out');
const HOME = process.env.YI_WT_HOME || path.join(OUT, 'home');
const TOOL = process.env.YI_WT_TOOL || 'kiro';
const MODEL = process.env.YI_WT_MODEL || (TOOL === 'kiro' ? 'claude-opus-5.5' : '');
const CDP = 9339;
fs.mkdirSync(OUT, { recursive: true });
const work = path.join(HOME, 'work');
fs.mkdirSync(work, { recursive: true });
const report = { steps: [] };
const step = (name, ok, detail = '') => {
  report.steps.push({ name, ok, detail });
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${detail ? ` · ${detail}` : ''}`);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (f, ms = 15000, what = 'condition') => {
  const end = Date.now() + ms;
  let last;
  while (Date.now() < end) {
    try {
      last = await f();
      if (last) return last;
    } catch (e) {
      last = e;
    }
    await sleep(200);
  }
  throw new Error(`timed out: ${what} (${last instanceof Error ? last.message : last})`);
};

const html = (title, body) =>
  `<!doctype html><meta charset="utf-8"><title>${title}</title><body style="font:16px sans-serif;padding:20px;background:#f4f8ff"><h2>${title}</h2>${body}</body>`;
let clicks = 0;
const site = http.createServer((req, res) => {
  const u = new URL(req.url, 'http://x');
  res.setHeader('Content-Type', 'text/html; charset=utf-8');
  res.setHeader('X-Frame-Options', 'DENY');
  if (u.pathname === '/count') clicks++;
  res.end(
    u.pathname === '/'
      ? html(
          'Todo demo (X-Frame-Options: DENY)',
          `<p><input id="t" placeholder="New todo" style="font-size:16px"> <button id="add">Add</button></p><ul id="l"></ul>
           <p><a id="next" href="/next">Go to the next page</a></p>
           <script>add.onclick=()=>{const li=document.createElement('li');li.textContent=t.value||'(empty)';l.appendChild(li);t.value='';fetch('/count')}</script>`,
        )
      : html('Next page', `<p>Navigated while the AI was replying.</p><p><a href="/">back home</a></p>`),
  );
});
await new Promise((r) => site.listen(0, '127.0.0.1', r));
const SITE = `http://127.0.0.1:${site.address().port}`;

const app = spawn(process.env.YI_WT_APP, [], {
  env: { ...process.env, YI_CONSOLE_HOME: HOME, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${CDP}` },
  stdio: ['ignore', fs.openSync(path.join(OUT, 'chat-app.log'), 'w'), fs.openSync(path.join(OUT, 'chat-app.err.log'), 'w')],
});
const shot = (name) =>
  new Promise((resolve) =>
    execFile(
      'powershell',
      ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', path.join(path.dirname(new URL(import.meta.url).pathname.slice(1)), 'windows-capture.ps1'), String(app.pid), path.join(OUT, `${name}.png`)],
      () => resolve(),
    ),
  );

let browser;
try {
  browser = await until(() => chromium.connectOverCDP(`http://127.0.0.1:${CDP}`), 60000, 'WebView2 CDP');
  const pages = () => browser.contexts().flatMap((c) => c.pages());
  const main = await until(() => pages().find((p) => /tauri\.localhost/.test(p.url())), 30000, 'main webview');
  const hub = (cmd, args = {}) => main.evaluate(([cmd, args]) => window.__TAURI_INTERNALS__.invoke('hub_call', { cmd, args }), [cmd, args]);
  const hosts = await until(() => hub('hosts.list'), 30000, 'hosts.list');
  const host = hosts.hosts.find((h) => h.config.kind === 'local').config.id;
  const rpc = (method, params = {}) => hub('rpc', { id: host, method, params });
  await until(async () => (await rpc('hello').catch(() => null)) && true, 60000, 'local daemon');
  const info = await rpc('session.create', { tool: TOOL, cwd: work, mode: 'structured', title: '对话 + 网页并排', permissionMode: 'default', ...(MODEL ? { model: MODEL } : {}) });
  const sid = info.id;
  await until(async () => ['idle', 'exited', 'error'].includes((await rpc('session.subscribe', { sessionId: sid, sinceSeq: 0 })).info.status), 90000, 'session ready');
  await main.evaluate(
    ({ host, sid, url, work }) => {
      localStorage.setItem('ui-lang', JSON.stringify('zh'));
      localStorage.setItem('ui-theme', JSON.stringify('light'));
      localStorage.setItem('yi-console-view', JSON.stringify({ kind: 'session', hostId: host, sessionId: sid }));
      localStorage.setItem(`file-panel-${host}::${sid}`, JSON.stringify(true));
      localStorage.setItem('file-panel-w', JSON.stringify(700));
      localStorage.setItem('wp-tree', JSON.stringify(false));
      localStorage.setItem(`wp-open-${host}/${work}`, JSON.stringify({ tabs: ['web:' + url], current: 'web:' + url }));
    },
    { host, sid, url: `${SITE}/`, work },
  );
  await main.reload();
  const web = await until(() => pages().find((p) => p.url().startsWith(SITE)), 30000, 'web tab');
  await web.waitForLoadState();
  step('web tab open next to the chat', true, web.url());

  // The AI replies in the chat...
  const box = main.locator('.composer-row textarea');
  await box.click();
  await box.fill('请用 10 行左右、每行一句，介绍一下 X-Frame-Options 和 CSP frame-ancestors 的区别。不要调用任何工具。');
  await box.press('Enter');
  await until(async () => (await main.locator('.chat-msg, .msg, [data-role=assistant]').count()) > 0 || (await main.getByText('X-Frame-Options').count()) > 1, 60000, 'reply starts').catch(() => {});
  await shot('chat-1-reply-and-page');
  // ...while the person uses the page
  await web.fill('#t', '一边对话一边操作网页');
  await web.click('#add');
  await web.fill('#t', '第二条');
  await web.click('#add');
  const items = await web.locator('#l li').count();
  step('person adds todos in the page during the reply', items === 2 && clicks === 2, `items ${items}`);
  await shot('chat-2-page-used-during-reply');
  // ...and types the next message in the composer
  await box.click();
  await box.type('下一条消息：网页和对话框可以同时用', { delay: 15 });
  step('composer takes typing with the web tab open', (await box.inputValue()).includes('同时用'));
  await shot('chat-3-typing-next-message');
  await web.click('#next');
  await until(async () => (await main.getByLabel('地址').inputValue()) === `${SITE}/next`, 10000, 'address').catch(() => {});
  step('page navigates, address bar follows', (await main.getByLabel('地址').inputValue()) === `${SITE}/next`);
  await until(async () => (await rpc('session.subscribe', { sessionId: sid, sinceSeq: 0 })).info.status === 'idle', 180000, 'reply done').catch(() => {});
  await sleep(800);
  await shot('chat-4-reply-done');
  const ev = await rpc('session.subscribe', { sessionId: sid, sinceSeq: 0 });
  const reply = (ev.events || []).filter((e) => e.kind === 'assistant_text').map((e) => e.text ?? '').join('\n');
  step('AI replied', reply.length > 20, reply.slice(0, 80).replace(/\n/g, ' '));
} catch (e) {
  step('run', false, String(e?.stack ?? e));
} finally {
  fs.writeFileSync(path.join(OUT, 'chat-report.json'), JSON.stringify(report, null, 2));
  await browser?.close().catch(() => {});
  app.kill();
  site.close();
  const failed = report.steps.filter((s) => !s.ok).length;
  console.log(failed ? `FAILED ${failed}` : 'ALL PASS');
  process.exit(failed ? 1 : 0);
}
