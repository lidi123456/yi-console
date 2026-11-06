// Real installed CLI adapters + a harmless external MCP. No mocked provider process.
import fs from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import assert from 'node:assert/strict';

const root = process.env.YI_CU_TEST_ROOT;
if (!root) throw new Error('Set YI_CU_TEST_ROOT to the isolated test directory');
const python = process.env.YI_MCP_TEST_PYTHON || 'python';
const providers = (process.env.YI_MCP_TEST_PROVIDERS || 'claude,codex,cursor,kiro,qoder,codebuddy').split(',');
const runId = Date.now().toString();
const report = { providers: [], settingsRedaction: false, reload: {} };
await fs.mkdir('out', { recursive: true });
const ws = new WebSocket('ws://127.0.0.1:8193/ws?token=cu-validation');
await new Promise((resolve, reject) => { ws.addEventListener('open', resolve, { once: true }); ws.addEventListener('error', reject, { once: true }); });
let next = 0;
const pending = new Map();
ws.addEventListener('message', (event) => {
  const value = JSON.parse(event.data), p = pending.get(value.id);
  if (p) { clearTimeout(p.timer); pending.delete(value.id); value.error ? p.reject(new Error(String(value.error))) : p.resolve(value.result); }
});
const hub = (cmd, args = {}) => new Promise((resolve, reject) => {
  const id = ++next, timer = setTimeout(() => { pending.delete(id); reject(new Error(`RPC timeout: ${cmd} ${args.method || ''}`)); }, 90000);
  pending.set(id, { resolve, reject, timer }); ws.send(JSON.stringify({ id, cmd, args }));
});
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const host = (await hub('hosts.list')).hosts.find((h) => h.config.kind === 'local').config.id;
const rpc = (method, params = {}) => hub('rpc', { id: host, method, params });
let hello;
for (let i = 0; i < 150; i++) { try { hello = await rpc('hello'); break; } catch { await sleep(200); } }
assert.ok(hello, 'isolated host must connect');
const original = await rpc('settings.get');
const nonces = Object.fromEntries(providers.map((tool) => [tool, randomUUID()]));
const markers = Object.fromEntries(providers.map((tool) => [tool, `${root}/mcp/${tool}-${runId}.jsonl`]));
let configs = providers.map((tool) => ({ name: `yi_acceptance_probe_${tool}`, enabled: true, transport: 'stdio', command: python, args: [root + '/probe-server.py', '--marker', markers[tool], '--label', tool + ' argument with spaces'], env: { YI_MCP_TEST_NONCE: nonces[tool] }, headers: {}, url: '', tools: [tool] }));
const persist = () => rpc('settings.set', { ...original, autoResume: false, mcpServers: [...(original.mcpServers || []), ...configs] });
const readMarker = async (tool) => { try { return (await fs.readFile(markers[tool], 'utf8')).trim().split('\n').filter(Boolean).map(JSON.parse); } catch { return []; } };
const sessions = [];
const save = () => fs.writeFile('out/mcp-provider-report.json', JSON.stringify(report, null, 2));
async function ready(sid) {
  for (let i = 0; i < 240; i++) {
    const d = await rpc('session.subscribe', { sessionId: sid, sinceSeq: 0 });
    if (['idle', 'error', 'exited'].includes(d.info.status)) return d;
    await sleep(250);
  }
  throw new Error('CLI initialization timed out');
}
async function turn(tool, sid, nonce, onCall) {
  const baseline = (await rpc('session.subscribe', { sessionId: sid, sinceSeq: 0 })).info.lastSeq;
  const handled = new Set();
  await rpc('session.send', { sessionId: sid, text: `Use only the yi_mcp_probe tool from server yi_acceptance_probe_${tool}. Call it once with delayMs=1500, then report the exact nonce returned. Do not read files, use shell, or guess the nonce.`, images: [] });
  let changed = false;
  for (let i = 0; i < 360; i++) {
    const d = await rpc('session.subscribe', { sessionId: sid, sinceSeq: 0 });
    const events = d.events.filter((e) => e.seq > baseline);
    for (const permission of events.filter((e) => e.kind === 'permission_request' && !handled.has(e.requestId))) {
      const option = permission.options.find((o) => o.kind === 'allow_once');
      const expected = `mcp__yi_acceptance_probe_${tool}__yi_mcp_probe`;
      const probe = permission.toolName === expected || permission.toolName === 'yi_mcp_probe' ||
        (permission.toolName === 'DeferExecuteTool' && permission.input?.toolName === expected);
      if (!probe || !option) throw new Error(`Unexpected permission: ${permission.toolName}`);
      handled.add(permission.requestId);
      await rpc('session.permission', { sessionId: sid, requestId: permission.requestId, optionId: option.id });
    }
    if (onCall && !changed && (await readMarker(tool)).some((row) => row.method === 'tools/call')) { changed = true; await onCall(); }
    const errors = events.filter((e) => e.kind === 'error' || e.kind === 'system').map((e) => e.message || e.text || '');
    const blocked = errors.find((text) => /403|401|HTTP 429|HTTP429|quota|limit exceeded|not logged|登录/i.test(text));
    if (blocked) return { ok: false, providerBlock: blocked.slice(0, 700), changed };
    if (events.some((e) => e.kind === 'turn_end') || ['error', 'exited'].includes(d.info.status)) {
      const answer = events.filter((e) => e.kind === 'assistant_text').map((e) => e.text).join('\n');
      return { ok: answer.includes(nonce), answer, errors, changed, permissions: handled.size };
    }
    await sleep(250);
  }
  return { ok: false, providerBlock: 'model response timed out' };
}
try {
  await persist();
  const masked = await rpc('settings.get');
  assert.ok(masked.mcpServers.filter((s) => s.name.startsWith('yi_acceptance_probe_')).every((s) => s.env.YI_MCP_TEST_NONCE === '********'));
  await rpc('settings.set', masked);
  report.settingsRedaction = true;
  for (const tool of providers) {
    const row = { tool, version: hello.tools.find((t) => t.kind === tool)?.version, connected: false, called: false, modelPassed: false };
    report.providers.push(row); let sid;
    try {
      assert.ok(hello.tools.find((t) => t.kind === tool)?.available, tool + ' must be installed');
      const info = await rpc('session.create', { tool, cwd: root, mode: 'structured', permissionMode: 'default', title: `Generic MCP · ${tool} · ${runId}`, ...(tool === 'kiro' ? { model: 'claude-opus-5.5' } : {}) });
      sid = info.id; sessions.push(sid); row.sessionId = sid;
      const initial = await ready(sid);
      row.startupErrors = initial.events.filter((e) => e.kind === 'error').map((e) => e.message);
      let replacement;
      const result = await turn(tool, sid, nonces[tool], tool === 'kiro' ? async () => {
        replacement = randomUUID(); configs = configs.map((s) => s.tools[0] === 'kiro' ? { ...s, env: { YI_MCP_TEST_NONCE: replacement } } : s);
        await persist(); report.reload.changedWhileBusy = true;
      } : undefined);
      Object.assign(row, { modelPassed: result.ok, result });
      if (tool === 'kiro' && result.ok && replacement) {
        await ready(sid);
        const refreshed = await turn(tool, sid, replacement);
        report.reload.afterBusy = refreshed.ok;
        assert.ok(refreshed.ok, 'MCP changes must apply after the active reply without interrupting its original nonce');
        const idleNonce = randomUUID(); configs = configs.map((s) => s.tools[0] === 'kiro' ? { ...s, env: { YI_MCP_TEST_NONCE: idleNonce } } : s);
        await persist();
        report.reload.afterIdle = (await turn(tool, sid, idleNonce)).ok;
        assert.ok(report.reload.afterIdle, 'idle session must reload MCP before its next message');
      }
    } catch (e) { row.error = String(e); }
    finally {
      const evidence = await readMarker(tool);
      row.connected = evidence.some((e) => e.method === 'initialize') && evidence.some((e) => e.method === 'tools/list');
      row.called = evidence.some((e) => e.method === 'tools/call' && e.tool === 'yi_mcp_probe');
      row.evidence = evidence;
      if (sid) await rpc('session.kill', { sessionId: sid }).catch(() => {});
      console.log(`${tool}: MCP connected=${row.connected}, called=${row.called}, model=${row.modelPassed}${row.error ? ', ' + row.error : ''}`);
      await save();
    }
  }
  report.allConnected = report.providers.every((row) => row.connected);
  report.finished = true;
  await save();
} finally {
  for (const sessionId of sessions) await rpc('session.kill', { sessionId }).catch(() => {});
  await rpc('settings.set', original).catch(() => {});
  ws.close();
}
