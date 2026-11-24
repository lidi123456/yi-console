import { beforeEach, describe, test } from 'node:test';
import assert from 'node:assert/strict';
import {
  _resetComposerStateForTests,
  addComposerUpload,
  armWaitSend,
  beginComposerReedit,
  beginSend,
  beginUploadJob,
  cancelComposerReedit,
  cancelComposerUpload,
  clearComposerState,
  completeSendSuccess,
  dismissComposerReedit,
  endSend,
  endUploadJob,
  finishReeditImages,
  getComposerSnapshot,
  insertOrAppendComposerText,
  isComposerUploadCancelled,
  registerActiveComposer,
  removeComposerUpload,
  restoreReeditDraft,
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

  test('idle snapshot keeps stable identity across unsubscribing last subscriber and clearing active draft', () => {
    const idleKey = 'h1::idle';
    const snapIdle1 = getComposerSnapshot(idleKey);
    const unsubIdle = subscribeComposer(idleKey, () => {});
    const snapIdle2 = getComposerSnapshot(idleKey);
    unsubIdle();
    const snapIdle3 = getComposerSnapshot(idleKey);
    assert.equal(snapIdle1, snapIdle2);
    assert.equal(snapIdle2, snapIdle3);

    const activeKey = 'h1::active-then-idle';
    const unsubActive = subscribeComposer(activeKey, () => {});
    setComposerText(activeKey, 'temporary draft');
    assert.equal(getComposerSnapshot(activeKey).text, 'temporary draft');
    clearComposerState(activeKey);
    const clearedBeforeUnsub = getComposerSnapshot(activeKey);
    unsubActive();
    const clearedAfterUnsub = getComposerSnapshot(activeKey);
    assert.equal(clearedBeforeUnsub, clearedAfterUnsub);
    assert.equal(clearedAfterUnsub, snapIdle1);
  });

  test('re-edit on empty draft sets active state and cancel clears composer', () => {
    const keyA = 'h1::s1';
    const ref1 = makeRef('r1', 'ChatView.tsx');
    const res = beginComposerReedit(keyA, {
      text: 'fix button',
      refs: [ref1],
      loadingImages: 0,
    });
    assert.ok(res.nonce > 0);
    assert.equal(res.hasStashedDraft, false);
    const snap = getComposerSnapshot(keyA);
    assert.equal(snap.text, 'fix button');
    assert.deepEqual(snap.refs, [ref1]);
    assert.equal(snap.reedit?.nonce, res.nonce);
    assert.equal(snap.reedit?.stashedDraft, null);
    assert.equal(storage.get(`draft-${keyA}`), 'fix button');

    cancelComposerReedit(keyA);
    const afterCancel = getComposerSnapshot(keyA);
    assert.equal(afterCancel.text, '');
    assert.deepEqual(afterCancel.imgs, []);
    assert.deepEqual(afterCancel.refs, []);
    assert.equal(afterCancel.reedit, null);
    assert.equal(storage.has(`draft-${keyA}`), false);
    assert.equal(storage.has(`draft-refs-${keyA}`), false);
  });

  test('re-edit stashes active draft, preserves it when switching messages, and restores intact', () => {
    const keyA = 'h1::s1';
    const draftImg = makeImg('draft.png');
    const draftRef = makeRef('d1', 'Draft.tsx');
    const msg1Ref = makeRef('m1', 'One.tsx');
    const msg2Ref = makeRef('m2', 'Two.tsx');

    setComposerText(keyA, 'unfinished draft');
    setComposerImages(keyA, [draftImg]);
    setComposerRefs(keyA, [draftRef]);

    const first = beginComposerReedit(keyA, {
      text: 'first prompt',
      refs: [msg1Ref],
    });
    assert.equal(first.hasStashedDraft, true);
    const duringFirst = getComposerSnapshot(keyA);
    assert.equal(duringFirst.text, 'first prompt');
    assert.deepEqual(duringFirst.imgs, []);
    assert.deepEqual(duringFirst.refs, [msg1Ref]);
    assert.equal(duringFirst.reedit?.stashedDraft?.text, 'unfinished draft');
    assert.deepEqual(duringFirst.reedit?.stashedDraft?.imgs, [draftImg]);
    assert.deepEqual(duringFirst.reedit?.stashedDraft?.refs, [draftRef]);

    // Switching straight to another message must keep the original stashed draft
    const second = beginComposerReedit(keyA, {
      text: 'second prompt',
      refs: [msg2Ref],
    });
    assert.equal(second.hasStashedDraft, true);
    const duringSecond = getComposerSnapshot(keyA);
    assert.equal(duringSecond.text, 'second prompt');
    assert.equal(duringSecond.reedit?.nonce, second.nonce);
    assert.equal(duringSecond.reedit?.stashedDraft?.text, 'unfinished draft');

    // Stale toast nonce must not clobber newer re-edit
    assert.equal(restoreReeditDraft(keyA, first.nonce), false);
    assert.equal(getComposerSnapshot(keyA).text, 'second prompt');

    assert.equal(restoreReeditDraft(keyA, second.nonce), true);
    const restored = getComposerSnapshot(keyA);
    assert.equal(restored.reedit, null);
    assert.equal(restored.text, 'unfinished draft');
    assert.deepEqual(restored.imgs, [draftImg]);
    assert.deepEqual(restored.refs, [draftRef]);
    assert.equal(storage.get(`draft-${keyA}`), 'unfinished draft');
    assert.deepEqual(JSON.parse(storage.get(`draft-refs-${keyA}`)), [draftRef]);
  });

  test('dismissComposerReedit clears reedit state while preserving edited text and refs', () => {
    const keyA = 'h1::s1';
    const ref1 = makeRef('r1', 'App.tsx');
    setComposerText(keyA, 'old draft');
    beginComposerReedit(keyA, {
      text: 'old prompt',
      refs: [ref1],
    });
    setComposerText(keyA, 'edited prompt');
    dismissComposerReedit(keyA);
    const snap = getComposerSnapshot(keyA);
    assert.equal(snap.reedit, null);
    assert.equal(snap.text, 'edited prompt');
    assert.deepEqual(snap.refs, [ref1]);
    assert.equal(restoreReeditDraft(keyA), false);
  });

  test('finishReeditImages respects nonce and blocks stale hydration after cancel, switch, or send', () => {
    const keyA = 'h1::s1';
    const img1 = makeImg('hist1.png');
    const img2 = makeImg('hist2.png');

    const { nonce: n1 } = beginComposerReedit(keyA, {
      text: 'with image',
      refs: [],
      loadingImages: 1,
    });
    assert.equal(getComposerSnapshot(keyA).reedit?.loadingImages, 1);

    // Switch to another re-edit before image 1 finishes
    const { nonce: n2 } = beginComposerReedit(keyA, {
      text: 'newer prompt',
      refs: [],
      loadingImages: 1,
    });
    // Stale completion for n1 must be ignored
    finishReeditImages(keyA, n1, [img1]);
    assert.deepEqual(getComposerSnapshot(keyA).imgs, []);
    assert.equal(getComposerSnapshot(keyA).reedit?.loadingImages, 1);

    // Matching completion for n2 attaches image and clears loadingImages
    finishReeditImages(keyA, n2, [img2]);
    assert.deepEqual(getComposerSnapshot(keyA).imgs, [img2]);
    assert.equal(getComposerSnapshot(keyA).reedit?.loadingImages, 0);

    // Cancel clears everything and ignores any subsequent stale image load
    const { nonce: n3 } = beginComposerReedit(keyA, {
      text: 'cancelled prompt',
      refs: [],
      loadingImages: 1,
    });
    cancelComposerReedit(keyA);
    finishReeditImages(keyA, n3, [img1]);
    assert.deepEqual(getComposerSnapshot(keyA).imgs, []);
    assert.equal(getComposerSnapshot(keyA).reedit, null);
  });

  test('completeSendSuccess clears matching reedit nonce and clearComposerState resets reedit', () => {
    const keyA = 'h1::s1';
    const { nonce: n1 } = beginComposerReedit(keyA, {
      text: 'prompt 20',
      refs: [],
    });
    assert.equal(beginSend(keyA), true);

    // User opens a newer re-edit while previous send is still in flight
    const { nonce: n2 } = beginComposerReedit(keyA, {
      text: 'prompt 21',
      refs: [],
    });

    completeSendSuccess(keyA, {
      sentText: 'prompt 20',
      sentImgs: [],
      sentRefs: [],
      reeditNonce: n1,
    });
    const afterFirstSend = getComposerSnapshot(keyA);
    assert.equal(afterFirstSend.reedit?.nonce, n2);
    assert.equal(afterFirstSend.text, 'prompt 21');

    // Sending n2 clears reedit
    assert.equal(beginSend(keyA), true);
    completeSendSuccess(keyA, {
      sentText: 'prompt 21',
      sentImgs: [],
      sentRefs: [],
      reeditNonce: n2,
    });
    assert.equal(getComposerSnapshot(keyA).reedit, null);

    // clearComposerState clears active reedit
    beginComposerReedit(keyA, {
      text: 'prompt 22',
      refs: [],
    });
    clearComposerState(keyA);
    assert.equal(getComposerSnapshot(keyA).reedit, null);
  });

  test('re-edit dirty tracking skips unchanged re-filled prompts and stashes user-edited content', () => {
    const keyA = 'h1::s1';
    const img1 = makeImg('hist.png');

    // Empty composer -> msg #1 (+ async image hydration) -> immediate msg #2 must NOT stash msg #1
    const res1 = beginComposerReedit(keyA, {
      text: 'msg 1',
      refs: [],
      loadingImages: 1,
    });
    assert.equal(res1.hasStashedDraft, false);
    finishReeditImages(keyA, res1.nonce, [img1]);
    assert.equal(getComposerSnapshot(keyA).reedit?.dirty, false);

    const res2 = beginComposerReedit(keyA, {
      text: 'msg 2',
      refs: [],
    });
    assert.equal(res2.hasStashedDraft, false);
    assert.equal(getComposerSnapshot(keyA).reedit?.stashedDraft, null);

    // User edits the refilled text -> triggering re-edit #3 stashes the edited text
    setComposerText(keyA, 'edited draft');
    assert.equal(getComposerSnapshot(keyA).reedit?.dirty, true);
    const res3 = beginComposerReedit(keyA, {
      text: 'msg 3',
      refs: [],
    });
    assert.equal(res3.hasStashedDraft, true);
    assert.equal(getComposerSnapshot(keyA).reedit?.stashedDraft?.text, 'edited draft');
    assert.equal(restoreReeditDraft(keyA, res3.nonce), true);
    assert.equal(getComposerSnapshot(keyA).text, 'edited draft');

    // Starting from an existing draft, tweaking a refilled prompt still restores orig draft when undone directly,
    // or stashes tweaked text if superseded by another re-edit
    const keyB = 'h1::s2';
    setComposerText(keyB, 'orig draft');
    const b1 = beginComposerReedit(keyB, { text: 'prompt A', refs: [] });
    assert.equal(b1.hasStashedDraft, true);
    setComposerText(keyB, 'tweaked msg1');
    assert.equal(restoreReeditDraft(keyB, b1.nonce), true);
    assert.equal(getComposerSnapshot(keyB).text, 'orig draft');

    const b2 = beginComposerReedit(keyB, { text: 'prompt B', refs: [] });
    setComposerText(keyB, 'tweaked msg1');
    const b3 = beginComposerReedit(keyB, { text: 'prompt C', refs: [] });
    assert.equal(b3.hasStashedDraft, true);
    assert.notEqual(b2.nonce, b3.nonce);
    assert.equal(restoreReeditDraft(keyB, b3.nonce), true);
    assert.equal(getComposerSnapshot(keyB).text, 'tweaked msg1');
  });
});
