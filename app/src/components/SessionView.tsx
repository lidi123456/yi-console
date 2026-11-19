import { ComputerUsePanel } from './ComputerUsePanel';
import { BrowserUsePanel } from './BrowserUsePanel';
import { canUseComputer } from '../computerUse';
import { canUseBrowser } from '../browserUse';
import { useEffect, useMemo, useRef, useState } from 'react';
import { useBack } from '../back';
import { getT, isEn } from '../i18n';
import { errorToast, hostStatus, loadOlder, loadSession, navigate, setState, skey, toast, useSessionData, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, supportsAccountsTool, type QuestionAnswer, type ToolAccountsSummary } from '../types';
import { fmtUsage, isMobile, lsGet, lsSet, shortPath, STATUS_LABEL } from '../util';
import { openAccountManager } from './AccountManagerModal';
import { openMenu } from './Overlay';
import { exportSession, OPEN_CHANGES, OPEN_FILE, openFileInPanel, renameSession, sessionMenu, showChanges, switchMode } from './SessionActions';
import { AI_PREFIX } from './AiBrowser';
import { FilePanel } from './FilePanel';
import { TermPanel } from './TermPanel';
import { AsyncAskCard, ChatView, openAsyncAsk, PermissionCard, setChatStyle, useChatStyle } from './ChatView';
import { PlanDock } from './PlanDock';
import { LimitDock } from './LimitDock';
import { Composer } from './Composer';
import { Icon, ToolBadge } from './Icon';
import { TermTools, TerminalView } from './TerminalView';
import { StatusLine } from './StatusLine';
import { BgTasks } from './BgTasks';
import { UsageDialog } from './UsageDialog';
import { SessionMemoryDrawer } from './MemoryView';
import { SessionBotBar } from './BotsSection';

const t = getT('sessionView');
/** Sessions whose AI browser view was opened automatically in this window */
const autoOpened = new Set<string>();

