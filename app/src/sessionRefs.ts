import { getComposerSnapshot, setComposerRefs } from './composerState';
import { nextContextRefId, type ContextRef, type ContextRefInput } from './contextRefs';
import { getT } from './i18n';
import { errorToast, toast } from './store';
import { rpc } from './transport';
import { TOOL_LABEL, type SessionInfo } from './types';

// `@` references to other sessions on the same host. Picking one adds the pill to the draft at once (content filled
// in when the daemon's digest arrives; sending waits for it). The referenced ids go with the message as
// `session.send` `readSessions`, and the daemon writes the read grants when the message is actually delivered
// (`Queued::reads`), so a failed send, a queued message that is withdrawn, or a hand-off never leaves a stray grant.

const t = getT('composer');

/** Digest requests still in flight, by pill id */
const loading = new Map<string, Promise<void>>();

/** Whether `info` can reference other sessions: AI chat sessions that mount yi_session (not Bots or review side chats) */
export function canReferenceSessions(info: SessionInfo): boolean {
  return info.mode === 'structured' && info.tool !== 'shell' && info.tool !== 'files' && !info.bot && !info.review;
}

/** Sessions offered in the `@` picker: other top-level AI sessions on the host, same working directory first */
export function referenceableSessions(all: readonly SessionInfo[] | undefined, info: SessionInfo): SessionInfo[] {
  if (!all) return [];
  const own = info.parentId ?? info.id;
  return all
    .filter((s) => !s.parentId && !s.bot && s.id !== own && s.tool !== 'shell' && s.tool !== 'files')
    .sort((a, b) => Number(b.cwd === info.cwd) - Number(a.cwd === info.cwd) || b.updatedAt - a.updatedAt);
}

export function buildSessionContextRef(target: Pick<SessionInfo, 'id' | 'title' | 'tool'>, digest: string): ContextRefInput {
  const content = digest.trim();
  return {
    kind: 'session',
    path: target.title?.trim() || t('untitledSessionRef'),
    fromLine: 1,
    toLine: Math.max(1, content.split('\n').length),
    comment: TOOL_LABEL[target.tool],
    sideSessionId: target.id,
    content,
  };
}

/** Fetch the daemon's digest of a session (`session.digest`); fails on daemons without session references */
export async function sessionDigest(hostId: string, sessionId: string): Promise<string> {
  return (await rpc<{ text: string }>(hostId, 'session.digest', { sessionId })).text;
}

/** `@` picker: add a session pill to the draft of `callerId` now, and fill in its digest when it arrives. The draft
 *  store is keyed by session, so this works even if the composer is unmounted meanwhile */
export function referenceSession(hostId: string, callerId: string, target: SessionInfo): void {
  const key = `${hostId}::${callerId}`;
  if (getComposerSnapshot(key).refs.some((r) => r.kind === 'session' && r.sideSessionId === target.id)) return;
  const id = nextContextRefId();
  setComposerRefs(key, (cur) => [...cur, { ...buildSessionContextRef(target, ''), id }]);
  const job = sessionDigest(hostId, target.id)
    .then((digest) => {
      const filled = buildSessionContextRef(target, digest);
      setComposerRefs(key, (cur) => cur.map((r) => (r.id === id ? { ...filled, id } : r)));
    })
    .catch((e) => {
      // Older daemons have no session references (no digest, no read grants): drop the pill rather than send a dead pointer
      setComposerRefs(key, (cur) => cur.filter((r) => r.id !== id));
      if (/未知方法|unknown method/i.test(String(e))) toast(t('sessionRefUnsupported'));
      else errorToast(e);
    })
    .finally(() => loading.delete(id));
  loading.set(id, job);
}

/** Before sending: wait for the digests of session pills still loading */
export async function settleSessionRefs(refs: readonly ContextRef[]): Promise<void> {
  await Promise.all(refs.map((r) => loading.get(r.id)).filter(Boolean));
}

/** Session ids a message references, for `session.send` `readSessions` */
export function sessionRefIds(refs: readonly ContextRef[]): string[] {
  return [...new Set(refs.filter((r) => r.kind === 'session' && r.sideSessionId).map((r) => r.sideSessionId!))];
}
