import { getT, isEn } from '../i18n';
import { errorToast, getState, loadSession, navigate, setState, skey, toast } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type HubEvent, type SessionInfo } from '../types';
import { downloadText, isMobile, lsSet, safeFileName, sessionMarkdown, toB64 } from '../util';
import { handoffPrompt } from '../handoff';
import { writeClipboard } from '../clipboard';
import { askConfirm, askText, type MenuItem } from './Overlay';

const tx = getT('sessionActions');

/** When the session page receives this, it opens the file panel on "Changes" */
export const OPEN_CHANGES = 'yi-console:open-changes';

/** Sessions whose file panel should switch to "Changes". The event alone isn't enough: a panel that was closed
 *  mounts only after the event fired, so it takes the request from here when it mounts */
const pendingChanges = new Set<string>();
export const takeOpenChanges = (hostId: string, sessionId: string) => pendingChanges.delete(skey(hostId, sessionId));

/** View changes: go to the session page and open the "Changes" view of the file panel on the right */
export function showChanges(hostId: string, sessionId: string) {
  lsSet(`file-panel-${skey(hostId, sessionId)}`, true);
  lsSet('wp-view', 'changes');
  pendingChanges.add(skey(hostId, sessionId));
  navigate({ kind: 'session', hostId, sessionId });
  setTimeout(() => window.dispatchEvent(new CustomEvent(OPEN_CHANGES, { detail: { hostId, sessionId } })), 0);
}

// Session actions: shared by the menu at the top right of the session page and the sidebar context menu

/** For export: fetch all events of the session (opening only fetched the most recent part) */
async function allEvents(hostId: string, sessionId: string): Promise<HubEvent[]> {
  const r = await rpc<{ events: HubEvent[] }>(hostId, 'session.history', { sessionId, limit: 20000 });
  return r.events;
}

async function call(hostId: string, sessionId: string, method: string, params: Record<string, unknown> = {}, ok?: string) {
  try {
    await rpc(hostId, method, { sessionId, ...params });
    if (ok) toast(ok);
  } catch (e) {
    errorToast(e);
  }
}

export async function renameSession(hostId: string, info: SessionInfo) {
  const t = await askText({
    title: tx('renameSession'),
    value: info.title || '',
    placeholder: tx('sessionTitle'),
    okText: tx('save'),
  });
  if (t && t.trim() && t.trim() !== info.title) await call(hostId, info.id, 'session.rename', { title: t.trim() });
}

export async function exportSession(hostId: string, info: SessionInfo, how: 'copy' | 'download') {
  try {
    const md = sessionMarkdown(info, await allEvents(hostId, info.id), TOOL_LABEL[info.tool]);
    if (how === 'copy') toast((await writeClipboard(md)) ? tx('copiedAsMarkdown') : tx('failedToCopy'));
    else downloadText(`${safeFileName(info.title)}.md`, md);
  } catch (e) {
    errorToast(e);
  }
}

/** Continue with another tool: find the original session's log file (for tools like Cursor with no readable file, export to Markdown into the session's upload directory),
 *  open the new session dialog, and have the first message ask the new tool to read the log and report back */
export async function handoffSession(hostId: string, info: SessionInfo) {
  try {
    // The daemon also says whether there's any user message: a session with no conversation yet has nothing to hand off
    // (the menu already filters most by externalId; this catches ACP sessions created before anything was said, and after /clear).
    // Older daemons don't send hasUser; skip the check then
    let { path, hasUser } = await rpc<{ path: string | null; hasUser?: boolean }>(hostId, 'session.logFile', { sessionId: info.id });
    if (hasUser === false) {
      toast(tx('thisSessionHasNoConversation'));
      return;
    }
    const exported = !path;
    if (!path) {
      // No readable log file (Cursor): export the whole conversation as Markdown, which needs all events
      const done = toast(tx('loadingConversation'), 'loading');
      let events: HubEvent[];
      try {
        events = await allEvents(hostId, info.id);
      } finally {
        done();
      }
      const md = sessionMarkdown(info, events, TOOL_LABEL[info.tool]);
      const name = `${safeFileName(info.title)}.md`;
      path = (await rpc<{ path: string }>(hostId, 'session.attachFile', { sessionId: info.id, name, data: toB64(md) })).path;
    }
    setState({ newSession: { hostId, cwd: info.cwd, handoff: { from: info.tool, prompt: handoffPrompt(info, path, exported) } } });
  } catch (e) {
    errorToast(e);
  }
}

