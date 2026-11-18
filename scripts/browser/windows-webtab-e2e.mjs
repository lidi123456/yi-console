// Native web tabs of the desktop app on Windows (app/src-tauri/src/webtab.rs, components/NativeWeb.tsx).
// Starts the debug app with WebView2 remote debugging, drives the main webview and the tab's child webview
// with Playwright over CDP, and takes screen shots of the real window (a CDP screenshot doesn't show the
// native child). Needs an interactive desktop session: run it from a scheduled task, not over plain ssh.
//
//   YI_WT_APP=<path to yi-console.exe> YI_WT_OUT=<dir> node windows-webtab-e2e.mjs
//
// The app runs with its own YI_CONSOLE_HOME and a test identifier (tauri build --config), so the user's own
// Yi Console, its data and its single-instance lock are left alone.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';

const OUT = process.env.YI_WT_OUT || path.resolve('out');
const APP = process.env.YI_WT_APP;
const HOME = process.env.YI_WT_HOME || path.join(OUT, 'home');
const CDP = 9339;
fs.mkdirSync(OUT, { recursive: true });
fs.mkdirSync(path.join(HOME, 'work'), { recursive: true });
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

// Fixture site: forbids framing, so an iframe would show a blank page
const page = (title, body) =>
  `<!doctype html><meta charset="utf-8"><title>${title}</title><body style="font:18px sans-serif;padding:24px;background:#fffbe6"><h1>${title}</h1>${body}</body>`;
const site = http.createServer((req, res) => {
  const u = new URL(req.url, 'http://x');
  res.setHeader('Content-Type', 'text/html; charset=utf-8');
  res.setHeader('X-Frame-Options', 'DENY');
  res.setHeader('Content-Security-Policy', "frame-ancestors 'none'");
  if (u.pathname === '/')
    res.end(
      page(
        'WT home',
        `<p>This page sends X-Frame-Options: DENY.</p>
        <p><a id="next" href="/next">next page</a></p>
        <p><a id="blank" href="/popup" target="_blank">open in new window</a></p>
        <p><button id="push" onclick="history.pushState({}, '', '/spa/route'); document.title = 'WT spa'">pushState</button></p>
        <p><a id="file" href="file:///C:/Windows/win.ini">file link</a> <a id="app" href="http://tauri.localhost/">app origin link</a></p>`,
      ),
    );
  else res.end(page(`WT ${u.pathname.slice(1).replace(/\//g, ' ')}`, `<p><a href="/">home</a></p>`));
});
await new Promise((r) => site.listen(0, '127.0.0.1', r));
const SITE = `http://127.0.0.1:${site.address().port}`;

const ps = (file, args) =>
  new Promise((resolve) =>
    execFile('powershell', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', path.join(path.dirname(new URL(import.meta.url).pathname.slice(1)), file), ...args.map(String)], (e, so, se) => {
      if (e) console.log(file, 'failed', se || e.message);
      resolve();
    }),
  );
/** Real OS click at CSS px of the main webview */
const osClick = (x, y) => ps('windows-click.ps1', [app.pid, x, y]);
const shot = (name) =>
  new Promise((resolve) => {
    execFile(
      'powershell',
      ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', path.join(path.dirname(new URL(import.meta.url).pathname.slice(1)), 'windows-capture.ps1'), String(app.pid), path.join(OUT, `${name}.png`)],
      (e, so, se) => {
        if (e) console.log('capture failed', name, se || e.message);
        resolve();
      },
    );
  });

