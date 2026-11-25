import { describe, test } from 'node:test';
import assert from 'node:assert/strict';
import {
  applyGatewayEvent,
  browserToolName,
  claimEarlyGatewayForTool,
  isDesktopTool,
} from '../src/gatewayFold.ts';

function runFoldSequence(events) {
  const items = [];
  const tools = new Map();
  const desktopTools = new Map();
  const earlyGateways = [];
  let planIdx = -1;
  const adjustPlanIdx = (removedIdx) => {
    if (planIdx > removedIdx) planIdx--;
    else if (planIdx === removedIdx) planIdx = -1;
  };

  for (const e of events) {
    if (e.kind === 'plan') {
      if (planIdx >= 0) items.splice(planIdx, 1);
      items.push({ k: 'plan', key: `p${e.seq}`, plan: e.plan });
      planIdx = items.length - 1;
      continue;
    }
    if (e.kind === 'user_message') {
      items.push({ k: 'user', key: `u${e.seq}`, seq: e.seq, text: e.text, ts: e.ts });
      continue;
    }
    if (e.kind === 'assistant_text') {
      items.push({ k: 'text', key: `t${e.seq}`, ts: e.ts, text: e.text, id: e.id });
      continue;
    }
    if (e.kind === 'turn_end') {
      items.push({ k: 'turn', key: `end${e.seq}`, seq: e.seq, ts: e.ts });
      continue;
    }
    if (e.kind === 'computer_use' || e.kind === 'browser_use') {
      if (e.status === 'control') {
        items.push({ k: 'note', key: `desktop-${e.actionId}`, text: e.tool });
        continue;
      }
      applyGatewayEvent(items, earlyGateways, desktopTools, e, adjustPlanIdx);
      continue;
    }
    if (e.kind === 'tool_call') {
      const old = tools.get(e.id);
      if (old) {
        old.title = e.title || old.title;
        old.input = e.input ?? old.input;
        if (!old.desktop) claimEarlyGatewayForTool(items, earlyGateways, desktopTools, old, adjustPlanIdx);
      } else {
        const it = {
          k: 'tool',
          key: `e${e.seq}`,
          id: e.id,
          ts: e.ts,
          name: e.name,
          title: e.title,
          input: e.input,
        };
        tools.set(e.id, it);
        items.push(it);
        claimEarlyGatewayForTool(items, earlyGateways, desktopTools, it, adjustPlanIdx);
      }
      continue;
    }
    if (e.kind === 'tool_result') {
      const t = tools.get(e.id);
      if (t) t.result = { output: e.output, isError: e.isError };
    }
  }
  return { items, earlyGateways, getPlanIdx: () => planIdx };
}

