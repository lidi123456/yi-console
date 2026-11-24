import { beforeEach, describe, test } from 'node:test';
import assert from 'node:assert/strict';
import {
  _resetComposerStateForTests,
  addComposerUpload,
  armWaitSend,
  beginSend,
  beginUploadJob,
  cancelComposerUpload,
  clearComposerState,
  completeSendSuccess,
  endSend,
  endUploadJob,
  getComposerSnapshot,
  insertOrAppendComposerText,
  isComposerUploadCancelled,
  registerActiveComposer,
  removeComposerUpload,
  setComposerImages,
  setComposerRefs,
  setComposerText,
  subscribeComposer,
  updateComposerUpload,
} from '../src/composerState.ts';

function installMockSessionStorage() {
  const store = new Map();
  globalThis.sessionStorage = {
    get length() {
      return store.size;
    },
    clear() {
      store.clear();
    },
    getItem(key) {
      return store.has(key) ? store.get(key) : null;
    },
    setItem(key, value) {
      store.set(key, String(value));
    },
    removeItem(key) {
      store.delete(key);
    },
    key(index) {
      return [...store.keys()][index] ?? null;
    },
  };
  return store;
}

const makeImg = (name) => ({
  mimeType: 'image/png',
  data: 'AAAA',
  url: `data:image/png;base64,${name}`,
  name,
});

const makeRef = (id, path = 'src/app.ts') => ({
  id,
  kind: 'code',
  path,
  fromLine: 1,
  toLine: 10,
  content: 'const x = 1;',
});

