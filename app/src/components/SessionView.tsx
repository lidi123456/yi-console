import { ComputerUsePanel } from './ComputerUsePanel';
import { BrowserUsePanel } from './BrowserUsePanel';
import { canUseComputer } from '../computerUse';
import { canUseBrowser } from '../browserUse';
import { useEffect, useMemo, useRef, useState } from 'react';
import { useBack } from '../back';
import { getT, isEn, useLang } from '../i18n';
import { errorToast, getState, hostStatus, loadOlder, loadSession, loadUserMessages, navigate, replaceView, setState, skey, toast, useSessionData, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, supportsAccountsTool, type QuestionAnswer, type ToolAccountsSummary } from '../types';
import { fmtUsage, isMobile, lsGet, lsSet, shortPath, STATUS_LABEL } from '../util';
import { openAccountManager } from './AccountManagerModal';
import { openMenu } from './Overlay';
import { exportSession, OPEN_CHANGES, OPEN_FILE, openFileInPanel, renameSession, sessionMenu, showTurnDiff } from './SessionActions';
import { AI_PREFIX } from './AiBrowser';
import { FilePanel } from './FilePanel';
import { sideChatTitle, sideChatTools, sideTabPath, startSideChat } from './SideChatTab';
import { TermPanel } from './TermPanel';
import { AsyncAskCard, ChatView, openAsyncAsk, PermissionCard, setChatStyle, useChatStyle } from './ChatView';
import { PlanDock } from './PlanDock';
import { LimitDock } from './LimitDock';
import { Composer, restoreToComposer } from './Composer';
import { Icon, ToolBadge } from './Icon';
import { PromptOutlinePanel, PromptOutlineSheet, PromptStickyBar, PromptTickRail, buildPromptEntries } from './PromptNav';
import { TermTools, TerminalView } from './TerminalView';
import { StatusLine } from './StatusLine';
import { BgTasks } from './BgTasks';
import { UsageDialog } from './UsageDialog';
import { SessionMemoryDrawer } from './MemoryView';
import { SessionBotBar } from './BotsSection';

const t = getT('sessionView');
const tp = getT('promptNav');
/** Sessions whose AI browser view was opened automatically in this window */
const autoOpened = new Set<string>();

function useNarrowViewport() {
  const [narrow, setNarrow] = useState(() =>
    typeof window !== 'undefined' && typeof window.matchMedia === 'function'
      ? window.matchMedia('(max-width: 760px)').matches
      : isMobile(),
  );
  useEffect(() => {
    if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return;
    const mq = window.matchMedia('(max-width: 760px)');
    const sync = () => setNarrow(mq.matches);
    sync();
    mq.addEventListener('change', sync);
    return () => mq.removeEventListener('change', sync);
  }, []);
  return narrow;
}

