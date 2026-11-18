import type { Hello, SessionInfo } from './types';

/** The AI browser runs Chrome on the session's host, so any desktop OS works; Android hosts have none */
export function canUseBrowser(hello: Pick<Hello, 'os'> | undefined, info: Pick<SessionInfo, 'tool' | 'mode'>): boolean {
  return (
    !!hello &&
    hello.os !== 'android' &&
    info.mode === 'structured' &&
    ['claude', 'codex', 'cursor', 'kiro', 'qoder', 'codebuddy', 'antigravity'].includes(info.tool)
  );
}