describe('composerState', () => {
  let storage;

  beforeEach(() => {
    storage = installMockSessionStorage();
    _resetComposerStateForTests();
  });

  test('switching away and back mid-send preserves sending state and clears prompt atomically on completion', () => {
    const keyA = 'h1::s1';
    const keyB = 'h1::s2';
    const img1 = makeImg('shot1.png');
    const ref1 = makeRef('r1');

    let calls1 = 0;
    const unsub1 = subscribeComposer(keyA, () => {
      calls1 += 1;
    });

    setComposerText(keyA, 'check this screenshot');
    setComposerImages(keyA, [img1]);
    setComposerRefs(keyA, [ref1]);
    assert.equal(storage.get(`draft-${keyA}`), 'check this screenshot');
    assert.ok(storage.get(`draft-refs-${keyA}`));

    assert.equal(beginSend(keyA), true);
    assert.equal(beginSend(keyA), false, 'duplicate send while already transmitting must be rejected');

    // Switch away to session B (session A Composer unmounts)
    unsub1();
    const unsubB = subscribeComposer(keyB, () => {});
    assert.equal(getComposerSnapshot(keyB).sending, false);
    assert.equal(getComposerSnapshot(keyB).text, '');

    // Switch back to session A while send is still in flight (fresh Composer mounts)
    unsubB();
    let calls2 = 0;
    const unsub2 = subscribeComposer(keyA, () => {
      calls2 += 1;
    });
    const remounted = getComposerSnapshot(keyA);
    assert.equal(remounted.sending, true);
    assert.equal(remounted.text, 'check this screenshot');
    assert.deepEqual(remounted.imgs, [img1]);
    assert.deepEqual(remounted.refs, [ref1]);
    assert.equal(storage.get(`draft-${keyA}`), 'check this screenshot');

    // In-flight RPC resolves and clears the sent prompt + attachments
    completeSendSuccess(keyA, {
      sentText: 'check this screenshot',
      sentImgs: [img1],
      sentRefs: [ref1],
    });
    assert.equal(calls2, 1);
    assert.equal(calls1, 4, 'unmounted listener must not receive post-unmount updates');

    const after = getComposerSnapshot(keyA);
    assert.equal(after.sending, false);
    assert.equal(after.text, '');
    assert.deepEqual(after.imgs, []);
    assert.deepEqual(after.refs, []);
    assert.equal(storage.has(`draft-${keyA}`), false);
    assert.equal(storage.has(`draft-refs-${keyA}`), false);

    unsub2();
  });

  test('completing send while viewing another session leaves clean draft on return', () => {
    const keyA = 'h1::s1';
    const img1 = makeImg('shot1.png');
    const ref1 = makeRef('r1');

    const unsub1 = subscribeComposer(keyA, () => {});
    setComposerText(keyA, 'analyze image');
    setComposerImages(keyA, [img1]);
    setComposerRefs(keyA, [ref1]);
    assert.equal(beginSend(keyA), true);

    // Switch away before send completes
    unsub1();

    completeSendSuccess(keyA, {
      sentText: 'analyze image',
      sentImgs: [img1],
      sentRefs: [ref1],
    });

    // Return to session A later
    const snap = getComposerSnapshot(keyA);
    assert.equal(snap.sending, false);
    assert.equal(snap.text, '');
    assert.deepEqual(snap.imgs, []);
    assert.deepEqual(snap.refs, []);
    assert.equal(storage.has(`draft-${keyA}`), false);
    assert.equal(storage.has(`draft-refs-${keyA}`), false);
  });

  test('send failure clears sending flag while preserving draft text, images, and refs across remount', () => {
    const keyA = 'h1::s1';
    const img1 = makeImg('shot1.png');
    const ref1 = makeRef('r1');

    const unsub1 = subscribeComposer(keyA, () => {});
    setComposerText(keyA, 'retry me');
    setComposerImages(keyA, [img1]);
    setComposerRefs(keyA, [ref1]);
    assert.equal(beginSend(keyA), true);

    // Switch away and back
    unsub1();
    let notifications = 0;
    const unsub2 = subscribeComposer(keyA, () => {
      notifications += 1;
    });
    assert.equal(getComposerSnapshot(keyA).sending, true);

    // Send fails -> endSend resets sending without clearing drafts
    endSend(keyA);
    assert.equal(notifications, 1);
    const snap = getComposerSnapshot(keyA);
    assert.equal(snap.sending, false);
    assert.equal(snap.text, 'retry me');
    assert.deepEqual(snap.imgs, [img1]);
    assert.deepEqual(snap.refs, [ref1]);
    assert.equal(storage.get(`draft-${keyA}`), 'retry me');
    assert.ok(storage.get(`draft-refs-${keyA}`));

    unsub2();
  });

  test('mid-send edits keep new text, images, and refs while removing sent ones', () => {
    const keyA = 'h1::s1';
    const img1 = makeImg('first.png');
    const img2 = makeImg('second.png');
    const ref1 = makeRef('r1', 'a.ts');
    const ref2 = makeRef('r2', 'b.ts');

    const unsub = subscribeComposer(keyA, () => {});
    setComposerText(keyA, 'first prompt');
    setComposerImages(keyA, [img1]);
    setComposerRefs(keyA, [ref1]);
    assert.equal(beginSend(keyA), true);

    // User switches back or edits while send is in flight
    setComposerText(keyA, 'follow up prompt');
    setComposerImages(keyA, (cur) => [...cur, img2]);
    setComposerRefs(keyA, (cur) => [...cur, ref2]);

    completeSendSuccess(keyA, {
      sentText: 'first prompt',
      sentImgs: [img1],
      sentRefs: [ref1],
    });

    const snap = getComposerSnapshot(keyA);
    assert.equal(snap.sending, false);
    assert.equal(snap.text, 'follow up prompt');
    assert.deepEqual(snap.imgs, [img2]);
    assert.deepEqual(snap.refs, [ref2]);
    assert.equal(storage.get(`draft-${keyA}`), 'follow up prompt');
    assert.deepEqual(JSON.parse(storage.get(`draft-refs-${keyA}`)), [ref2]);

    unsub();
  });

  test('file upload progress, cancel, and waitSend survive session switches', () => {
    const keyA = 'h1::s1';

    const unsub1 = subscribeComposer(keyA, () => {});
    beginUploadJob(keyA);
    addComposerUpload(keyA, { id: 101, name: 'big.bin', sent: 0, total: 1000 });
    updateComposerUpload(keyA, 101, 500);

    let fired = 0;
    armWaitSend(keyA, () => {
      fired += 1;
    });

    // Switch away and back while upload is running
    unsub1();
    const unsub2 = subscribeComposer(keyA, () => {});
    const mid = getComposerSnapshot(keyA);
    assert.equal(mid.waitSend, true);
    assert.equal(mid.pendingUploads, 1);
    assert.deepEqual(mid.uploads, [{ id: 101, name: 'big.bin', sent: 500, total: 1000 }]);

    // Cancelling marks upload cancelled and disarms waitSend
    cancelComposerUpload(keyA, 101);
    assert.equal(isComposerUploadCancelled(keyA, 101), true);
    assert.equal(getComposerSnapshot(keyA).waitSend, false);
    assert.deepEqual(getComposerSnapshot(keyA).uploads, []);
    removeComposerUpload(keyA, 101);
    assert.equal(isComposerUploadCancelled(keyA, 101), false);
    assert.equal(endUploadJob(keyA), null);
    assert.equal(fired, 0);

    // Completing an upload while away appends @path to draft text and fires waitSend runner
    setComposerText(keyA, 'inspect');
    beginUploadJob(keyA);
    addComposerUpload(keyA, { id: 102, name: 'trace.log', sent: 0, total: 200 });
    let capturedTextAtSend = '';
    armWaitSend(keyA, () => {
      capturedTextAtSend = getComposerSnapshot(keyA).text;
      fired += 1;
    });

    // Switch away before upload finishes (active composer unregisters)
    const inserted = [];
    const unregister1 = registerActiveComposer(keyA, {
      insertAtCaret: (s) => inserted.push(s),
      focus: () => {},
    });
    // Simulate a second mount before first cleanup runs: cleanup must not clobber newer handler
    const inserted2 = [];
    const h2 = { insertAtCaret: (s) => inserted2.push(s), focus: () => {} };
    const unregister2 = registerActiveComposer(keyA, h2);
    unregister1();
    insertOrAppendComposerText(keyA, '@/tmp/early.txt ');
    assert.deepEqual(inserted, []);
    assert.deepEqual(inserted2, ['@/tmp/early.txt ']);

    unregister2();
    unsub2();

    // Now no active composer for keyA; upload finishes while away
    removeComposerUpload(keyA, 102);
    insertOrAppendComposerText(keyA, '@/tmp/trace.log ');
    const runner = endUploadJob(keyA);
    assert.equal(typeof runner, 'function');
    assert.equal(getComposerSnapshot(keyA).waitSend, false);
    runner();
    assert.equal(fired, 1);
    assert.equal(capturedTextAtSend, 'inspect @/tmp/trace.log ');
  });

  test('hydrates from sessionStorage without wiping, maintains snapshot identity, and clears on session removal', () => {
    const keyA = 'h1::s1';
    const ref1 = makeRef('r1', 'README.md');
    storage.set(`draft-${keyA}`, 'persisted text');
    storage.set(`draft-refs-${keyA}`, JSON.stringify([ref1]));

    const snap1 = getComposerSnapshot(keyA);
    const snap2 = getComposerSnapshot(keyA);
    assert.equal(snap1, snap2, 'unchanged getComposerSnapshot must return the exact same reference');
    assert.equal(snap1.text, 'persisted text');
    assert.deepEqual(snap1.refs, [ref1]);
    assert.equal(storage.get(`draft-${keyA}`), 'persisted text');

    const unsub = subscribeComposer(keyA, () => {});
    setComposerImages(keyA, [makeImg('x.png')]);
    addComposerUpload(keyA, { id: 201, name: 'x.zip', sent: 10, total: 100 });
    armWaitSend(keyA, () => {});

    clearComposerState(keyA);
    const cleared = getComposerSnapshot(keyA);
    assert.equal(cleared.text, '');
    assert.deepEqual(cleared.imgs, []);
    assert.deepEqual(cleared.refs, []);
    assert.deepEqual(cleared.uploads, []);
    assert.equal(cleared.waitSend, false);
    assert.equal(isComposerUploadCancelled(keyA, 201), true);
    assert.equal(storage.has(`draft-${keyA}`), false);
    assert.equal(storage.has(`draft-refs-${keyA}`), false);

    unsub();
  });
});