/** Delete every session in one working directory (the sidebar's directory row). Bottom-panel terminals go with their chat in Hub::delete */
export async function deleteDirSessions(hostId: string, cwd: string) {
  const list = (getState().sessions[hostId] ?? []).filter((s) => s.cwd === cwd && !s.parentId);
  if (list.length === 0) return;
  const busy = list.filter((s) => s.status === 'running' || s.status === 'starting' || s.status === 'waiting-permission').length;
  const name = cwd.split(/[\\/]/).filter(Boolean).pop() || cwd;
  const ok = await askConfirm({
    title: tx('deleteAllSessionsIn', { name, length: list.length }),
    message:
      tx('onlyRemovesTheRecordsIn') +
      (busy ? tx('ofThemAreRunningAnd', { busy }) : ''),
    okText: tx('deleteAll'),
    danger: true,
  });
  if (!ok) return;
  let failed = 0;
  for (const s of list) {
    try {
      await rpc(hostId, 'session.delete', { sessionId: s.id });
    } catch {
      failed++;
    }
  }
  const v = getState().view;
  if ('sessionId' in v && v.hostId === hostId && list.some((s) => s.id === v.sessionId)) navigate({ kind: 'home' });
  toast(
    failed
      ? tx('deletedSessionsFailed', { v: list.length - failed, failed })
      : tx('deletedSessions', { length: list.length }),
    failed ? 'error' : 'info',
  );
}

export async function deleteSession(hostId: string, info: SessionInfo) {
  const name = info.title || tx('newSession');
  const ok = await askConfirm({
    title: tx('deleteSession', { name }),
    message: tx('onlyRemovesTheRecordIn'),
    okText: tx('delete'),
    danger: true,
  });
  if (!ok) return;
  await call(hostId, info.id, 'session.delete');
  const v = getState().view;
  if ('sessionId' in v && v.sessionId === info.id) navigate({ kind: 'home' });
}

/** /clear: delete this session's chat history and reset usage; the next message starts a new tool session; the Yi Console session stays the same */
export async function clearSession(hostId: string, info: SessionInfo) {
  const busy = info.status === 'running' || info.status === 'waiting-permission' || info.status === 'starting';
  const ok = await askConfirm({
    title: tx('clearThisConversation'),
    message: isEn()
      ? `Chat history will be permanently deleted, token usage and cost reset to zero, and ${info.tool === 'claude' ? 'Claude Code' : 'the tool'} will start fresh on the next message.${busy ? '\nThe current turn in progress will be interrupted.' : ''}`
      : `聊天记录会被删除且无法恢复，token 和费用清零，下次发消息时 ${info.tool === 'claude' ? 'Claude Code' : '工具'}会从头开始，不记得之前的内容。${busy ? '\nAI 正在回复，这一轮会被中断。' : ''}`,
    okText: tx('clear'),
    danger: true,
  });
  if (!ok) return;
  await call(hostId, info.id, 'session.clear', {}, tx('conversationCleared'));
}

/** Switch the same session between chat mode and terminal mode; the tool continues its original session */
export async function switchMode(hostId: string, info: SessionInfo) {
  const to = info.mode === 'terminal' ? 'structured' : 'terminal';
  const busy = info.status === 'running' || info.status === 'waiting-permission';
  if (to === 'terminal' && busy) {
    const ok = await askConfirm({
      title: tx('switchToTerminalMode'),
      message: tx('aiIsReplyingSwitchingModes'),
      okText: tx('switch'),
    });
    if (!ok) return;
  }
  await call(hostId, info.id, 'session.setMode', { mode: to });
}

