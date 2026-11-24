import { describe, test } from 'node:test';
import assert from 'node:assert/strict';
import {
  DRAG_REF,
  hasExternalFiles,
  isOtherSessionUpload,
  planChatDrop,
  planComposerDrop,
} from '../src/dropFiles.ts';

const IMAGE_TYPES = ['image/png', 'image/jpeg', 'image/gif', 'image/webp'];

describe('dropFiles helpers', () => {
  test('hasExternalFiles accepts Files and ignores internal DRAG_REF or plain text', () => {
    assert.equal(hasExternalFiles({ types: ['Files'] }), true);
    assert.equal(hasExternalFiles({ types: ['Files', 'text/plain'] }), true);
    assert.equal(hasExternalFiles({ types: [DRAG_REF, 'text/plain'] }), false);
    assert.equal(hasExternalFiles({ types: ['Files', DRAG_REF] }), false);
    assert.equal(hasExternalFiles({ types: ['text/plain'] }), false);
    assert.equal(hasExternalFiles(null), false);
  });

  test('isOtherSessionUpload filters only other sessions temporary upload paths', () => {
    assert.equal(
      isOtherSessionUpload('C:\\Users\\a\\.yi-console\\uploads\\s2\\files\\note.md', 's1'),
      true,
    );
    assert.equal(
      isOtherSessionUpload('/home/u/.yi-console/uploads/s2/files/note.md', 's1'),
      true,
    );
    assert.equal(
      isOtherSessionUpload('/home/u/.yi-console/uploads/s1/files/note.md', 's1'),
      false,
    );
    assert.equal(isOtherSessionUpload('D:\\Downloads\\spec.md', 's1'), false);
  });

  test('planComposerDrop keeps images inline-only, uses local paths for non-image local files, and rejects local dirs', () => {
    const files = [
      { name: 'shot.png', type: 'image/png' },
      { name: 'readme.md', type: 'text/markdown' },
      { name: 'empty.txt', type: '' },
      { name: 'docs', type: '' },
    ];
    const localPlan = planComposerDrop(files, {
      isLocalHost: true,
      resolved: [
        { path: 'D:\\tmp\\shot.png', isDir: false },
        { path: 'D:\\tmp\\readme.md', isDir: false },
        { path: 'D:\\tmp\\empty.txt', isDir: false },
        { path: 'D:\\tmp\\docs', isDir: true },
      ],
      dirHints: [false, false, false, true],
      imageTypes: IMAGE_TYPES,
      openInPanel: true,
    });
    assert.deepEqual(localPlan.localPaths, [
      undefined, // image must still be handled as inline image attachment, never opened in panel from composer
      'D:\\tmp\\readme.md',
      'D:\\tmp\\empty.txt',
      undefined,
    ]);
    assert.deepEqual(localPlan.dirHints, [false, false, false, true]);
    assert.equal(localPlan.shouldOpenInPanel, true);

    const remotePlan = planComposerDrop(files.slice(0, 2), {
      isLocalHost: false,
      resolved: [{ path: 'D:\\tmp\\shot.png', isDir: false }, { path: 'D:\\tmp\\readme.md', isDir: false }],
      dirHints: [false, false],
      imageTypes: IMAGE_TYPES,
      openInPanel: false,
    });
    assert.deepEqual(remotePlan.localPaths, [undefined, undefined]);
    assert.equal(remotePlan.shouldOpenInPanel, false);
  });

  test('planChatDrop opens all local files including images and directories directly, and uploads on remote/fallback', () => {
    const files = [
      { name: 'shot.png', type: 'image/png' },
      { name: 'main.rs', type: '' },
      { name: 'src', type: '' },
    ];
    const localResult = planChatDrop(files, {
      isLocalHost: true,
      resolved: [
        { path: 'D:\\tmp\\shot.png', isDir: false },
        { path: 'D:\\tmp\\main.rs', isDir: false },
        { path: 'D:\\tmp\\src', isDir: true },
      ],
      dirHints: [false, false, true],
    });
    assert.deepEqual(localResult, [
      { kind: 'open-local', path: 'D:\\tmp\\shot.png' },
      { kind: 'open-local', path: 'D:\\tmp\\main.rs' },
      { kind: 'open-local', path: 'D:\\tmp\\src' },
    ]);

    const remoteResult = planChatDrop(files, {
      isLocalHost: false,
      resolved: null,
      dirHints: [false, false, true],
    });
    assert.equal(remoteResult[0].kind, 'upload-and-open');
    assert.equal(remoteResult[0].file, files[0]);
    assert.equal(remoteResult[1].kind, 'upload-and-open');
    assert.equal(remoteResult[1].file, files[1]);
    assert.deepEqual(remoteResult[2], { kind: 'error-dir', name: 'src' });

    const browserLocalFallback = planChatDrop(files, {
      isLocalHost: true,
      resolved: null,
      dirHints: [false, false, true],
    });
    assert.equal(browserLocalFallback[0].kind, 'upload-and-open');
    assert.equal(browserLocalFallback[1].kind, 'upload-and-open');
    assert.deepEqual(browserLocalFallback[2], { kind: 'error-dir', name: 'src' });
  });
});