export function SessionView({ hostId, sessionId, seq }: { hostId: string; sessionId: string; seq?: number }) {
  const key = skey(hostId, sessionId);
  const data = useSessionData(key);
  const hostSessions = useStore((s) => s.sessions[hostId]);
  const info = useMemo(() => hostSessions?.find((x) => x.id === sessionId), [hostSessions, sessionId]);
  const parentExists = !!info?.parentId && !!hostSessions?.some((x) => x.id === info.parentId);
  useEffect(() => {
    if (!info?.parentId || !parentExists) return;
    const pid = info.parentId;
    if (info.sideChat) {
      openFileInPanel(hostId, pid, sideTabPath(info.id));
    } else if (info.mode === 'structured' && info.tool !== 'shell' && info.tool !== 'files') {
      openFileInPanel(hostId, pid, `review:${info.id}`);
    } else if (info.tool === 'shell') {
      lsSet(`term-panel-${skey(hostId, pid)}`, true);
      lsSet(`term-tab-${pid}`, info.id);
    }
    replaceView({ kind: 'session', hostId, sessionId: pid });
  }, [hostId, info?.id, info?.parentId, info?.sideChat, info?.mode, info?.tool, parentExists]);
  const sideChats = useMemo(
    () => (hostSessions ? hostSessions.filter((x) => x.parentId === sessionId && x.sideChat) : []),
    [hostSessions, sessionId],
  );
  const liveSideCount = sideChats.filter((x) => x.status === 'running' || x.status === 'starting').length;
  const attnSideCount = sideChats.filter((x) => x.pendingPermissions > 0 || x.status === 'waiting-permission').length;
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
  // Active directory in the file panel (defaults to info.cwd; can switch to any detected git worktree of the same repo)
  const panelCwdKey = `file-panel-cwd-${key}`;
  const [panelCwd, setPanelCwd] = useState(() => lsGet(panelCwdKey, ''));
  useEffect(() => {
    setPanelCwd(lsGet(panelCwdKey, ''));
  }, [panelCwdKey]);
  const switchPanelCwd = (next: string) => {
    const val = !next || next === info?.cwd ? '' : next;
    lsSet(panelCwdKey, val);
    setPanelCwd(val);
  };
  const activeFileCwd = panelCwd || info?.cwd || '';
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
  const [startingSide, setStartingSide] = useState(false);
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

  const [, lang] = useLang();
  const mobileViewport = useNarrowViewport();
  const isChatMode = !!info && info.tool !== 'files' && info.tool !== 'shell' && info.mode === 'structured';
  const pinKey = `prompt-outline-pin-${key}`;
  const [outlinePinned, setOutlinePinned] = useState(() => lsGet(pinKey, false));
  useEffect(() => {
    setOutlinePinned(lsGet(pinKey, false));
  }, [pinKey]);
  const [outlineMode, setOutlineMode] = useState<'none' | 'hover' | 'click'>('none');
  const [promptScroll, setPromptScroll] = useState<{ activeSeq: number | null; stickyVisible: boolean }>({
    activeSeq: null,
    stickyVisible: false,
  });
  const [jumpReq, setJumpReq] = useState<{ seq: number; nonce: number } | null>(null);
  const [jumpingSeq, setJumpingSeq] = useState<number | null>(null);
  const jumpNonce = useRef(0);
  const stepAnchorRef = useRef<{ seq: number; until: number } | null>(null);
  const popoverRef = useRef<HTMLDivElement>(null);
  const hoverTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const searchFocused = useRef(false);

  const clearHoverTimer = () => {
    if (hoverTimer.current) {
      clearTimeout(hoverTimer.current);
      hoverTimer.current = null;
    }
  };

  useEffect(() => {
    clearHoverTimer();
    searchFocused.current = false;
    stepAnchorRef.current = null;
    setOutlineMode('none');
    setJumpReq(null);
    setJumpingSeq(null);
    setPromptScroll({ activeSeq: null, stickyVisible: false });
  }, [key]);

  useEffect(() => () => clearHoverTimer(), []);

  const docked = isChatMode && outlinePinned && !mobileViewport && !files;

  const loadedUserSeqs = useMemo(() => {
    const s = new Set<number>();
    for (const e of data.events) {
      if (e.kind === 'user_message' && e.seq > 0) s.add(e.seq);
    }
    return s;
  }, [data.events, data.events.length]);

  const hideCompactCmd = info?.tool === 'claude' || info?.tool === 'codex' || info?.tool === 'cursor' || info?.tool === 'antigravity';
  const prompts = useMemo(
    () =>
      buildPromptEntries(data.userMessages, loadedUserSeqs, hideCompactCmd, {
        images: (n) => tp('imagesSummary', { count: n }),
        refs: (n) => tp('refsSummary', { count: n }),
        empty: tp('emptyPromptFallback'),
      }),
    [data.userMessages, loadedUserSeqs, hideCompactCmd, lang],
  );
  const promptSeqs = useMemo(() => prompts.map((p) => p.seq), [prompts]);

  useEffect(() => {
    if (
      connected &&
      isChatMode &&
      data.loaded &&
      data.hasMore &&
      !data.userMessagesLoaded &&
      !data.userMessagesLoading &&
      !data.userMessagesUnsupported
    ) {
      loadUserMessages(hostId, sessionId).catch(() => {});
    }
  }, [
    connected,
    isChatMode,
    hostId,
    sessionId,
    data.loaded,
    data.hasMore,
    data.userMessagesLoaded,
    data.userMessagesLoading,
    data.userMessagesUnsupported,
  ]);

  const jumpToPrompt = (targetSeq: number) => {
    stepAnchorRef.current = { seq: targetSeq, until: Date.now() + 900 };
    setJumpingSeq(targetSeq);
    setJumpReq({ seq: targetSeq, nonce: ++jumpNonce.current });
  };

  const stepPrompt = (delta: -1 | 1) => {
    if (!prompts.length) return;
    const anchorSeq =
      stepAnchorRef.current && Date.now() < stepAnchorRef.current.until
        ? stepAnchorRef.current.seq
        : promptScroll.activeSeq;
    let curIdx = anchorSeq != null ? prompts.findIndex((p) => p.seq === anchorSeq) : prompts.length - 1;
    if (curIdx < 0) curIdx = prompts.length - 1;
    const nextIdx = Math.max(0, Math.min(prompts.length - 1, curIdx + delta));
    const target = prompts[nextIdx];
    if (target && (nextIdx !== curIdx || target.seq !== anchorSeq)) {
      jumpToPrompt(target.seq);
    }
  };

  const stepPromptRef = useRef(stepPrompt);
  stepPromptRef.current = stepPrompt;
  useEffect(() => {
    if (!isChatMode || prompts.length === 0) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.isComposing || e.keyCode === 229) return;
      if (getState().palette) return;
      const mod = /Mac|iPhone|iPad/.test(navigator.platform) ? e.metaKey : e.ctrlKey;
      if (!e.altKey || !e.shiftKey || mod) return;
      if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
        e.preventDefault();
        stepPromptRef.current(e.key === 'ArrowUp' ? -1 : 1);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [isChatMode, prompts.length]);

  const scheduleHoverClose = () => {
    clearHoverTimer();
    hoverTimer.current = setTimeout(() => {
      if (!searchFocused.current) setOutlineMode((m) => (m === 'hover' ? 'none' : m));
    }, 220);
  };

  const openHoverOutline = () => {
    clearHoverTimer();
    hoverTimer.current = setTimeout(() => {
      hoverTimer.current = null;
      setOutlineMode((m) => (m === 'none' ? 'hover' : m));
    }, 180);
  };

  const closeOutline = () => {
    clearHoverTimer();
    searchFocused.current = false;
    if (docked) {
      setOutlinePinned(false);
      lsSet(pinKey, false);
    }
    setOutlineMode('none');
  };

  const togglePinOutline = () => {
    const next = !outlinePinned;
    setOutlinePinned(next);
    lsSet(pinKey, next);
    clearHoverTimer();
    if (next && files && !mobileViewport) {
      lsSet(filesKey, false);
      setFiles(false);
      setFileOpen(false);
      setFull(false);
    }
    setOutlineMode(next ? 'none' : 'click');
  };

  const toggleOutlineBtn = () => {
    clearHoverTimer();
    if (docked) {
      setOutlinePinned(false);
      lsSet(pinKey, false);
      setOutlineMode('none');
      return;
    }
    setOutlineMode((m) => (m === 'click' ? 'none' : 'click'));
  };

  useBack(!mobileViewport && !docked && outlineMode === 'click', () => setOutlineMode('none'));

  useEffect(() => {
    if (mobileViewport || docked || outlineMode === 'none') return;
    const onKey = (e: KeyboardEvent) => {
      if (e.isComposing || e.keyCode === 229) return;
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        clearHoverTimer();
        searchFocused.current = false;
        setOutlineMode('none');
      }
    };
    const onDown = (e: PointerEvent) => {
      const target = e.target as HTMLElement | null;
      if (target?.closest('[data-outline-trigger]')) return;
      if (popoverRef.current && target && !popoverRef.current.contains(target)) {
        clearHoverTimer();
        searchFocused.current = false;
        setOutlineMode('none');
      }
    };
    window.addEventListener('keydown', onKey, true);
    document.addEventListener('pointerdown', onDown);
    return () => {
      window.removeEventListener('keydown', onKey, true);
      document.removeEventListener('pointerdown', onDown);
    };
  }, [mobileViewport, docked, outlineMode]);

  const showRail = isChatMode && !mobileViewport && !docked && !files && prompts.length >= 2;
  const outlineActive = docked || outlineMode !== 'none';
  const refillPrompt = (rawText: string) => {
    restoreToComposer(sessionId, rawText);
    toast(tp('refilled'));
  };

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
  if (info.parentId && parentExists) {
    return null;
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
          <div className="session-title-row">
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
            {!filesOnly && <span className={`status-chip st-${info.status}`}>{STATUS_LABEL[info.status]}</span>}
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
        <div className="head-toolbar">
          {sideChats.length > 0 && isChatMode && (
            <button
              type="button"
              className={`head-side-btn ${files ? 'open' : ''}`}
              title={t('sideChatsMenuTip')}
              onClick={(e) => {
                const r = e.currentTarget.getBoundingClientRect();
                const otherTools = sideChatTools(hostId).filter((tk) => tk !== info.tool);
                openMenu(
                  { clientX: r.right - 260, clientY: r.bottom + 4 },
                  [
                    ...sideChats.map((sc) => ({
                      label: sideChatTitle(sc),
                      icon: 'sideChat',
                      hint: [TOOL_LABEL[sc.tool], STATUS_LABEL[sc.status]].join(' · '),
                      run: () => openFileInPanel(hostId, sessionId, sideTabPath(sc.id)),
                    })),
                    '-' as const,
                    {
                      label: t('newSideChat'),
                      icon: 'plus',
                      hint: TOOL_LABEL[info.tool],
                      run: () => void startSideChat({ hostId, parent: info }).catch(errorToast),
                    },
                    ...otherTools.map((tk) => ({
                      label: t('newSideChatWithTool', { tool: TOOL_LABEL[tk] }),
                      icon: 'sideChat',
                      run: () => void startSideChat({ hostId, parent: info, tool: tk }).catch(errorToast),
                    })),
                  ],
                );
              }}
            >
              {attnSideCount > 0 ? <span className="wp-side-dot attn" /> : liveSideCount > 0 ? <span className="wp-side-dot running" /> : <Icon name="sideChat" size={13} />}
              <span>{t('sideChatsCount', { count: sideChats.length })}</span>
              <Icon name="down" size={11} />
            </button>
          )}
          {info.tool !== 'shell' && !filesOnly && (
            <div className="head-group hide-mobile">
              <button
                type="button"
                className="head-ghost-btn usage-btn"
                onClick={() => setUsage(true)}
                title={t('viewSessionUsageAndAccount')}
              >
                <Icon name="chart" size={14} />
                <span className="narrow-hide">{fmtUsage(info.usage) || t('usage')}</span>
              </button>
              {isChatMode && !info.sideChat && (
                <button
                  type="button"
                  className="head-ghost-btn"
                  disabled={!connected || startingSide}
                  onClick={async () => {
                    setStartingSide(true);
                    try {
                      await startSideChat({ hostId, parent: info });
                    } catch (e) {
                      errorToast(e);
                    } finally {
                      setStartingSide(false);
                    }
                  }}
                  title={t('newSideChatTip')}
                  aria-label={t('newSideChat')}
                >
                  <Icon name="sideChatPlus" size={14} />
                  <span className="narrow-hide">{t('addSideChat')}</span>
                </button>
              )}
            </div>
          )}
          {info.tool !== 'shell' && !filesOnly && <span className="head-sep hide-mobile" aria-hidden="true" />}
          <div className="head-group head-layout-group">
            {/* On mobile this spot holds "Usage" (the desktop ghost usage button is hidden), and the bottom terminal panel moves into "More" */}
            {info.tool !== 'shell' && !filesOnly && (
              <button
                type="button"
                className="head-toggle-btn show-mobile"
                title={t('viewSessionUsageAndAccount')}
                aria-label={t('usage')}
                onClick={() => setUsage(true)}
              >
                <Icon name="chart" size={16} />
              </button>
            )}
            {info.tool !== 'shell' && (
              <button
                type="button"
                className={`head-toggle-btn ${filesOnly ? '' : 'hide-mobile'} ${term ? 'active' : ''}`}
                aria-pressed={term}
                title={term ? t('hideTerminal') : t('openTerminal')}
                aria-label={term ? t('hideTerminal') : t('openTerminal')}
                onClick={toggleTerm}
              >
                <Icon name="panelBottom" size={16} />
              </button>
            )}
            {!filesOnly && (
              <button
                type="button"
                className={`head-toggle-btn ${files ? 'active' : ''}`}
                aria-pressed={files}
                title={t('workspaceFiles')}
                aria-label={t('workspaceFiles')}
                onClick={toggleFiles}
              >
                <Icon name="panelRight" size={16} />
                {attnSideCount > 0 ? (
                  <span className="head-live-badge attn">{attnSideCount}</span>
                ) : (
                  liveSideCount > 0 && <span className="head-live-badge">{liveSideCount}</span>
                )}
              </button>
            )}
            <button
              type="button"
              className="head-toggle-btn"
              title={t('more')}
              aria-label={t('more')}
              onClick={(e) => {
                const r = e.currentTarget.getBoundingClientRect();
                openMenu(
                  { clientX: r.right - 200, clientY: r.bottom + 4 },
                  sessionMenu(hostId, info, {
                    inSession: true,
                    onMemory: () => setMemoryOpen(true),
                    onNewSideChat: isChatMode && connected && !info.sideChat ? () => void startSideChat({ hostId, parent: info }).catch(errorToast) : undefined,
                    onPromptOutline: isChatMode ? toggleOutlineBtn : undefined,
                    outlineActive,
                    promptCount: prompts.length,
                    term,
                    onToggleTerm: toggleTerm,
                    style,
                    onToggleStyle: () => setChatStyle(style === 'modern' ? 'claude' : 'modern'),
                  }),
                );
              }}
            >
              <Icon name="more" size={16} />
            </button>
          </div>
        </div>
      </header>
      {info.bot && <SessionBotBar info={info} />}

      {filesOnly ? (
        connected ? (
          <FilePanel
            key={`${hostId}/${sessionId}/${activeFileCwd}`}
            hostId={hostId}
            cwd={activeFileCwd}
            baseCwd={info.cwd}
            onSwitchCwd={switchPanelCwd}
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
        <div className="session-body-split">
          <div className="session-chat-col">
            <div className="chat-stage">
              <PromptStickyBar
                prompts={prompts}
                activeSeq={promptScroll.activeSeq}
                visible={promptScroll.stickyVisible && prompts.length > 0}
                mobile={mobileViewport}
                outlineOpen={outlineActive}
                hasRail={showRail}
                hostId={hostId}
                sessionId={sessionId}
                cwd={info.cwd}
                onJump={jumpToPrompt}
                onStep={stepPrompt}
                onToggleOutline={toggleOutlineBtn}
                onRefill={refillPrompt}
              />
              {showRail && (
                <PromptTickRail
                  prompts={prompts}
                  activeSeq={promptScroll.activeSeq}
                  lit={outlineMode !== 'none'}
                  onHoverEnter={openHoverOutline}
                  onHoverLeave={scheduleHoverClose}
                  onJump={jumpToPrompt}
                />
              )}
              {!mobileViewport && !docked && outlineMode !== 'none' && (
                <div
                  ref={popoverRef}
                  className={`outline-card outline-popover ${showRail ? '' : 'no-rail'}`}
                  onMouseEnter={clearHoverTimer}
                  onMouseLeave={() => {
                    if (outlineMode === 'hover') scheduleHoverClose();
                  }}
                >
                  <PromptOutlinePanel
                    mode="popover"
                    prompts={prompts}
                    activeSeq={promptScroll.activeSeq}
                    jumpingSeq={jumpingSeq}
                    fullIndexed={data.userMessagesLoaded && !data.userMessagesUnsupported}
                    hasMore={data.hasMore}
                    loadingIndex={data.userMessagesLoading}
                    loadingOlder={data.loadingOlder}
                    pinned={outlinePinned}
                    onJump={jumpToPrompt}
                    onStep={stepPrompt}
                    onRefill={refillPrompt}
                    onTogglePin={togglePinOutline}
                    onClose={() => {
                      clearHoverTimer();
                      searchFocused.current = false;
                      setOutlineMode('none');
                    }}
                    onLoadOlder={() => loadOlder(hostId, sessionId).catch(errorToast)}
                    onSearchFocusChange={(focused) => {
                      searchFocused.current = focused;
                      if (focused) clearHoverTimer();
                    }}
                  />
                </div>
              )}
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
                onLoadOlder={(targetSeq) => loadOlder(hostId, sessionId, targetSeq).catch(errorToast)}
                focusSeq={seq}
                jumpReq={jumpReq}
                onJumpDone={() => setJumpingSeq(null)}
                promptSeqs={promptSeqs}
                onPromptScrollState={(st) => {
                  if (stepAnchorRef.current && st.activeSeq === stepAnchorRef.current.seq) {
                    stepAnchorRef.current = null;
                  }
                  setPromptScroll(st);
                }}
                pendingOutside
                skippedAsks={skippedAsks}
                onOpenTurnDiff={connected ? (turn) => showTurnDiff(hostId, sessionId, turn) : undefined}
              />
            </div>
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
          </div>
          {docked && (
            <aside className="outline-dock">
              <PromptOutlinePanel
                mode="dock"
                prompts={prompts}
                activeSeq={promptScroll.activeSeq}
                jumpingSeq={jumpingSeq}
                fullIndexed={data.userMessagesLoaded && !data.userMessagesUnsupported}
                hasMore={data.hasMore}
                loadingIndex={data.userMessagesLoading}
                loadingOlder={data.loadingOlder}
                pinned={outlinePinned}
                onJump={jumpToPrompt}
                onStep={stepPrompt}
                onRefill={refillPrompt}
                onTogglePin={togglePinOutline}
                onClose={closeOutline}
                onLoadOlder={() => loadOlder(hostId, sessionId).catch(errorToast)}
              />
            </aside>
          )}
        </div>
      )}
      {term && connected && info.tool !== 'shell' && <TermPanel key={key} hostId={hostId} info={info} onClose={toggleTerm} />}
    </div>
      {!filesOnly && files && connected && full && <div className="panel-scrim hide-mobile" onClick={() => setFull(false)} title={t('exitFullscreen')} />}
      {!filesOnly && files && connected && !full && fileOpen && <div className="panel-resizer hide-mobile" onPointerDown={dragPanel} title={t('dragToResize')} />}
      {!filesOnly && files && connected && (
        <FilePanel
          key={`${hostId}/${sessionId}/${activeFileCwd}`}
          hostId={hostId}
          cwd={activeFileCwd}
          baseCwd={info.cwd}
          onSwitchCwd={switchPanelCwd}
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
      {isChatMode && mobileViewport && (
        <PromptOutlineSheet
          open={outlineMode !== 'none'}
          prompts={prompts}
          activeSeq={promptScroll.activeSeq}
          jumpingSeq={jumpingSeq}
          fullIndexed={data.userMessagesLoaded && !data.userMessagesUnsupported}
          hasMore={data.hasMore}
          loadingIndex={data.userMessagesLoading}
          loadingOlder={data.loadingOlder}
          pinned={false}
          onJump={jumpToPrompt}
          onStep={stepPrompt}
          onRefill={refillPrompt}
          onClose={() => setOutlineMode('none')}
          onLoadOlder={() => loadOlder(hostId, sessionId).catch(errorToast)}
        />
      )}
    </div>
  );
}