const app = spawn(APP, [], {
  env: { ...process.env, YI_CONSOLE_HOME: HOME, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${CDP}` },
  stdio: ['ignore', fs.openSync(path.join(OUT, 'app.log'), 'w'), fs.openSync(path.join(OUT, 'app.err.log'), 'w')],
});
let browser;
try {
  browser = await until(() => chromium.connectOverCDP(`http://127.0.0.1:${CDP}`), 60000, 'WebView2 CDP');
  const pages = () => browser.contexts().flatMap((c) => c.pages());
  const main = await until(() => pages().find((p) => /tauri\.localhost/.test(p.url())), 30000, 'main webview');
  main.errors = [];
  main.on('pageerror', (e) => main.errors.push(e.message));
  const hub = (cmd, args = {}) => main.evaluate(([cmd, args]) => window.__TAURI_INTERNALS__.invoke('hub_call', { cmd, args }), [cmd, args]);
  const hosts = await until(() => hub('hosts.list'), 30000, 'hosts.list');
  const host = hosts.hosts.find((h) => h.config.kind === 'local').config.id;
  const rpc = (method, params = {}) => hub('rpc', { id: host, method, params });
  await until(async () => (await rpc('hello').catch(() => null)) && true, 60000, 'local daemon');
  const work = path.join(HOME, 'work');
  const info = await rpc('session.create', { tool: 'files', cwd: work, title: 'native web tab' });
  const sid = info.id;
  await main.evaluate(
    ({ host, sid, url }) => {
      localStorage.setItem('ui-lang', JSON.stringify('zh'));
      localStorage.setItem('ui-theme', JSON.stringify('light'));
      localStorage.setItem('yi-console-view', JSON.stringify({ kind: 'session', hostId: host, sessionId: sid }));
      localStorage.setItem(`wp-open-s-${host}/${sid}`, JSON.stringify({ tabs: ['web:' + url], current: 'web:' + url }));
    },
    { host, sid, url: `${SITE}/` },
  );
  await main.reload();
  const addr = main.getByLabel('地址');
  const child = await until(() => pages().find((p) => p.url().startsWith(SITE)), 30000, 'child webview');
  await child.waitForLoadState();
  step('page that forbids framing loads', (await child.title()) === 'WT home', await child.title());
  await until(async () => (await addr.inputValue()) === `${SITE}/`, 10000, 'address bar').catch(() => {});
  step('address bar shows the page', (await addr.inputValue()) === `${SITE}/`, await addr.inputValue());
  // Size of the child = size of the placeholder (logical px)
  const ph = await main.locator('.wb-native').boundingBox();
  const inner = await child.evaluate(() => [innerWidth, innerHeight]);
  step('child fills the placeholder', Math.abs(inner[0] - ph.width) <= 2 && Math.abs(inner[1] - ph.height) <= 2, `placeholder ${Math.round(ph.width)}x${Math.round(ph.height)}, page ${inner.join('x')}`);
  await sleep(500);
  await shot('webtab-1-home');

  // Remote page can't reach the app's commands
  const ipc = await child.evaluate(async () => {
    const t = window.__TAURI_INTERNALS__;
    if (!t) return 'no ipc object';
    try {
      await t.invoke('hub_call', { cmd: 'hosts.list', args: {} });
      return 'ALLOWED';
    } catch (e) {
      return `rejected: ${String(e).slice(0, 80)}`;
    }
  });
  step('page cannot call hub_call', ipc !== 'ALLOWED', ipc);

  await child.click('#next');
  await until(async () => (await addr.inputValue()) === `${SITE}/next`, 10000, 'address after link').catch(() => {});
  step('address follows a link', (await addr.inputValue()) === `${SITE}/next`, await addr.inputValue());
  await main.getByTitle('后退', { exact: true }).click();
  await until(async () => (await addr.inputValue()) === `${SITE}/`, 10000, 'back').catch(() => {});
  step('back button', (await addr.inputValue()) === `${SITE}/`, await addr.inputValue());

  await child.click('#push');
  await until(async () => (await addr.inputValue()) === `${SITE}/spa/route`, 10000, 'pushState').catch(() => {});
  step('address follows pushState', (await addr.inputValue()) === `${SITE}/spa/route`, await addr.inputValue());

  await main.getByTitle('后退', { exact: true }).click();
  await until(async () => child.url() === `${SITE}/`, 10000, 'back to home').catch(() => {});
  await child.evaluate(() => (window.__wtMark = 1));
  await child.click('#blank');
  const popupTab = await until(() => main.locator('[role=tab]').filter({ hasText: '/popup' }).count(), 10000, 'popup tab').catch(() => 0);
  step('target=_blank opens a new web tab', popupTab > 0, `tabs: ${(await main.locator('[role=tab]').allInnerTexts()).join(' | ')}`);
  await shot('webtab-2-popup-tab');
  // Back to the first tab
  await main.locator('[role=tab]').filter({ hasText: '127.0.0.1' }).filter({ hasNotText: 'popup' }).first().click();
  const home = await until(() => pages().find((p) => p.url() === `${SITE}/` || p.url() === `${SITE}/spa/route`), 15000, 'first tab webview');
  await until(async () => (await main.locator('.wb-native').getAttribute('data-shown')) === '1', 5000, 'first tab shown').catch(() => {});
  step('switching tabs keeps the page (no reload)', (await home.evaluate(() => window.__wtMark)) === 1);

  const before = home.url();
  await home.click('#file').catch(() => {});
  await sleep(800);
  await home.click('#app').catch(() => {});
  await sleep(800);
  step('file: and app-origin links are blocked', home.url() === before && !pages().some((p) => p.url().startsWith('http://tauri.localhost') && p !== main), home.url());
  await shot('webtab-3-blocked-toast');

  // Something of the app's own over part of the view: cut out of it, both sides live
  const shown = () => main.locator('.wb-native').getAttribute('data-shown');
  const ph2 = await main.locator('.wb-native').boundingBox();
  await main.evaluate((r) => {
    const d = document.createElement('div');
    d.id = 'wt-pop';
    d.style.cssText = `position:fixed;left:${r.x + r.width - 300}px;top:${r.y + 20}px;width:280px;height:170px;z-index:99999;background:#1f6feb;color:#fff;font:16px sans-serif;border-radius:10px;box-shadow:0 6px 20px rgba(0,0,0,.3);display:flex;align-items:center;justify-content:center;text-align:center`;
    d.textContent = 'App popup over the web tab: click me';
    d.onclick = () => ((window.__popClicks = (window.__popClicks || 0) + 1), (d.textContent = 'clicked ' + window.__popClicks));
    document.body.appendChild(d);
  }, ph2);
  const holed = await until(async () => (await shown()) === 'h', 5000, 'hole').catch(() => false);
  step('popup over part of the view: cut out, view stays', !!holed, await shown());
  await sleep(300);
  await osClick(ph2.x + ph2.width - 160, ph2.y + 105);
  await sleep(400);
  step('real click on the popup reaches the app', (await main.evaluate(() => window.__popClicks || 0)) === 1);
  // A real click on the page outside the popup: the page is live
  const nb = await home.locator('#next').boundingBox();
  await osClick(ph2.x + nb.x + 10, ph2.y + nb.y + nb.height / 2);
  await until(() => home.url() === `${SITE}/next`, 5000, 'page click').catch(() => {});
  step('real click on the page next to the popup works', home.url() === `${SITE}/next`, home.url());
  await shot('webtab-4-popup-and-page');
  await main.evaluate(() => document.getElementById('wt-pop').remove());
  const back = await until(async () => (await shown()) === '1', 5000, 'visible').catch(() => false);
  step('hole closes with the popup', !!back);

  // A modal over most of the view: a picture of the page under it
  await main.evaluate(() => {
    const bg = document.createElement('div');
    bg.id = 'wt-cover';
    bg.style.cssText = 'position:fixed;inset:0;z-index:99999;background:rgba(10,12,16,.45);display:flex;align-items:center;justify-content:center';
    bg.innerHTML = '<div style="background:#fff;padding:28px 36px;border-radius:12px;font:18px sans-serif">App dialog: the page behind is a picture</div>';
    document.body.appendChild(bg);
  });
  const still = await until(async () => (await shown()) === '0' && (await main.locator('.wb-still').count()) > 0, 6000, 'still').catch(() => false);
  step('modal: page shown as a picture under the backdrop', !!still);
  await sleep(300);
  await shot('webtab-5-modal');
  await main.evaluate(() => document.getElementById('wt-cover').remove());
  const live = await until(async () => (await shown()) === '1' && (await main.locator('.wb-still').count()) === 0, 5000, 'live').catch(() => false);
  step('live again after the modal', !!live);

  // The tab's real context menu
  await main.locator('[role=tab]').first().click({ button: 'right' });
  await sleep(600);
  step('tab context menu shows over the live view', (await shown()) === 'h', await shown());
  await shot('webtab-6-menu');
  await main.keyboard.press('Escape');
  await sleep(400);
  // A toast in the corner
  await main.getByTitle('复制地址', { exact: true }).click();
  await sleep(500);
  step('toast shows over the live view', (await shown()) === 'h', await shown());
  await shot('webtab-7-toast');

  await addr.fill(`${SITE}/typed`);
  await addr.press('Enter');
  await until(() => pages().some((p) => p.url() === `${SITE}/typed`), 10000, 'typed address').catch(() => {});
  step('typed address opens in the view', pages().some((p) => p.url() === `${SITE}/typed`));

  // Panel width change: the view follows
  const w0 = (await main.locator('.wb-native').boundingBox()).width;
  await main.evaluate(() => {
    const el = document.querySelector('.wb-native');
    el.style.marginLeft = '120px';
  });
  await sleep(600);
  const typed = pages().find((p) => p.url() === `${SITE}/typed`);
  const w1 = typed ? await typed.evaluate(() => innerWidth) : -1;
  step('view follows a layout change', Math.abs(w1 - (w0 - 120)) <= 2, `placeholder ${Math.round(w0 - 120)}, page ${w1}`);
  await shot('webtab-8-resized');
  await main.evaluate(() => (document.querySelector('.wb-native').style.marginLeft = ''));

  // Closing the tabs closes the webviews
  for (let i = 0; i < 4 && (await main.locator('[role=tab]').count()); i++) {
    await main.locator('[role=tab]').first().click({ button: 'middle' });
    await sleep(500);
  }
  const left = await until(() => !pages().some((p) => p.url().startsWith(SITE)), 8000, 'webviews closed').catch(() => false);
  step('closing tabs closes the webviews', !!left, pages().map((p) => p.url()).join(' '));
  step('no page errors in the app', main.errors.length === 0, main.errors.join(' | '));
} catch (e) {
  step('run', false, String(e?.stack ?? e));
} finally {
  fs.writeFileSync(path.join(OUT, 'report.json'), JSON.stringify(report, null, 2));
  await browser?.close().catch(() => {});
  app.kill();
  site.close();
  const failed = report.steps.filter((s) => !s.ok).length;
  console.log(failed ? `FAILED ${failed}` : 'ALL PASS');
  process.exit(failed ? 1 : 0);
}