/** Open a terminal (shell session) on the host: a local terminal for this machine, a terminal on the remote machine for SSH / relay hosts */
export async function openTerminal(hostId: string, cwd = '~') {
  try {
    const dir = cwd.replace(/[\\/]+$/, '').split(/[\\/]/).pop() || cwd;
    const info = await rpc<SessionInfo>(hostId, 'session.create', {
      tool: 'shell',
      cwd,
      mode: 'terminal',
      title: `${tx('terminal')} · ${dir}`,
      cols: 120,
      rows: 32,
    });
    navigate({ kind: 'session', hostId, sessionId: info.id });
  } catch (e) {
    errorToast(e);
  }
}

/** Command to continue this session from the command line (the tool's own session ID) */
function resumeCommand(info: SessionInfo): string | null {
  const id = info.externalId;
  if (!id) return null;
  switch (info.tool) {
    case 'claude':
      return `claude --resume ${id}`;
    case 'codex':
      return `codex resume ${id}`;
    case 'cursor':
      return `cursor-agent --resume ${id}`;
    case 'kiro':
      return `kiro-cli chat --resume-id ${id}`;
    case 'qoder':
      return `qodercli --resume ${id}`;
    case 'codebuddy':
      return `codebuddy --resume ${id}`;
    default:
      return null;
  }
}

/** Copy session IDs: the tool's own ID (absent before a tool session exists), the resume command, the Yi Console internal ID */
function idItems(info: SessionInfo): MenuItem[] {
  const copy = (text: string, msg: string) => () => writeClipboard(text).then(() => toast(msg));
  const cmd = resumeCommand(info);
  const tool = TOOL_LABEL[info.tool];
  return [
    ...(info.externalId
      ? [
          {
            label: tx('copySessionId', { tool }),
            icon: 'copy',
            hint: info.externalId.slice(0, 8),
            run: copy(info.externalId, tx('copiedSessionId', { tool })),
          },
        ]
      : []),
    ...(cmd ? [{ label: tx('copyResumeCommand'), icon: 'terminal', run: copy(cmd, tx('copied', { cmd })) }] : []),
    {
      label: tx('copyYiConsoleSessionId'),
      icon: 'copy',
      hint: info.id.slice(0, 8),
      run: copy(info.id, tx('copiedYiConsoleSessionId')),
    },
  ];
}

/** Files sessions have no process or conversation: only the session-record actions and directory shortcuts make sense */
function filesMenu(hostId: string, info: SessionInfo, inSession?: boolean): (MenuItem | '-')[] {
  const sid = info.id;
  return [
    ...(inSession ? [] : [{ label: tx('open'), icon: 'folder', run: () => navigate({ kind: 'session', hostId, sessionId: sid }) }]),
    {
      label: tx('rename'),
      icon: 'edit',
      hint: isMobile() ? undefined : inSession ? tx('doubleClickTitle') : 'F2',
      run: () => renameSession(hostId, info),
    },
    { label: info.pinned ? tx('unpin') : tx('pin'), icon: 'pin', run: () => call(hostId, sid, 'session.pin', { pinned: !info.pinned }) },
    '-',
    ...(inSession ? [] : [{ label: tx('viewChanges'), icon: 'diff', run: () => showChanges(hostId, sid) }]),
    { label: tx('portForwarding'), icon: 'external', run: () => navigate({ kind: 'ports', hostId }) },
    { label: tx('copyDirectoryPath'), icon: 'copy', run: () => writeClipboard(info.cwd).then(() => toast(tx('pathCopied'))) },
    '-',
    { label: tx('deleteSession2'), icon: 'trash', danger: true, run: () => deleteSession(hostId, info) },
  ];
}

