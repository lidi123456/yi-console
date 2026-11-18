// End-to-end Playwright verification on Windows Chrome (desktop + mobile touch).
// Spawns an isolated Vite server on 127.0.0.1:4179 and cleans up its process tree on exit.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs/promises';

await fs.mkdir('out', { recursive: true });

const PORT = 4179;
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

const readClip = (page) => page.evaluate(async () => (await navigator.clipboard.readText()).replace(/\r\n/g, '\n'));
async function clickCopy(btn) {
  await btn.click();
  const h = await btn.elementHandle();
  await btn.page().waitForFunction((el) => el.classList.contains('copied'), h);
}

try {
  await waitServer(`${BASE}/tests/chat-copy.html`);
  const browser = await chromium.launch({ channel: 'chrome', headless: true });

  try {
    // ---------------- 1. Desktop verification ----------------
    const deskCtx = await browser.newContext({
      viewport: { width: 1120, height: 820 },
      deviceScaleFactor: 2,
      permissions: ['clipboard-read', 'clipboard-write'],
    });
    const desk = await deskCtx.newPage();
    const deskErrors = [];
    desk.on('pageerror', (e) => deskErrors.push(e.message));
    await desk.goto(`${BASE}/tests/chat-copy.html`);
    await desk.locator('.md-code-wrap').first().waitFor();

    // Mobile-only select-text button must stay hidden on desktop
    assert.equal(await desk.locator('.mobile-select-btn').first().isVisible(), false, 'Select text button stays hidden on desktop');

    // Option B floating code copy button: hidden at rest on desktop, visible on hover
    const firstCodeWrap = desk.locator('.md-code-wrap').first();
    const firstCodeBtn = firstCodeWrap.locator('button.copy-float-btn');
    assert.equal(await firstCodeBtn.evaluate((el) => getComputedStyle(el).opacity), '0');
    await firstCodeWrap.hover();
    await desk.waitForFunction(() => getComputedStyle(document.querySelector('.md-code-wrap button.copy-float-btn')).opacity === '1');
    await desk.screenshot({ path: 'out/desktop-light-hover.png' });

    await clickCopy(firstCodeBtn);
    assert.equal(
      await readClip(desk),
      'export async function verifyCopy(input: string): Promise<boolean> {\n  const trimmed = input.trim();\n  return trimmed.length > 0;\n}',
    );
    assert.equal(await firstCodeBtn.evaluate((el) => el.classList.contains('copied')), true);

    // Table copy button: wrapper fits table width, copies valid GFM Markdown
    const tableWrap = desk.locator('.md-table-wrap').first();
    const widths = await tableWrap.evaluate((w) => ({
      wrapW: Math.round(w.getBoundingClientRect().width),
      tableW: Math.round(w.querySelector('table').getBoundingClientRect().width),
      parentW: Math.round(w.parentElement.getBoundingClientRect().width),
    }));
    assert.ok(Math.abs(widths.wrapW - widths.tableW) <= 2 && widths.wrapW < widths.parentW, `table wrapper fits table: ${JSON.stringify(widths)}`);
    await tableWrap.hover();
    await clickCopy(tableWrap.locator('button.table-copy-btn'));
    const tableClip = await readClip(desk);
    assert.equal(
      tableClip,
      [
        '| 平台 | 对齐 `:---:` | 命令示例 |',
        '| :--- | :---: | ---: |',
        '| Windows | `x86_64` | `cargo test \\| tee` |',
        '| Android | **aarch64** | `yc-android.cmd` |',
      ].join('\n'),
    );

    // Wide 5-column table: stays within chat column width, keeps short code tokens unbroken, scrolls horizontally, and opens fullscreen viewer
    const wideWrap = desk.locator('.md-table-wrap').nth(1);
    const wideMetrics = await wideWrap.evaluate((w) => {
      const scroll = w.querySelector('.md-table-scroll');
      const table = scroll.querySelector('table');
      const actions = w.querySelector('.table-actions');
      const wRect = w.getBoundingClientRect();
      const aRect = actions.getBoundingClientRect();
      const codeRects = [...table.querySelectorAll('code.md-code-nowrap')].map((c) => ({
        text: c.textContent,
        lines: c.getClientRects().length,
      }));
      return {
        wrapW: Math.round(wRect.width),
        parentW: Math.round(w.parentElement.getBoundingClientRect().width),
        scrollW: scroll.scrollWidth,
        clientW: scroll.clientWidth,
        hasScrollRight: w.classList.contains('has-scroll-right'),
        actionsInsideWrap: aRect.top >= wRect.top - 16 && aRect.bottom <= wRect.top + 16 && aRect.right <= wRect.right + 1,
        codeRects,
        firstColW: Math.round(table.rows[0].cells[0].getBoundingClientRect().width),
      };
    });
    assert.ok(wideMetrics.wrapW <= wideMetrics.parentW, `wide table stays inside 768px column: ${JSON.stringify(wideMetrics)}`);
    assert.ok(wideMetrics.scrollW > wideMetrics.clientW && wideMetrics.hasScrollRight, `wide table scrolls horizontally with hint: ${JSON.stringify(wideMetrics)}`);
    assert.ok(wideMetrics.firstColW >= 84, `first column is not crushed: ${wideMetrics.firstColW}`);
    assert.ok(wideMetrics.actionsInsideWrap, `sticky table actions sit cleanly inside top-right corner: ${JSON.stringify(wideMetrics)}`);
    for (const c of wideMetrics.codeRects) {
      assert.equal(c.lines, 1, `short code token "${c.text}" stays on a single line`);
    }

    await wideWrap.scrollIntoViewIfNeeded();
    await wideWrap.hover();
    await desk.waitForFunction(() => getComputedStyle(document.querySelectorAll('.md-table-wrap')[1].querySelector('.table-actions')).opacity === '1');
    await desk.screenshot({ path: 'out/desktop-wide-table.png' });
    await wideWrap.locator('button.table-expand-btn').click();
    const viewer = desk.locator('.table-viewer-bg');
    await viewer.waitFor();
    const viewerFit = await viewer.evaluate((v) => {
      const scroll = v.querySelector('.md-table-scroll');
      return { scrollW: scroll.scrollWidth, clientW: scroll.clientWidth };
    });
    assert.ok(viewerFit.scrollW <= viewerFit.clientW + 2, `fullscreen table fits wide viewport without scroll: ${JSON.stringify(viewerFit)}`);
    await clickCopy(viewer.locator('.table-viewer-head button.btn'));
    assert.match(await readClip(desk), /SKILL\.md/);
    const htmlClip = await desk.evaluate(async () => {
      const items = await navigator.clipboard.read();
      for (const item of items) {
        if (item.types.includes('text/html')) {
          const blob = await item.getType('text/html');
          return await blob.text();
        }
      }
      return '';
    });
    assert.match(htmlClip, /<br>.*<code>AGENTS\.md<\/code>/);
    assert.match(htmlClip, /<strong>RFC 2119/);
    await desk.screenshot({ path: 'out/desktop-table-viewer.png' });
    await desk.keyboard.press('Escape');
    assert.equal(await viewer.count(), 0);
    await wideWrap.hover();
    await wideWrap.locator('button.table-expand-btn').click();
    await viewer.waitFor();
    await desk.waitForFunction(() => document.activeElement?.closest('.table-viewer-head') !== null);
    assert.equal(await desk.evaluate(() => window.__yiConsoleBack()), true);
    await viewer.waitFor({ state: 'detached' });

    // Sticky behavior on tall code block while scrolling `.chat`
    const tallWrap = desk.locator('.md-code-wrap').nth(1);
    const tallBtn = tallWrap.locator('button.copy-float-btn');
    await desk.evaluate(() => {
      const chat = document.querySelector('.chat');
      const wrap = document.querySelectorAll('.md-code-wrap')[1];
      chat.scrollTop = wrap.offsetTop + 180;
    });
    const stickyPos = await desk.evaluate(() => {
      const chatR = document.querySelector('.chat').getBoundingClientRect();
      const wrapR = document.querySelectorAll('.md-code-wrap')[1].getBoundingClientRect();
      const btnR = document.querySelectorAll('.md-code-wrap button.copy-float-btn')[1].getBoundingClientRect();
      return { chatTop: chatR.top, wrapTop: wrapR.top, wrapBottom: wrapR.bottom, btnTop: btnR.top, btnBottom: btnR.bottom };
    });
    assert.ok(stickyPos.wrapTop < stickyPos.chatTop, 'tall code block scrolled past top');
    assert.ok(stickyPos.btnTop >= stickyPos.chatTop && stickyPos.btnBottom <= stickyPos.wrapBottom, `sticky button stays inside bounds: ${JSON.stringify(stickyPos)}`);
    assert.ok(await tallBtn.isVisible());

    // Compact tool group input & output copy (copies full 450 lines even when UI truncates at 400)
    await desk.evaluate(() => (document.querySelector('.chat').scrollTop = 0));
    await desk.locator('.ag-head').click();
    await desk.locator('.mt-head').click();
    await desk.locator('.mt-sec').first().hover();
    await clickCopy(desk.locator('.mt-sec').first().locator('.inline-copy-btn'));
    assert.equal(await readClip(desk), 'cargo test --workspace -- --nocapture');
    await desk.locator('.mt-sec').nth(1).hover();
    await clickCopy(desk.locator('.mt-sec').nth(1).locator('.inline-copy-btn'));
    assert.equal((await readClip(desk)).split('\n').length, 450, 'tool output copy captures all 450 lines');

    // Permission approval command copy
    await desk.locator('.perm-card .copy-pre-wrap').hover();
    await clickCopy(desk.locator('.perm-card .copy-pre-wrap button.copy-float-btn'));
    assert.equal(await readClip(desk), 'git status --porcelain -b');

    // Claude timeline style ToolLine copy
    await desk.evaluate(() => window.setChatStyle('claude'));
    await desk.locator('.tl-tool .tl-head').click();
    const tlWraps = desk.locator('.tl-detail .copy-pre-wrap');
    await tlWraps.first().hover();
    await clickCopy(tlWraps.first().locator('button.copy-float-btn'));
    assert.equal(await readClip(desk), 'cargo test --workspace -- --nocapture');
    await tlWraps.nth(1).hover();
    await clickCopy(tlWraps.nth(1).locator('button.copy-float-btn'));
    assert.equal((await readClip(desk)).split('\n').length, 450);
    await desk.evaluate(() => window.setChatStyle('modern'));

    // Streaming copied-state persistence across DOM redraws
    await desk.evaluate(() => window.setLiveText('流式代码块：\n```js\nconsole.log(42);\n```\n段落一'));
    const liveBtn = desk.locator('.streaming .md-code-wrap button.copy-float-btn');
    await desk.locator('.streaming .md-code-wrap').hover();
    await clickCopy(liveBtn);
    assert.equal(await readClip(desk), 'console.log(42);');
    await desk.evaluate(() => window.setLiveText('流式代码块：\n```js\nconsole.log(42);\n```\n段落一，继续流式追加新内容不会冲掉已复制打勾态'));
    assert.equal(await liveBtn.evaluate((el) => el.classList.contains('copied')), true, 'copied checkmark survives streaming redraw');

    await desk.evaluate(() => window.setTheme('dark'));
    await desk.locator('.md-code-wrap').first().hover();
    await desk.screenshot({ path: 'out/desktop-dark-hover.png' });

    assert.deepEqual(deskErrors, []);
    await deskCtx.close();

    // ---------------- 2. Mobile touch verification ----------------
    const mobCtx = await browser.newContext({
      viewport: { width: 390, height: 844 },
      deviceScaleFactor: 2,
      isMobile: true,
      hasTouch: true,
      permissions: ['clipboard-read', 'clipboard-write'],
    });
    const mob = await mobCtx.newPage();
    const mobErrors = [];
    mob.on('pageerror', (e) => mobErrors.push(e.message));
    await mob.goto(`${BASE}/tests/chat-copy.html`);
    await mob.locator('.md-code-wrap').first().waitFor();

    // Copy buttons stay visible without hover on coarse/touch devices, with enlarged touch targets
    const mobBtnInfo = await mob.locator('.md-code-wrap button.copy-float-btn').first().evaluate((el) => {
      const s = getComputedStyle(el);
      const r = el.getBoundingClientRect();
      return { opacity: s.opacity, w: r.width, h: r.height, userSelect: s.userSelect || s.webkitUserSelect };
    });
    assert.equal(mobBtnInfo.opacity, '1', 'copy button visible without hover on mobile');
    assert.ok(mobBtnInfo.w >= 28 && mobBtnInfo.h >= 28, `enlarged mobile touch target: ${JSON.stringify(mobBtnInfo)}`);
    assert.equal(mobBtnInfo.userSelect, 'none');

    // Surrounding chrome elements have user-select: none so drag handles don't grab timestamps/buttons
    const chromeSelect = await mob.evaluate(() =>
      ['.m-actions', '.msg-user-actions', '.ag-head'].map((sel) => {
        const s = getComputedStyle(document.querySelector(sel));
        return s.userSelect || s.webkitUserSelect;
      }),
    );
    assert.deepEqual(chromeSelect, ['none', 'none', 'none']);

    // Native touch long-press selection: contextmenu on prose, code, user bubble, and textarea MUST NOT preventDefault or open `.ctx-scrim`
    const touchPassThrough = await mob.evaluate(() => {
      const targets = [
        document.querySelector('.md p'),
        document.querySelector('.md pre code'),
        document.querySelector('.msg-user .bubble'),
        document.querySelector('textarea[aria-label="fixture-input"]'),
      ];
      return targets.map((el) => {
        const ev = new PointerEvent('contextmenu', { bubbles: true, cancelable: true, pointerType: 'touch', clientX: 60, clientY: 120 });
        const notPrevented = el.dispatchEvent(ev);
        const scrimOpen = !!document.querySelector('.ctx-scrim');
        return { tag: el.tagName, notPrevented, scrimOpen };
      });
    });
    for (const item of touchPassThrough) {
      assert.equal(item.notPrevented, true, `touch contextmenu must pass through to OS on ${item.tag}`);
      assert.equal(item.scrimOpen, false, `.ctx-scrim must not block touch selection handles on ${item.tag}`);
    }

    // Streaming touch freeze: pointerdown freezes LiveMarkdown DOM until pointerup
    await mob.evaluate(() => window.setLiveText('正在输出第一句'));
    const liveEl = mob.locator('.streaming');
    await liveEl.waitFor();
    await mob.evaluate(() => {
      document.querySelector('.streaming').dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, pointerType: 'touch' }));
      window.setLiveText('正在输出第一句，紧接着第二句到来');
    });
    assert.match(await liveEl.innerText(), /^正在输出第一句$/);
    await mob.evaluate(() => {
      window.dispatchEvent(new PointerEvent('pointerup', { bubbles: true, pointerType: 'touch' }));
    });
    await mob.waitForFunction(() => document.querySelector('.streaming')?.innerText.includes('紧接着第二句到来'));
    await mob.evaluate(() => window.setLiveText(''));

    // Mobile-only "Select text" sheet: visible on phone, supports partial selection copy & Android back button
    const selBtn = mob.locator('.m-actions .mobile-select-btn').first();
    assert.equal(await selBtn.isVisible(), true, 'Select text button visible on mobile');
    await mob.screenshot({ path: 'out/mobile-chat.png' });

    await selBtn.click();
    const modal = mob.locator('.select-text-modal');
    await modal.waitFor();
    assert.match(await modal.locator('.select-text-head button.btn').innerText(), /复制全文/);

    // Simulate user dragging native selection handles over a phrase inside `.select-text-body`
    await mob.evaluate(() => {
      const textNode = document.querySelector('.select-text-body').firstChild;
      const range = document.createRange();
      range.setStart(textNode, 0);
      range.setEnd(textNode, 18);
      const sel = window.getSelection();
      sel.removeAllRanges();
      sel.addRange(range);
      document.dispatchEvent(new Event('selectionchange'));
    });
    await mob.waitForFunction(() => document.querySelector('.select-text-head button.btn')?.innerText.includes('复制选中'));
    await mob.screenshot({ path: 'out/mobile-select-sheet.png' });

    await modal.locator('.select-text-head button.btn').click();
    assert.equal(await readClip(mob), '排查完成，可以执行下面这段构建与校验');

    // Android hardware back closes the Select text sheet
    assert.equal(await mob.evaluate(() => window.__yiConsoleBack()), true);
    assert.equal(await modal.count(), 0);

    assert.deepEqual(mobErrors, []);
    await mobCtx.close();
    console.log('PASS: desktop & mobile code/table/tool/permission copy, sticky scroll, streaming persistence, touch selection pass-through, streaming pointer freeze, and mobile Select Text sheet.');
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
