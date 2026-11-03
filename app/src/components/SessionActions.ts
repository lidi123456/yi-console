import { isEn, tr } from '../i18n';
import { errorToast, getState, loadSession, navigate, setState, skey, toast } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type HubEvent, type SessionInfo } from '../types';
import { downloadText, isMobile, lsSet, safeFileName, sessionMarkdown, toB64 } from '../util';
import { handoffPrompt } from '../handoff';
import { writeClipboard } from '../clipboard';
import { askConfirm, askText, type MenuItem } from './Overlay';

/** When the session page receives this, it opens the file panel on "Changes" */
export const OPEN_CHANGES = 'yi-console:open-changes';

/** View changes: go to the session page and open the "Changes" view of the file panel on the right */
export function showChanges(hostId: string, sessionId: string) {
  lsSet(`file-panel-${skey(hostId, sessionId)}`, true);
  lsSet('wp-view', 'changes');
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
    title: tr('重命名会话', 'Rename Session'),
    value: info.title || '',
    placeholder: tr('会话名称', 'Session title'),
    okText: tr('保存', 'Save'),
  });
  if (t && t.trim() && t.trim() !== info.title) await call(hostId, info.id, 'session.rename', { title: t.trim() });
}

export async function exportSession(hostId: string, info: SessionInfo, how: 'copy' | 'download') {
  try {
    const md = sessionMarkdown(info, await allEvents(hostId, info.id), TOOL_LABEL[info.tool]);
    if (how === 'copy') toast((await writeClipboard(md)) ? tr('已复制为 Markdown', 'Copied as Markdown') : tr('复制失败', 'Failed to copy'));
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
      toast(tr('这个会话还没有对话内容', 'This session has no conversation yet'));
      return;
    }
    const exported = !path;
    if (!path) {
      // No readable log file (Cursor): export the whole conversation as Markdown, which needs all events
      const done = toast(tr('正在读取会话内容…', 'Loading conversation…'), 'loading');
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

export async function deleteSession(hostId: string, info: SessionInfo) {
  const name = info.title || tr('新会话', 'New Session');
  const ok = await askConfirm({
    title: tr(`删除会话“${name}”？`, `Delete session "${name}"?`),
    message: tr('只删除 Yi Console 里的记录，工具自己的历史不受影响。', 'Only removes the record in Yi Console. Tool history is unaffected.'),
    okText: tr('删除', 'Delete'),
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
    title: tr('清空这个会话的对话？', 'Clear this conversation?'),
    message: isEn()
      ? `Chat history will be permanently deleted, token usage and cost reset to zero, and ${info.tool === 'claude' ? 'Claude Code' : 'the tool'} will start fresh on the next message.${busy ? '\nThe current turn in progress will be interrupted.' : ''}`
      : `聊天记录会被删除且无法恢复，token 和费用清零，下次发消息时 ${info.tool === 'claude' ? 'Claude Code' : '工具'}会从头开始，不记得之前的内容。${busy ? '\nAI 正在回复，这一轮会被中断。' : ''}`,
    okText: tr('清空', 'Clear'),
    danger: true,
  });
  if (!ok) return;
  await call(hostId, info.id, 'session.clear', {}, tr('已清空对话', 'Conversation cleared'));
}

/** Switch the same session between chat mode and terminal mode; the tool continues its original session */
export async function switchMode(hostId: string, info: SessionInfo) {
  const to = info.mode === 'terminal' ? 'structured' : 'terminal';
  const busy = info.status === 'running' || info.status === 'waiting-permission';
  if (to === 'terminal' && busy) {
    const ok = await askConfirm({
      title: tr('切换到终端模式？', 'Switch to Terminal Mode?'),
      message: tr('AI 正在回复，切换会中断这一轮。', 'AI is replying. Switching modes will interrupt the current turn.'),
      okText: tr('切换', 'Switch'),
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
      title: `${tr('终端', 'Terminal')} · ${dir}`,
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
            label: tr(`复制 ${tool} 会话 ID`, `Copy ${tool} Session ID`),
            icon: 'copy',
            hint: info.externalId.slice(0, 8),
            run: copy(info.externalId, tr(`已复制 ${tool} 会话 ID`, `Copied ${tool} Session ID`)),
          },
        ]
      : []),
    ...(cmd ? [{ label: tr('复制恢复命令', 'Copy Resume Command'), icon: 'terminal', run: copy(cmd, tr(`已复制：${cmd}`, `Copied: ${cmd}`)) }] : []),
    {
      label: tr('复制 Yi Console 会话 ID', 'Copy Yi Console Session ID'),
      icon: 'copy',
      hint: info.id.slice(0, 8),
      run: copy(info.id, tr('已复制 Yi Console 会话 ID', 'Copied Yi Console Session ID')),
    },
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
  return [
    ...(opts.inSession ? [] : [{ label: tr('打开', 'Open'), icon: 'chat', run: () => navigate({ kind: 'session', hostId, sessionId: sid }) }]),
    {
      label: tr('重命名', 'Rename'),
      icon: 'edit',
      hint: mobile ? undefined : opts.inSession ? tr('双击标题', 'Double-click title') : 'F2',
      run: () => renameSession(hostId, info),
    },
    {
      label: info.pinned ? tr('取消置顶', 'Unpin') : tr('置顶', 'Pin'),
      icon: 'pin',
      run: () => call(hostId, sid, 'session.pin', { pinned: !info.pinned }),
    },
    ...(info.tool !== 'shell' && !info.parentId
      ? [
          info.mode === 'terminal'
            ? { label: tr('切换到对话模式', 'Switch to Chat Mode'), icon: 'chat', run: () => switchMode(hostId, info) }
            : { label: tr('切换到终端模式', 'Switch to Terminal Mode'), icon: 'terminal', run: () => switchMode(hostId, info) },
        ]
      : []),
    ...(opts.onToggleStyle && info.mode === 'structured' && info.tool !== 'shell'
      ? [
          {
            label: opts.style === 'modern' ? tr('回复风格：切换到时间线', 'Reply Style: Switch to Timeline') : tr('回复风格：切换到简约', 'Reply Style: Switch to Minimal'),
            icon: opts.style === 'modern' ? 'outline' : 'rows',
            run: opts.onToggleStyle,
          },
        ]
      : []),
    ...(info.tool !== 'shell' && info.mode !== 'terminal' ? [{ label: tr('清空对话', 'Clear Conversation'), icon: 'trash', run: () => clearSession(hostId, info) }] : []),
    // No tool session id means nothing has been said yet (or just /clear-ed), so there's nothing to hand off
    ...(info.tool !== 'shell' && info.mode === 'structured' && !info.parentId && info.externalId
      ? [{ label: tr('换个工具继续…', 'Continue in Another Tool…'), icon: 'forward', run: () => handoffSession(hostId, info) }]
      : []),
    '-',
    // The session page has its own "View Changes" button in the top bar
    ...(opts.inSession ? [] : [{ label: tr('查看改动', 'View Changes'), icon: 'diff', run: () => showChanges(hostId, sid) }]),
    // The mobile top bar already has a usage button, so don't repeat it in the menu; the terminal panel is the reverse and only appears in the mobile menu
    ...(opts.onUsage && info.tool !== 'shell' && !mobile ? [{ label: tr('查看用量', 'View Usage'), icon: 'chart', run: opts.onUsage }] : []),
    ...(opts.onToggleTerm && info.tool !== 'shell' && mobile
      ? [{ label: opts.term ? tr('收起终端面板', 'Hide Terminal Panel') : tr('打开终端面板', 'Open Terminal Panel'), icon: 'terminal', run: opts.onToggleTerm }]
      : []),
    { label: tr('复制为 Markdown', 'Copy as Markdown'), icon: 'copy', run: () => exportSession(hostId, info, 'copy') },
    { label: tr('导出 Markdown 文件', 'Export Markdown File'), icon: 'download', run: () => exportSession(hostId, info, 'download') },
    { label: tr('浏览工作目录', 'Browse Working Directory'), icon: 'folder', run: () => navigate({ kind: 'files', hostId, path: info.cwd }) },
    { label: tr('端口转发', 'Port Forwarding'), icon: 'external', run: () => navigate({ kind: 'ports', hostId }) },
    { label: tr('复制工作目录路径', 'Copy Directory Path'), icon: 'copy', run: () => writeClipboard(info.cwd).then(() => toast(tr('已复制路径', 'Path copied'))) },
    ...idItems(info),
    '-',
    ...(alive ? [{ label: tr('停止进程', 'Stop Process'), icon: 'power', run: () => call(hostId, sid, 'session.kill', {}, tr('已停止', 'Stopped')) }] : []),
    ...(opts.inSession
      ? [{ label: tr('重新加载', 'Reload'), icon: 'refresh', run: () => loadSession(hostId, sid, true).catch(errorToast) }]
      : []),
    { label: tr('删除会话', 'Delete Session'), icon: 'trash', danger: true, run: () => deleteSession(hostId, info) },
  ];
}
