import { useEffect, useMemo, useRef, useState } from 'react';
import { useBack } from '../back';
import { isEn, tr } from '../i18n';
import { errorToast, hostStatus, loadOlder, loadSession, navigate, setState, skey, toast, useSessionData, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type QuestionAnswer } from '../types';
import { fmtUsage, lsGet, lsSet, shortPath, STATUS_LABEL } from '../util';
import { openMenu } from './Overlay';
import { exportSession, OPEN_CHANGES, renameSession, sessionMenu, switchMode } from './SessionActions';
import { FilePanel } from './FilePanel';
import { TermPanel } from './TermPanel';
import { ChatView, PermissionCard, setChatStyle, useChatStyle } from './ChatView';
import { PlanDock } from './PlanDock';
import { Composer } from './Composer';
import { Icon, ToolBadge } from './Icon';
import { TermTools, TerminalView } from './TerminalView';
import { StatusLine } from './StatusLine';
import { BgTasks } from './BgTasks';
import { UsageDialog } from './UsageDialog';

export function SessionView({ hostId, sessionId, seq }: { hostId: string; sessionId: string; seq?: number }) {
  const key = skey(hostId, sessionId);
  const data = useSessionData(key);
  const info = useStore((s) => s.sessions[hostId]?.find((x) => x.id === sessionId));
  const connected = useStore((s) => s.hosts.find((h) => h.config.id === hostId)?.status.status === 'connected');
  const attn = useStore((s) =>
    Object.values(s.sessions).reduce((n, l) => n + l.filter((x) => x.pendingPermissions > 0).length, 0),
  );
  const home = hostStatus(hostId).hello?.home;
  const style = useChatStyle();
  // 文件侧栏开没开：每个会话各记各的（同底部终端），在一个会话里打开不会让别的会话也打开
  const filesKey = `file-panel-${key}`;
  const [files, setFiles] = useState(() => lsGet(filesKey, false));
  const toggleFiles = () => {
    lsSet(filesKey, !files);
    setFiles(!files);
    setFileOpen(false);
    setFull(false);
  };
  // 「查看改动」：打开文件面板（面板自己切到「改动」）
  useEffect(() => {
    const on = (e: Event) => {
      const d = (e as CustomEvent<{ hostId: string; sessionId: string }>).detail;
      if (d.hostId !== hostId || d.sessionId !== sessionId) return;
      lsSet(filesKey, true);
      setFiles(true);
      setFull(false);
    };
    window.addEventListener(OPEN_CHANGES, on);
    return () => window.removeEventListener(OPEN_CHANGES, on);
  }, [hostId, sessionId, filesKey]);
  // 底部终端面板：每个会话各记各的开关，在一个会话里打开不会让别的会话也弹出终端
  const termKey = `term-panel-${key}`;
  const [, bumpTerm] = useState(0);
  const term = lsGet(termKey, false);
  const toggleTerm = () => {
    lsSet(termKey, !term);
    bumpTerm((n) => n + 1);
  };
  // 手机返回键：先收起文件面板、底部终端（后打开的先关）
  useBack(files, toggleFiles);
  useBack(term, toggleTerm);
  // 工作区文件面板的宽度（拖左边缘调整）和全屏
  const [panelW, setPanelW] = useState(() => lsGet('file-panel-w', 640));
  const [full, setFull] = useState(false);
  // 面板里打开了文件：变宽、可以拖宽度；只有文件树时是窄的一栏
  const [fileOpen, setFileOpen] = useState(false);
  const [switching, setSwitching] = useState(false);
  const [usage, setUsage] = useState(false);
  const [recovering, setRecovering] = useState(false);
  const split = useRef<HTMLDivElement>(null);
  const dragPanel = (e: React.PointerEvent) => {
    e.preventDefault();
    const x0 = e.clientX;
    const w0 = panelW;
    const max = (split.current?.clientWidth ?? 1200) - 420;
    let w = w0;
    const move = (ev: PointerEvent) => {
      w = Math.max(420, Math.min(max, w0 - (ev.clientX - x0)));
      setPanelW(w);
    };
    const up = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
      document.body.classList.remove('resizing');
      lsSet('file-panel-w', w);
    };
    document.body.classList.add('resizing');
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  };

  // 会话已退出时，一开始打字就先把工具进程拉起来（恢复要 1～4 秒），等发送时基本已经就绪
  const warmed = useRef('');
  useEffect(() => {
    if (info?.status !== 'exited' && info?.status !== 'error') warmed.current = '';
  }, [info?.status]);
  const prewarm = () => {
    if (!info || !connected || info.mode === 'terminal' || warmed.current === key) return;
    if (info.status !== 'exited' && info.status !== 'error') return;
    warmed.current = key;
    rpc(hostId, 'session.resume', { sessionId }).catch(() => {
      warmed.current = '';
    });
  };

  useEffect(() => {
    if (connected) loadSession(hostId, sessionId).catch(errorToast);
    // data.loaded：补拉失败被标成未加载时，正在看的会话也重新加载
  }, [hostId, sessionId, connected, data.loaded]);

  const pending = useMemo(() => {
    const open = new Map<string, any>();
    for (const e of data.events) {
      if (e.kind === 'permission_request') open.set(e.requestId, e);
      if (e.kind === 'permission_resolved') open.delete(e.requestId);
    }
    return [...open.values()];
  }, [data.events, data.events.length]);

  if (!info) {
    return (
      <div className="center-msg">
        {connected ? tr('会话不存在或已删除', 'Session not found or deleted') : tr('主机未连接', 'Host not connected')}
        <button className="btn" onClick={() => navigate({ kind: 'home' })}>
          {tr('返回总览', 'Back to Overview')}
        </button>
      </div>
    );
  }

  const running = info.status === 'running' || info.status === 'starting' || info.status === 'waiting-permission';
  const alive = info.status !== 'exited' && info.status !== 'error';

  const answer = async (requestId: string, optionId: string, answers?: QuestionAnswer[]) => {
    try {
      await rpc(hostId, 'session.permission', { sessionId, requestId, optionId, answers });
    } catch (e) {
      errorToast(e);
    }
  };

  const sendText = async (text: string) => {
    try {
      await rpc(hostId, 'session.send', { sessionId, text, images: [], queue: true });
    } catch (e) {
      errorToast(e);
    }
  };

  const act = async (method: string, params: Record<string, unknown> = {}, ok?: string) => {
    try {
      await rpc(hostId, method, { sessionId, ...params });
      if (ok) toast(ok);
    } catch (e) {
      errorToast(e);
    }
  };

  const rename = () => renameSession(hostId, info);

  // 工具进程异常退出（被杀、崩溃）后手动恢复：接着原来的工具会话重新拉起，不发消息
  const recover = async () => {
    setRecovering(true);
    try {
      await rpc(hostId, 'session.resume', { sessionId });
    } catch (e) {
      errorToast(e);
    } finally {
      setRecovering(false);
    }
  };

  return (
    <div className="session-split" ref={split} style={{ '--panel-w': `${panelW}px` } as React.CSSProperties}>
    <div className="session-view">
      <header className="session-head">
        <button className="icon-btn show-mobile session-nav-btn" onClick={() => setState({ sidebarOpen: true })} aria-label={tr('菜单', 'Menu')}>
          <Icon name="menu" />
          {attn > 0 && <span className="badge">{attn}</span>}
        </button>
        <ToolBadge tool={info.tool} size={20} />
        <div className="session-meta">
          <div
            className="session-name truncate"
            onDoubleClick={rename}
            title={
              isEn()
                ? `${info.externalId ? `${TOOL_LABEL[info.tool]} Session ID: ${info.externalId}\n` : ''}Yi Console Session ID: ${info.id}\nDouble-click to rename, copy ID from More menu`
                : `${info.externalId ? `${TOOL_LABEL[info.tool]} 会话 ID：${info.externalId}\n` : ''}Yi Console 会话 ID：${info.id}\n双击重命名，「更多」菜单里可复制 ID`
            }
          >
            {info.title || tr('新会话', 'New Session')}
          </div>
          <div className="muted small truncate">
            {TOOL_LABEL[info.tool]}
            {info.mode === 'terminal' && info.model ? ` · ${info.model}` : ''} · {shortPath(info.cwd, home)}
            {info.contextPercent != null && ` · ${tr('上下文', 'Context')} ${Math.round(info.contextPercent)}%`}
          </div>
        </div>
        <span className={`status-chip st-${info.status}`}>{STATUS_LABEL[info.status]}</span>
        {info.tool !== 'shell' && (
          <button
            className="btn small usage-btn hide-mobile"
            onClick={() => setUsage(true)}
            title={tr('查看这个会话的用量和账号额度', 'View session usage and account quota')}
          >
            <Icon name="chart" size={14} />
            <span className="hide-mobile narrow-hide">{fmtUsage(info.usage) || tr('用量', 'Usage')}</span>
          </button>
        )}
        {running && info.mode === 'structured' && (
          <button className="btn small hide-mobile" onClick={() => act('session.interrupt')} title={tr('中断当前回合', 'Interrupt current turn')}>
            <Icon name="stop" size={14} /> {tr('中断', 'Stop')}
          </button>
        )}
        {info.tool !== 'shell' && (
          <button
            className="btn small hide-mobile"
            disabled={!connected || switching}
            onClick={async () => {
              setSwitching(true);
              await switchMode(hostId, info);
              setSwitching(false);
            }}
            title={
              info.mode === 'terminal'
                ? tr('切换到对话模式，接着同一个会话', 'Switch to Chat mode and continue the same session')
                : tr('切换到终端模式，用工具自己的界面接着这个会话', 'Switch to Terminal mode and continue in the tool CLI')
            }
          >
            <Icon name={info.mode === 'terminal' ? 'chat' : 'terminal'} size={14} />
            <span className="hide-mobile narrow-hide">
              {switching
                ? tr('切换中…', 'Switching…')
                : info.mode === 'terminal'
                  ? tr('对话模式', 'Chat Mode')
                  : tr('终端模式', 'Terminal Mode')}
            </span>
          </button>
        )}
        {info.mode === 'structured' && info.tool !== 'shell' && (
          <button
            className="icon-btn hide-mobile"
            title={
              style === 'modern'
                ? tr(
                    '回复显示风格：简约（思考和工具调用收成一组）。点击切换到 Claude Code 时间线',
                    'Reply style: Minimal (grouped thinking & tool calls). Click to switch to Claude Code timeline',
                  )
                : tr('回复显示风格：Claude Code 时间线。点击切换到简约', 'Reply style: Claude Code timeline. Click to switch to Minimal')
            }
            onClick={() => setChatStyle(style === 'modern' ? 'claude' : 'modern')}
          >
            <Icon name={style === 'modern' ? 'rows' : 'outline'} size={16} />
          </button>
        )}
        <button className={`icon-btn ${files ? 'active' : ''}`} title={tr('工作目录文件', 'Workspace Files')} onClick={toggleFiles}>
          <Icon name="folder" size={17} />
        </button>
        {/* 手机上这个位置放「用量」（顶栏的用量按钮在手机上隐藏），底部终端面板挪进「更多」菜单 */}
        {info.tool !== 'shell' && (
          <button
            className="icon-btn show-mobile"
            title={tr('查看这个会话的用量和账号额度', 'View session usage and account quota')}
            aria-label={tr('用量', 'Usage')}
            onClick={() => setUsage(true)}
          >
            <Icon name="chart" size={17} />
          </button>
        )}
        {info.tool !== 'shell' && (
          <button className={`icon-btn hide-mobile ${term ? 'active' : ''}`} title={term ? tr('收起终端', 'Hide Terminal') : tr('打开终端', 'Open Terminal')} onClick={toggleTerm}>
            <Icon name="terminal" size={17} />
          </button>
        )}
        <button
          className="icon-btn"
          title={tr('更多', 'More')}
          onClick={(e) => {
            const r = e.currentTarget.getBoundingClientRect();
            openMenu(
              { clientX: r.right - 200, clientY: r.bottom + 4 },
              sessionMenu(hostId, info, {
                inSession: true,
                onUsage: () => setUsage(true),
                term,
                onToggleTerm: toggleTerm,
                style,
                onToggleStyle: () => setChatStyle(style === 'modern' ? 'claude' : 'modern'),
              }),
            );
          }}
        >
          <Icon name="more" size={18} />
        </button>
      </header>

      {info.mode === 'terminal' ? (
        <>
          <TerminalView hostId={hostId} sessionId={sessionId} running={alive} />
          <div className="term-status">
            <StatusLine hostId={hostId} info={info} events={data.events} />
            <TermTools />
          </div>
          {!alive && (
            <div className="term-restart">
              {tr('进程已结束', 'Process exited')}
              <button className="btn primary small" onClick={() => act('session.resume', { cols: 120, rows: 32 })}>
                {tr('重新启动', 'Restart')}
              </button>
            </div>
          )}
        </>
      ) : (
        <>
          <ChatView
            events={data.events}
            live={data.live}
            liveThinking={data.liveThinking}
            info={info}
            hostId={hostId}
            onAnswer={answer}
            hasMore={data.hasMore}
            loadingOlder={data.loadingOlder}
            loading={data.loading}
            onLoadOlder={() => loadOlder(hostId, sessionId).catch(errorToast)}
            focusSeq={seq}
            pendingOutside
            onSendText={sendText}
          />
          <div className="composer-wrap">
            <BgTasks hostId={hostId} info={info} />
            <PlanDock events={data.events} busy={info.status === 'running' || info.status === 'starting'} />
            {info.status === 'error' && (
              <div className="session-recover" role="status">
                <span className="session-recover-text">
                  {tr('工具进程异常退出，聊天记录还在', 'The tool process exited unexpectedly. Your history is kept.')}
                </span>
                <button className="btn primary small" disabled={!connected || recovering} onClick={recover}>
                  {recovering && <span className="spinner" />}
                  {recovering ? tr('正在恢复…', 'Resuming…') : tr('恢复会话', 'Resume session')}
                </button>
              </div>
            )}
            {pending.length > 0 && (
              <div className="pending-perms">
                {pending.map((p) => (
                  <PermissionCard key={p.requestId} it={p} onAnswer={answer} compact />
                ))}
              </div>
            )}
            <Composer
              hostId={hostId}
              info={info}
              connected={connected}
              onPrewarm={prewarm}
              onInterrupt={() => act('session.interrupt')}
              onExport={() => exportSession(hostId, info, 'download')}
              onRename={rename}
            />
            <StatusLine hostId={hostId} info={info} events={data.events} />
          </div>
        </>
      )}
      {usage && <UsageDialog hostId={hostId} info={info} onClose={() => setUsage(false)} />}
      {term && connected && info.tool !== 'shell' && <TermPanel key={key} hostId={hostId} info={info} onClose={toggleTerm} />}
    </div>
      {files && connected && full && <div className="panel-scrim hide-mobile" onClick={() => setFull(false)} title={tr('退出全屏', 'Exit Fullscreen')} />}
      {files && connected && !full && fileOpen && <div className="panel-resizer hide-mobile" onPointerDown={dragPanel} title={tr('拖动调整宽度', 'Drag to resize')} />}
      {files && connected && (
        <FilePanel
          key={`${hostId}/${info.cwd}`}
          hostId={hostId}
          cwd={info.cwd}
          sessionId={sessionId}
          status={info.status}
          fullscreen={full}
          onFullscreen={() => setFull(!full)}
          onClose={toggleFiles}
          onFileOpen={setFileOpen}
        />
      )}
    </div>
  );
}
