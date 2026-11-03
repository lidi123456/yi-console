import { useEffect, useMemo, useRef, useState } from 'react';
import { useBack } from '../back';
import { getT, isEn } from '../i18n';
import { errorToast, hostStatus, loadOlder, loadSession, navigate, setState, skey, toast, useSessionData, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type QuestionAnswer } from '../types';
import { fmtUsage, lsGet, lsSet, shortPath, STATUS_LABEL } from '../util';
import { openMenu } from './Overlay';
import { exportSession, OPEN_CHANGES, renameSession, sessionMenu, showChanges, switchMode } from './SessionActions';
import { FilePanel } from './FilePanel';
import { TermPanel } from './TermPanel';
import { ChatView, PermissionCard, setChatStyle, useChatStyle } from './ChatView';
import { PlanDock } from './PlanDock';
import { LimitDock } from './LimitDock';
import { Composer } from './Composer';
import { Icon, ToolBadge } from './Icon';
import { TermTools, TerminalView } from './TerminalView';
import { StatusLine } from './StatusLine';
import { BgTasks } from './BgTasks';
import { UsageDialog } from './UsageDialog';

const t = getT('sessionView');

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
  // Files session: no process and no chat, the page is the file panel (plus the bottom terminal)
  const filesOnly = info?.tool === 'files';
  // Whether the file panel is open: remembered per session (like the bottom terminal), so opening it in one session doesn't open it in others
  const filesKey = `file-panel-${key}`;
  const [files, setFiles] = useState(() => lsGet(filesKey, false));
  const toggleFiles = () => {
    lsSet(filesKey, !files);
    setFiles(!files);
    setFileOpen(false);
    setFull(false);
  };
  // "View changes": open the file panel (which switches itself to "Changes")
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
  // Bottom terminal panel: toggle remembered per session, so opening it in one session doesn't pop up a terminal in others
  const termKey = `term-panel-${key}`;
  const [, bumpTerm] = useState(0);
  const term = lsGet(termKey, false);
  const toggleTerm = () => {
    lsSet(termKey, !term);
    bumpTerm((n) => n + 1);
  };
  // Mobile back key: collapse the file panel and the bottom terminal first (last opened closes first)
  useBack(files && !filesOnly, toggleFiles);
  useBack(term, toggleTerm);
  // Width of the workspace file panel (drag the left edge) and full screen
  const [panelW, setPanelW] = useState(() => lsGet('file-panel-w', 640));
  const [full, setFull] = useState(false);
  // A file is open in the panel: wider and resizable; with only the tree it's a narrow column
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

  // When the session has exited, start the tool process as soon as the user starts typing (resuming takes 1–4 s), so it's mostly ready by send time
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
    // data.loaded: when a catch-up fetch failed and was marked unloaded, reload the session being viewed too
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
        {connected ? t('sessionNotFoundOrDeleted') : t('hostNotConnected')}
        <button className="btn" onClick={() => navigate({ kind: 'home' })}>
          {t('backToOverview')}
        </button>
      </div>
    );
  }

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

  // Manual recovery after the tool process exited abnormally (killed, crashed): restart it on the original tool session without sending a message
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
        <button className="icon-btn show-mobile session-nav-btn" onClick={() => setState({ sidebarOpen: true })} aria-label={t('menu')}>
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
            {info.title || t('newSession')}
          </div>
          <div className="muted small truncate">
            {TOOL_LABEL[info.tool]}
            {info.mode === 'terminal' && info.model ? ` · ${info.model}` : ''} · {shortPath(info.cwd, home)}
            {info.contextPercent != null && ` · ${t('context')} ${Math.round(info.contextPercent)}%`}
          </div>
        </div>
        {!filesOnly && <span className={`status-chip st-${info.status}`}>{STATUS_LABEL[info.status]}</span>}
        {info.tool !== 'shell' && !filesOnly && (
          <button
            className="btn small usage-btn hide-mobile"
            onClick={() => setUsage(true)}
            title={t('viewSessionUsageAndAccount')}
          >
            <Icon name="chart" size={14} />
            <span className="hide-mobile narrow-hide">{fmtUsage(info.usage) || t('usage')}</span>
          </button>
        )}
        {info.tool !== 'shell' && !filesOnly && (
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
                ? t('switchToChatModeAnd')
                : t('switchToTerminalModeAnd')
            }
          >
            <Icon name={info.mode === 'terminal' ? 'chat' : 'terminal'} size={14} />
            <span className="hide-mobile narrow-hide">
              {switching
                ? t('switching')
                : info.mode === 'terminal'
                  ? t('chatMode')
                  : t('terminalMode')}
            </span>
          </button>
        )}
        {info.mode === 'structured' && info.tool !== 'shell' && !filesOnly && (
          <button
            className="icon-btn hide-mobile"
            title={
              style === 'modern'
                ? t('replyStyleMinimalGroupedThinking')
                : t('replyStyleClaudeCodeTimeline')
            }
            onClick={() => setChatStyle(style === 'modern' ? 'claude' : 'modern')}
          >
            <Icon name={style === 'modern' ? 'rows' : 'outline'} size={16} />
          </button>
        )}
        <button className="icon-btn" title={t('viewChanges')} aria-label={t('viewChanges')} onClick={() => showChanges(hostId, sessionId)}>
          <Icon name="diff" size={17} />
        </button>
        {!filesOnly && (
          <button className={`icon-btn ${files ? 'active' : ''}`} title={t('workspaceFiles')} onClick={toggleFiles}>
            <Icon name="folder" size={17} />
          </button>
        )}
        {/* On mobile this spot holds "Usage" (the top-bar usage button is hidden on mobile), and the bottom terminal panel moves into the "More" menu */}
        {info.tool !== 'shell' && !filesOnly && (
          <button
            className="icon-btn show-mobile"
            title={t('viewSessionUsageAndAccount')}
            aria-label={t('usage')}
            onClick={() => setUsage(true)}
          >
            <Icon name="chart" size={17} />
          </button>
        )}
        {info.tool !== 'shell' && (
          // Files sessions have no usage button, so the terminal button stays in the top bar on mobile too
          <button className={`icon-btn ${filesOnly ? '' : 'hide-mobile'} ${term ? 'active' : ''}`} title={term ? t('hideTerminal') : t('openTerminal')} onClick={toggleTerm}>
            <Icon name="terminal" size={17} />
          </button>
        )}
        <button
          className="icon-btn"
          title={t('more')}
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

      {filesOnly ? (
        connected ? (
          <FilePanel
            key={`${hostId}/${sessionId}`}
            hostId={hostId}
            cwd={info.cwd}
            sessionId={sessionId}
            status={info.status}
            fullscreen={false}
            onFullscreen={() => {}}
            onClose={() => {}}
            standalone
          />
        ) : (
          <div className="center-msg">{t('hostNotConnected')}</div>
        )
      ) : info.mode === 'terminal' ? (
        <>
          <TerminalView hostId={hostId} sessionId={sessionId} running={alive} />
          <div className="term-status">
            <StatusLine hostId={hostId} info={info} events={data.events} />
            <TermTools />
          </div>
          {!alive && (
            <div className="term-restart">
              {t('processExited')}
              <button className="btn primary small" onClick={() => act('session.resume', { cols: 120, rows: 32 })}>
                {t('restart')}
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
            <LimitDock hostId={hostId} info={info} connected={connected} />
            {info.status === 'error' && (
              <div className="session-recover" role="status">
                <span className="session-recover-text">
                  {t('theToolProcessExitedUnexpectedly')}
                </span>
                <button className="btn primary small" disabled={!connected || recovering} onClick={recover}>
                  {recovering && <span className="spinner" />}
                  {recovering ? t('resuming') : t('resumeSession')}
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
      {!filesOnly && files && connected && full && <div className="panel-scrim hide-mobile" onClick={() => setFull(false)} title={t('exitFullscreen')} />}
      {!filesOnly && files && connected && !full && fileOpen && <div className="panel-resizer hide-mobile" onPointerDown={dragPanel} title={t('dragToResize')} />}
      {!filesOnly && files && connected && (
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
