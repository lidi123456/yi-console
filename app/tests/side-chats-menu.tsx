// Browser E2E fixture mounting real SideChatsMenu against reactive store state and stubbed session.delete RPC.
import { useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';
import '@fontsource-variable/inter';
import { handleBack } from '../src/back';
import { Icon, ToolBadge } from '../src/components/Icon';
import { SideChatsMenu } from '../src/components/SideChatTab';
import { getT, setLangPref, useLang } from '../src/i18n';
import { getActiveReviewId, setActiveReviewId } from '../src/reviewState';
import { setState, useStore } from '../src/store';
import { setTheme } from '../src/theme';
import type { SessionInfo, Usage } from '../src/types';
import '../src/styles.css';

const t = getT('sessionView');
const query = new URLSearchParams(location.search);

const ZERO_USAGE: Usage = {
  inputTokens: 0,
  outputTokens: 0,
  cacheReadTokens: 0,
  cacheWriteTokens: 0,
  costUsd: 0,
  credits: 0,
};

function makeInitialSessions(): SessionInfo[] {
  const now = 1732068000000;
  return [
    {
      id: 'main-1',
      tool: 'kiro',
      mode: 'structured',
      cwd: '/data/git/yi-console',
      title: '功',
      status: 'running',
      externalId: 'ext-main',
      createdAt: now,
      updatedAt: now,
      lastSeq: 12,
      pendingPermissions: 0,
      usage: { ...ZERO_USAGE, credits: 71.69 },
    },
    {
      id: 'sc-review-1',
      parentId: 'main-1',
      sideChat: true,
      tool: 'codex',
      mode: 'structured',
      cwd: '/data/git/yi-console',
      title: 'Codex 审阅 · 未提交 (仅已跟踪)',
      status: 'running',
      externalId: null,
      createdAt: now + 1000,
      updatedAt: now + 1000,
      lastSeq: 2,
      pendingPermissions: 0,
      usage: { ...ZERO_USAGE },
      review: {
        scope: 'uncommitted',
        uncommittedFilter: 'tracked',
        scopeLabel: '未提交 (仅已跟踪)',
      },
    },
    {
      id: 'sc-used-2',
      parentId: 'main-1',
      sideChat: true,
      tool: 'codex',
      mode: 'structured',
      cwd: '/data/git/yi-console',
      title: '看下主会话进展怎么样了',
      status: 'idle',
      externalId: null,
      createdAt: now + 2000,
      updatedAt: now + 2000,
      lastSeq: 4,
      pendingPermissions: 0,
      usage: { ...ZERO_USAGE },
    },
    {
      id: 'sc-empty-3',
      parentId: 'main-1',
      sideChat: true,
      tool: 'kiro',
      mode: 'structured',
      cwd: '/data/git/yi-console',
      title: '',
      status: 'idle',
      externalId: null,
      createdAt: now + 3000,
      updatedAt: now + 3000,
      lastSeq: 0,
      pendingPermissions: 0,
      usage: { ...ZERO_USAGE },
    },
  ];
}

const calls: Array<{ method: string; params: Record<string, unknown> }> = [];
const openedTabs: string[] = [];
const createdTools: Array<string | undefined> = [];

function resetFixtureState() {
  calls.length = 0;
  openedTabs.length = 0;
  createdTools.length = 0;
  setActiveReviewId('local', 'main-1', 'sc-review-1');
  setState({
    hosts: [
      {
        config: { id: 'local', name: 'local', kind: 'local' },
        status: {
          status: 'connected',
          hello: {
            version: 'test',
            home: '/home/dev',
            tools: [
              { kind: 'kiro', available: true },
              { kind: 'claude', available: true },
              { kind: 'codex', available: true },
              { kind: 'cursor', available: true },
            ],
          } as any,
        },
      },
    ],
    sessions: { local: makeInitialSessions() },
  });
}

class FixtureSocket {
  readyState = 0;
  onopen?: () => void;
  onmessage?: (event: { data: string }) => void;
  constructor() {
    setTimeout(() => {
      this.readyState = 1;
      this.onopen?.();
    }, 5);
  }
  send(raw: string) {
    const req = JSON.parse(raw);
    const { method, params } = req.args ?? {};
    calls.push({ method, params });
    if (method === 'session.delete') {
      const targetId = String(params?.sessionId ?? '');
      setState((s) => ({
        sessions: {
          ...s.sessions,
          local: (s.sessions.local ?? []).filter((x) => x.id !== targetId),
        },
      }));
    }
    queueMicrotask(() => {
      this.onmessage?.({ data: JSON.stringify({ id: req.id, result: { ok: true } }) });
    });
  }
}

resetFixtureState();
setLangPref(query.get('lang') === 'en' ? 'en' : 'zh');
setTheme(query.get('theme') === 'dark' ? 'dark' : 'light');

Object.assign(window, {
  WebSocket: FixtureSocket,
  fixtureCalls: calls,
  fixtureOpenedTabs: openedTabs,
  fixtureCreatedTools: createdTools,
  fixtureBack: handleBack,
  fixtureReset: resetFixtureState,
  fixtureSetTheme: setTheme,
  fixtureSetLang: setLangPref,
  fixtureGetActiveReviewId: () => getActiveReviewId('local', 'main-1'),
});

function Fixture() {
  useLang();
  const sessions = useStore((s) => s.sessions.local ?? []);
  const parent = sessions.find((s) => s.id === 'main-1');
  const sideChats = sessions.filter((s) => s.parentId === 'main-1' && (s.sideChat || !!s.review));
  const [anchor, setAnchor] = useState<{ right: number; bottom: number } | null>(null);

  useEffect(() => {
    if (sideChats.length === 0) setAnchor(null);
  }, [sideChats.length]);

  if (!parent) return null;
  const liveCount = sideChats.filter((x) => x.status === 'running' || x.status === 'starting').length;

  return (
    <div className="app" style={{ height: '100vh', minHeight: 0, padding: 12 }}>
      <main className="main" style={{ flex: 1, minHeight: 0, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
        <header className="session-head">
          <ToolBadge tool={parent.tool} size={20} />
          <div className="session-meta">
            <div className="session-title-row">
              <div className="session-name truncate">{parent.title}</div>
              <span className="status-chip st-running">运行中</span>
            </div>
            <div className="muted small truncate">Kiro · /data/git/yi-console</div>
          </div>
          <div className="head-toolbar">
            {sideChats.length > 0 && (
              <>
                <button
                  type="button"
                  className={`head-side-btn ${anchor ? 'open' : ''}`}
                  aria-expanded={!!anchor}
                  onClick={(e) => {
                    if (anchor) {
                      setAnchor(null);
                      return;
                    }
                    const r = e.currentTarget.getBoundingClientRect();
                    setAnchor({ right: r.right, bottom: r.bottom + 4 });
                  }}
                >
                  {liveCount > 0 ? <span className="wp-side-dot running" /> : <Icon name="sideChat" size={13} />}
                  <span>{t('sideChatsCount', { count: sideChats.length })}</span>
                  <Icon name="down" size={11} />
                </button>
                {anchor && (
                  <SideChatsMenu
                    hostId="local"
                    parent={parent}
                    sideChats={sideChats}
                    connected
                    anchor={anchor}
                    onClose={() => setAnchor(null)}
                    onOpenSideChat={(sc) => openedTabs.push(sc.id)}
                    onCreateSideChat={(tool) => createdTools.push(tool)}
                  />
                )}
              </>
            )}
          </div>
        </header>
      </main>
    </div>
  );
}

createRoot(document.getElementById('root')!).render(<Fixture />);
