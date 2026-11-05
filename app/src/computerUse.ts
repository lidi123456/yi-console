import type { Hello, SessionInfo } from './types';

/** Desktop capability follows the target host, including a Windows host viewed from macOS or Linux. */
export function canUseComputer(hello: Pick<Hello, 'os' | 'arch'> | undefined, info: Pick<SessionInfo, 'tool' | 'mode'>): boolean {
  return (
    hello?.os === 'windows' &&
    hello.arch === 'x86_64' &&
    info.mode === 'structured' &&
    ['claude', 'codex', 'cursor', 'kiro', 'qoder', 'codebuddy'].includes(info.tool)
  );
}