export function SessionView({ hostId, sessionId, seq }: { hostId: string; sessionId: string; seq?: number }) {
  const key = skey(hostId, sessionId);
  const data = useSessionData(key);
  const info = useStore((s) => s.sessions[hostId]?.find((x) => x.id === sessionId));
  const connected = useStore((s) => s.hosts.find((h) => h.config.id === hostId)?.status.status === 'connected');
  const attn = useStore((s) =>
    Object.values(s.sessions).reduce((n, l) => n + l.filter((x) => x.pendingPermissions > 0).length, 0),
  );
  const home = hostStatus(hostId).hello?.home;
  const desktopRevision = useMemo(() => data.events.reduce((seq, event) => event.kind === 'computer_use' && event.status === 'control' ? event.seq : seq, 0), [data.events, data.events.length]);
  const browserRevision = useMemo(() => data.events.reduce((seq, event) => event.kind === 'browser_use' ? event.seq : seq, 0), [data.events, data.events.length]);
  // The AI starts using the browser: show its live view in the file panel, once per session (closing the tab sticks)
  const lastAiAction = useMemo(() => {
    for (let i = data.events.length - 1; i >= 0; i--) {
      const e = data.events[i];
      if (e.kind === 'browser_use' && e.status === 'running') return e.seq;
    }
    return 0;
  }, [data.events, data.events.length]);
  const seenAiAction = useRef<number | null>(null);
  useEffect(() => {
    if (!data.loaded) return;
    if (seenAiAction.current === null) {
      seenAiAction.current = lastAiAction;
      return;
    }
    if (lastAiAction <= seenAiAction.current) return;
    seenAiAction.current = lastAiAction;
    if (autoOpened.has(key) || isMobile()) return;
    autoOpened.add(key);
    // Wide enough to read the page (not remembered as the panel width)
    setPanelW((w) => Math.max(w, Math.min(960, Math.round(window.innerWidth * 0.55))));
    openFileInPanel(hostId, sessionId, AI_PREFIX + sessionId);
  }, [lastAiAction, data.loaded]); // eslint-disable-line react-hooks/exhaustive-deps
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
  // "View changes" / a file link in a reply: open the file panel (which switches itself to "Changes" or opens the file)
  useEffect(() => {
    const on = (e: Event) => {
      const d = (e as CustomEvent<{ hostId: string; sessionId: string }>).detail;
      if (d.hostId !== hostId || d.sessionId !== sessionId) return;
      lsSet(filesKey, true);
      setFiles(true);
      setFull(false);
    };
    window.addEventListener(OPEN_CHANGES, on);
    window.addEventListener(OPEN_FILE, on);
    return () => {
      window.removeEventListener(OPEN_CHANGES, on);
      window.removeEventListener(OPEN_FILE, on);
    };
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
  const [memoryOpen, setMemoryOpen] = useState(false);
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
  // Async questions (Codex request_user_input_async) don't block the tool; the one still unanswered is pinned with the approvals
  const ask = useMemo(() => openAsyncAsk(data.events), [data.events, data.events.length]);
  const skipKey = `asyncAskSkipped:${hostId}:${sessionId}`;
  const [skippedAsks, setSkippedAsks] = useState<string[]>(() => lsGet<string[]>(skipKey, []));
  useEffect(() => setSkippedAsks(lsGet<string[]>(skipKey, [])), [skipKey]);
  const skipAsk = (id: string) => {
    const next = [...skippedAsks.filter((x) => x !== id), id].slice(-50);
    setSkippedAsks(next);
    lsSet(skipKey, next);
  };
  const openAsk = ask && !skippedAsks.includes(ask.id) ? ask : null;

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
            {supportsAccountsTool(info.tool) && (
              <>
                {' · '}
                <button
                  type="button"
                  className="link-btn small acc-session-btn"
                  title={t('switchAccountTitle')}
                  onClick={async (e) => {
                    const coords = { clientX: e.clientX, clientY: e.clientY };
                    try {
                      const res = await rpc<{ tools: ToolAccountsSummary[] }>(hostId, 'tools.accounts.list', {
                        tool: info.tool,
                        checkNative: false,
                      });
                      const accounts = res.tools?.[0]?.accounts ?? [];
                      openMenu(coords, [
                        ...accounts.map((acc) => ({
                          label: acc.label,
                          hint: [acc.authKind === 'apiKey' ? 'API' : 'OAuth', acc.identity.email].filter(Boolean).join(' · '),
                          active: acc.id === info.accountId,
                          run: async () => {
                            try {
                              await rpc(hostId, 'session.switchAccount', { sessionId: info.id, accountId: acc.id });
                              toast(t('switchedAccount', { label: acc.label }), 'info');
                            } catch (err) {
                              errorToast(err);
                            }
                          },
                        })),
                        ...(accounts.length ? (['-'] as const) : []),
                        {
                          label: t('manageAccounts'),
                          icon: 'key',
                          run: () => openAccountManager({ hostId, tool: info.tool, sessionId: info.id, accountId: info.accountId }),
                        },
                      ]);
                    } catch (err) {
                      errorToast(err);
                    }
                  }}
                >
                  {info.accountLabel ?? t('defaultAccount')}
                </button>
              </>
            )}
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
        {info.mode === 'structured' && info.tool !== 'shell' && !filesOnly && (
          <button
            className="icon-btn hide-mobile"
            title={t('memory')}
            aria-label={t('memory')}
            onClick={() => setMemoryOpen(true)}
          >
            <Icon name="book" size={16} />
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
                onMemory: () => setMemoryOpen(true),
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
      {info.bot && <SessionBotBar info={info} />}

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
            skippedAsks={skippedAsks}
          />
          <div className="composer-wrap">
            <BgTasks hostId={hostId} info={info} />
            <PlanDock events={data.events} busy={info.status === 'running' || info.status === 'starting'} alive={connected && alive} />
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
            {(pending.length > 0 || openAsk) && (
              <div className="pending-perms">
                {pending.map((p) => (
                  <PermissionCard key={p.requestId} it={p} onAnswer={answer} compact />
                ))}
                {openAsk && (
                  <AsyncAskCard key={openAsk.id} ask={openAsk} tool={info.tool} compact onSend={sendText} onSkip={() => skipAsk(openAsk.id)} />
                )}
              </div>
            )}
            <Composer
              hostId={hostId}
              info={info}
              connected={connected}
              desktopControls={
                canUseComputer(hostStatus(hostId).hello, info) || canUseBrowser(hostStatus(hostId).hello, info) ? (
                  <>
                    {canUseComputer(hostStatus(hostId).hello, info) && <ComputerUsePanel hostId={hostId} info={info} connected={connected} revision={desktopRevision} />}
                    {canUseBrowser(hostStatus(hostId).hello, info) && <BrowserUsePanel hostId={hostId} info={info} connected={connected} revision={browserRevision} />}
                  </>
                ) : undefined
              }
              onPrewarm={prewarm}
              onInterrupt={() => act('session.interrupt')}
              onExport={() => exportSession(hostId, info, 'download')}
              onRename={rename}
            />
            <StatusLine hostId={hostId} info={info} events={data.events} />
          </div>
        </>
      )}
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
      {usage && <UsageDialog hostId={hostId} info={info} onClose={() => setUsage(false)} />}
      {memoryOpen && connected && <SessionMemoryDrawer hostId={hostId} cwd={info.cwd} onClose={() => setMemoryOpen(false)} />}
    </div>
  );
}
