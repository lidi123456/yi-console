import type { Hello, SessionInfo } from './types';

/** Desktop capability follows the target host, including a Windows host viewed from macOS or Linux. macOS hosts
 *  need the desktop app installed there (it ships the driver); the daemon reports `driverMissing` otherwise. */
export function canUseComputer(hello: Pick<Hello, 'os' | 'arch'> | undefined, info: Pick<SessionInfo, 'tool' | 'mode'>): boolean {
  const host =
    (hello?.os === 'windows' && hello.arch === 'x86_64') || (hello?.os === 'macos' && (hello.arch === 'x86_64' || hello.arch === 'aarch64'));
  return host && info.mode === 'structured' && ['claude', 'codex', 'cursor', 'kiro', 'qoder', 'codebuddy'].includes(info.tool);
}
