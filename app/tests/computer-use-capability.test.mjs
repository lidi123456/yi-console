import { test } from 'node:test';
import assert from 'node:assert/strict';
import { canUseComputer } from '../src/computerUse.ts';
const chat = (tool = 'kiro') => ({ tool, mode: 'structured' });
const windows = { os: 'windows', arch: 'x86_64' };
test('all six AI chat adapters share the Windows desktop entry', () => {
  for (const tool of ['claude', 'codex', 'cursor', 'kiro', 'qoder', 'codebuddy']) assert.equal(canUseComputer(windows, chat(tool)), true);
});
test('Linux and macOS target hosts never show the desktop entry', () => {
  for (const os of ['linux', 'macos']) assert.equal(canUseComputer({ os, arch: 'x86_64' }, chat()), false);
});
test('unknown hosts, unsupported architectures and non-AI sessions are excluded', () => {
  assert.equal(canUseComputer(undefined, chat()), false);
  assert.equal(canUseComputer({ os: 'windows', arch: 'aarch64' }, chat()), false);
  for (const tool of ['files', 'shell']) assert.equal(canUseComputer(windows, chat(tool)), false);
  assert.equal(canUseComputer(windows, { tool: 'kiro', mode: 'terminal' }), false);
});