export function sessionMenu(
  hostId: string,
  info: SessionInfo,
  opts: {
    inSession?: boolean;
    onUsage?: () => void;
    style?: 'modern' | 'claude';
    onToggleStyle?: () => void;
    /** On mobile the bottom terminal panel toggle lives in the menu (its top-bar spot is taken by usage) */
    term?: boolean;
    onToggleTerm?: () => void;
  } = {},
): (MenuItem | '-')[] {
  const alive = info.status !== 'exited' && info.status !== 'error';
  const sid = info.id;
  const mobile = isMobile();
  if (info.tool === 'files') return filesMenu(hostId, info, opts.inSession);
  return [
    ...(opts.inSession ? [] : [{ label: tx('open'), icon: 'chat', run: () => navigate({ kind: 'session', hostId, sessionId: sid }) }]),
    {
      label: tx('rename'),
      icon: 'edit',
      hint: mobile ? undefined : opts.inSession ? tx('doubleClickTitle') : 'F2',
      run: () => renameSession(hostId, info),
    },
    {
      label: info.pinned ? tx('unpin') : tx('pin'),
      icon: 'pin',
      run: () => call(hostId, sid, 'session.pin', { pinned: !info.pinned }),
    },
    ...(info.tool !== 'shell' && !info.parentId
      ? [
          info.mode === 'terminal'
            ? { label: tx('switchToChatMode'), icon: 'chat', run: () => switchMode(hostId, info) }
            : { label: tx('switchToTerminalMode2'), icon: 'terminal', run: () => switchMode(hostId, info) },
        ]
      : []),
    ...(opts.onToggleStyle && info.mode === 'structured' && info.tool !== 'shell'
      ? [
          {
            label: opts.style === 'modern' ? tx('replyStyleSwitchToTimeline') : tx('replyStyleSwitchToMinimal'),
            icon: opts.style === 'modern' ? 'outline' : 'rows',
            run: opts.onToggleStyle,
          },
        ]
      : []),
    ...(info.tool !== 'shell' && info.mode !== 'terminal' ? [{ label: tx('clearConversation'), icon: 'trash', run: () => clearSession(hostId, info) }] : []),
    // No tool session id means nothing has been said yet (or just /clear-ed), so there's nothing to hand off
    ...(info.tool !== 'shell' && info.mode === 'structured' && !info.parentId && info.externalId
      ? [{ label: tx('continueInAnotherTool'), icon: 'forward', run: () => handoffSession(hostId, info) }]
      : []),
    '-',
    // The session page has its own "View Changes" button in the top bar
    ...(opts.inSession ? [] : [{ label: tx('viewChanges'), icon: 'diff', run: () => showChanges(hostId, sid) }]),
    // The mobile top bar already has a usage button, so don't repeat it in the menu; the terminal panel is the reverse and only appears in the mobile menu
    ...(opts.onUsage && info.tool !== 'shell' && !mobile ? [{ label: tx('viewUsage'), icon: 'chart', run: opts.onUsage }] : []),
    ...(opts.onToggleTerm && info.tool !== 'shell' && mobile
      ? [{ label: opts.term ? tx('hideTerminalPanel') : tx('openTerminalPanel'), icon: 'terminal', run: opts.onToggleTerm }]
      : []),
    { label: tx('copyAsMarkdown'), icon: 'copy', run: () => exportSession(hostId, info, 'copy') },
    { label: tx('exportMarkdownFile'), icon: 'download', run: () => exportSession(hostId, info, 'download') },
    { label: tx('browseWorkingDirectory'), icon: 'folder', run: () => navigate({ kind: 'files', hostId, path: info.cwd }) },
    { label: tx('portForwarding'), icon: 'external', run: () => navigate({ kind: 'ports', hostId }) },
    { label: tx('copyDirectoryPath'), icon: 'copy', run: () => writeClipboard(info.cwd).then(() => toast(tx('pathCopied'))) },
    ...idItems(info),
    '-',
    ...(alive ? [{ label: tx('stopProcess'), icon: 'power', run: () => call(hostId, sid, 'session.kill', {}, tx('stopped')) }] : []),
    ...(opts.inSession
      ? [{ label: tx('reload'), icon: 'refresh', run: () => loadSession(hostId, sid, true).catch(errorToast) }]
      : []),
    { label: tx('deleteSession2'), icon: 'trash', danger: true, run: () => deleteSession(hostId, info) },
  ];
}
