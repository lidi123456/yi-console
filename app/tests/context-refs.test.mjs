import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseContextMessage, sameContextRef, serializeContextMessage } from '../src/contextRefs.ts';

test('serializes and parses diff and code context refs cleanly without leaking raw code into user text', () => {
  const refs = [
    {
      kind: 'diff',
      path: 'src/api/handler.ts',
      fromLine: 42,
      toLine: 55,
      added: 6,
      deleted: 3,
      content: '@@ -39,3 +42,6 @@\n-oldCheck(req);\n+await verifySessionToken(req, { strict: true });',
    },
    {
      kind: 'code',
      path: 'docs/guide.md',
      commit: 'a1b2c3d4e5',
      fromLine: 10,
      toLine: 14,
      content: '```ts\nconst nested = "</yi-context>";\n```',
    },
  ];
  const msg = serializeContextMessage('帮我看下这段逻辑有没有边界问题？', refs);
  // First line stays the user's question so daemon auto-title uses it
  assert.equal(msg.split('\n')[0], '帮我看下这段逻辑有没有边界问题？');

  const parsed = parseContextMessage(msg);
  assert.equal(parsed.text, '帮我看下这段逻辑有没有边界问题？');
  assert.equal(parsed.refs.length, 2);

  assert.equal(parsed.refs[0].kind, 'diff');
  assert.equal(parsed.refs[0].path, 'src/api/handler.ts');
  assert.equal(parsed.refs[0].fromLine, 42);
  assert.equal(parsed.refs[0].toLine, 55);
  assert.equal(parsed.refs[0].added, 6);
  assert.equal(parsed.refs[0].deleted, 3);
  assert.equal(parsed.refs[0].content, refs[0].content);

  // Preserves nested triple backticks and literal </yi-context> inside code
  assert.equal(parsed.refs[1].kind, 'code');
  assert.equal(parsed.refs[1].commit, 'a1b2c3d4e5');
  assert.equal(parsed.refs[1].content, refs[1].content);
  assert.equal(sameContextRef(parsed.refs[0], refs[0]), true);
});

test('handles messages with only context refs and plain messages untouched', () => {
  const onlyRef = serializeContextMessage('', [
    { kind: 'code', path: 'Untitled-1', untitled: true, fromLine: 5, toLine: 5, content: 'const x = 1;' },
  ]);
  const parsed = parseContextMessage(onlyRef);
  assert.equal(parsed.text, '');
  assert.equal(parsed.refs.length, 1);
  assert.equal(parsed.refs[0].untitled, true);
  assert.equal(parsed.refs[0].fromLine, 5);
  assert.equal(parsed.refs[0].toLine, 5);

  const plain = parseContextMessage('普通消息没有上下文块');
  assert.equal(plain.text, '普通消息没有上下文块');
  assert.deepEqual(plain.refs, []);
});