describe('gatewayFold', () => {
  test('recognizes built-in tools across all providers and avoids substring collisions', () => {
    assert.equal(browserToolName({ name: 'mcp__yi_browser__browser_click', title: 'mcp__yi_browser__browser_click' }), 'browser_click');
    assert.equal(browserToolName({ name: 'mcp_yi_browser_browser_open', title: 'mcp_yi_browser_browser_open' }), 'browser_open');
    assert.equal(browserToolName({ name: 'yi_browser.browser_wait', title: 'Wait' }), 'browser_wait');
    assert.equal(browserToolName({ name: 'tool', title: 'browser_snapshot (yi_browser)' }), 'browser_snapshot');
    assert.equal(browserToolName({ name: '@yi_browser/browser_network', title: 'net' }), 'browser_network');
    assert.equal(
      browserToolName({ name: 'DeferExecuteTool', title: 'DeferExecuteTool', input: { toolName: 'mcp__yi_browser__browser_set' } }),
      'browser_set',
    );
    assert.equal(browserToolName({ name: 'Bash', title: 'echo browser_click (yi_browser)' }), null);

    const doubleClickTool = { name: 'mcp__yi_computer_use__double_click', title: 'mcp__yi_computer_use__double_click' };
    assert.equal(isDesktopTool(doubleClickTool, { kind: 'computer_use', tool: 'click' }), false);
    assert.equal(isDesktopTool(doubleClickTool, { kind: 'computer_use', tool: 'double_click' }), true);

    const agyBrowserOpen = { name: 'mcp_yi_browser_browser_open', title: 'mcp_yi_browser_browser_open' };
    assert.equal(isDesktopTool(agyBrowserOpen, { kind: 'browser_use', tool: 'browser_open' }), true);
    assert.equal(isDesktopTool(agyBrowserOpen, { kind: 'browser_use', tool: 'browser_click' }), false);
  });

  test('folds normal tool_call -> browser_use running -> completed', () => {
    const { items, earlyGateways } = runFoldSequence([
      { seq: 1, ts: 1, kind: 'user_message', text: 'open page' },
      { seq: 2, ts: 2, kind: 'tool_call', id: 'tc1', name: 'mcp_yi_browser_browser_open', title: 'mcp_yi_browser_browser_open' },
      { seq: 3, ts: 3, kind: 'browser_use', actionId: 'a1', tool: 'browser_open', status: 'running', message: 'reload', durationMs: 0, images: [] },
      { seq: 4, ts: 4, kind: 'browser_use', actionId: 'a1', tool: 'browser_open', status: 'completed', message: 'reload', durationMs: 1200, url: 'http://127.0.0.1:13001', images: [] },
      { seq: 5, ts: 5, kind: 'tool_result', id: 'tc1', output: 'ok', isError: false },
    ]);
    assert.equal(earlyGateways.length, 0);
    assert.equal(items.filter((i) => i.k === 'desktop').length, 0);
    const tool = items.find((i) => i.k === 'tool');
    assert.equal(tool.desktop.actionId, 'a1');
    assert.equal(tool.desktop.status, 'completed');
    assert.equal(tool.desktop.durationMs, 1200);
  });

  test('claims out-of-order already-completed gateway event (including images) and keeps planIdx accurate', () => {
    const shot = [{ id: 'browser-1.png', mimeType: 'image/png' }];
    const { items, earlyGateways, getPlanIdx } = runFoldSequence([
      { seq: 1, ts: 1, kind: 'user_message', text: 'take shot' },
      { seq: 2, ts: 2, kind: 'browser_use', actionId: 'a1', tool: 'browser_screenshot', status: 'running', message: '', durationMs: 0, images: [] },
      { seq: 3, ts: 3, kind: 'browser_use', actionId: 'a1', tool: 'browser_screenshot', status: 'completed', message: '', durationMs: 85, url: 'http://x', images: shot },
      { seq: 4, ts: 4, kind: 'plan', plan: { entries: [{ content: 'step', status: 'in_progress' }] } },
      { seq: 5, ts: 5, kind: 'tool_call', id: 'tc1', name: 'mcp_yi_browser_browser_screenshot', title: 'mcp_yi_browser_browser_screenshot' },
      { seq: 6, ts: 6, kind: 'plan', plan: { entries: [{ content: 'step', status: 'completed' }] } },
    ]);
    assert.equal(earlyGateways.length, 0);
    assert.equal(items.filter((i) => i.k === 'desktop').length, 0);
    const tool = items.find((i) => i.k === 'tool');
    assert.equal(tool.desktop.status, 'completed');
    assert.deepEqual(tool.desktop.images, shot);
    assert.equal(items.filter((i) => i.k === 'plan').length, 1);
    assert.equal(items[getPlanIdx()].k, 'plan');
  });

  test('pairs parallel out-of-order same-name calls FIFO even when first finishes while second is still running', () => {
    const shot1 = [{ id: 'shot1.png', mimeType: 'image/png' }];
    const shot2 = [{ id: 'shot2.png', mimeType: 'image/png' }];
    const { items, earlyGateways } = runFoldSequence([
      { seq: 1, ts: 1, kind: 'user_message', text: 'click twice' },
      { seq: 2, ts: 2, kind: 'browser_use', actionId: 'a1', tool: 'browser_click', status: 'running', message: 'e1', durationMs: 0, images: [] },
      { seq: 3, ts: 3, kind: 'browser_use', actionId: 'a2', tool: 'browser_click', status: 'running', message: 'e2', durationMs: 0, images: [] },
      { seq: 4, ts: 4, kind: 'browser_use', actionId: 'a1', tool: 'browser_click', status: 'completed', message: 'e1', durationMs: 10, images: shot1 },
      { seq: 5, ts: 5, kind: 'tool_call', id: 'tc1', name: 'mcp_yi_browser_browser_click', title: 'mcp_yi_browser_browser_click' },
      { seq: 6, ts: 6, kind: 'tool_call', id: 'tc2', name: 'mcp_yi_browser_browser_click', title: 'mcp_yi_browser_browser_click' },
      { seq: 7, ts: 7, kind: 'browser_use', actionId: 'a2', tool: 'browser_click', status: 'completed', message: 'e2', durationMs: 25, images: shot2 },
    ]);
    assert.equal(earlyGateways.length, 0);
    assert.equal(items.filter((i) => i.k === 'desktop').length, 0);
    const tools = items.filter((i) => i.k === 'tool');
    assert.equal(tools.length, 2);
    assert.equal(tools[0].desktop.actionId, 'a1');
    assert.equal(tools[0].desktop.durationMs, 10);
    assert.deepEqual(tools[0].desktop.images, shot1);
    assert.equal(tools[1].desktop.actionId, 'a2');
    assert.equal(tools[1].desktop.durationMs, 25);
    assert.deepEqual(tools[1].desktop.images, shot2);
  });

  test('claims completed gateway event across assistant_text within the same turn while keeping planIdx accurate', () => {
    const { items, earlyGateways, getPlanIdx } = runFoldSequence([
      { seq: 1, ts: 1, kind: 'user_message', text: 'check page' },
      { seq: 2, ts: 2, kind: 'plan', plan: { entries: [{ content: 'step', status: 'in_progress' }] } },
      { seq: 3, ts: 3, kind: 'browser_use', actionId: 'a1', tool: 'browser_open', status: 'completed', message: 'open', durationMs: 40, images: [] },
      { seq: 4, ts: 4, kind: 'assistant_text', id: 'm1', text: 'Opening page now.' },
      { seq: 5, ts: 5, kind: 'tool_call', id: 'tc1', name: 'mcp_yi_browser_browser_open', title: 'mcp_yi_browser_browser_open' },
      { seq: 6, ts: 6, kind: 'plan', plan: { entries: [{ content: 'step', status: 'completed' }] } },
    ]);
    assert.equal(earlyGateways.length, 0);
    assert.equal(items.filter((i) => i.k === 'desktop').length, 0);
    const tool = items.find((i) => i.k === 'tool');
    assert.equal(tool.desktop.actionId, 'a1');
    assert.equal(items.filter((i) => i.k === 'plan').length, 1);
    assert.equal(items[getPlanIdx()].k, 'plan');
  });

  test('claims completed gateway event when DeferExecuteTool receives toolName on update', () => {
    const { items, earlyGateways } = runFoldSequence([
      { seq: 1, ts: 1, kind: 'user_message', text: 'defer' },
      { seq: 2, ts: 2, kind: 'tool_call', id: 'tc1', name: 'DeferExecuteTool', title: 'DeferExecuteTool', input: {} },
      { seq: 3, ts: 3, kind: 'tool_result', id: 'tc1', output: 'ok', isError: false },
      { seq: 4, ts: 4, kind: 'browser_use', actionId: 'a1', tool: 'browser_open', status: 'completed', message: 'open', durationMs: 50, images: [] },
      { seq: 5, ts: 5, kind: 'tool_call', id: 'tc1', name: 'DeferExecuteTool', title: 'DeferExecuteTool', input: { toolName: 'mcp__yi_browser__browser_open' } },
    ]);
    assert.equal(earlyGateways.length, 0);
    assert.equal(items.filter((i) => i.k === 'desktop').length, 0);
    const tool = items.find((i) => i.k === 'tool');
    assert.equal(tool.desktop.actionId, 'a1');
  });

  test('keeps standalone manual captures and never folds running or completed actions across user or turn boundaries', () => {
    const { items, earlyGateways } = runFoldSequence([
      { seq: 1, ts: 1, kind: 'user_message', text: 'turn 1' },
      { seq: 2, ts: 2, kind: 'browser_use', actionId: 'a-turn1-run', tool: 'browser_open', status: 'running', message: 'turn1', durationMs: 0, images: [] },
      { seq: 3, ts: 3, kind: 'turn_end' },
      // Manual action between turns
      { seq: 4, ts: 4, kind: 'browser_use', actionId: 'a-manual', tool: 'browser_open', status: 'completed', message: 'manual', durationMs: 100, images: [] },
      // Turn 2 opens with same-name tool call: must NOT claim Turn 1's still-running action or manual action
      { seq: 5, ts: 5, kind: 'user_message', text: 'turn 2' },
      { seq: 6, ts: 6, kind: 'tool_call', id: 'tc-turn2', name: 'mcp_yi_browser_browser_open', title: 'mcp_yi_browser_browser_open' },
      // Even when Turn 1's unmatched action finishes during Turn 2, it must stay in Turn 1 (never attach to tc-turn2)
      { seq: 7, ts: 7, kind: 'browser_use', actionId: 'a-turn1-run', tool: 'browser_open', status: 'completed', message: 'turn1', durationMs: 200, images: [] },
    ]);
    const tcTurn2 = items.find((i) => i.k === 'tool' && i.id === 'tc-turn2');
    assert.equal(tcTurn2.desktop, undefined);
    const desktops = items.filter((i) => i.k === 'desktop').map((i) => i.ev.actionId);
    assert.deepEqual(desktops, ['a-manual', 'a-turn1-run']);
    assert.equal(earlyGateways.length, 2);
  });
});
