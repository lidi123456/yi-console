// Browser E2E fixture mounting real ChatView + Composer + Overlays + Toasts for user message re-edit verification.
import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import '@fontsource-variable/inter';
import '../src/back';
import { ChatView, setChatStyle } from '../src/components/ChatView';
import { Composer, startReeditToComposer } from '../src/components/Composer';
import { Icon, ToolBadge } from '../src/components/Icon';
import { Overlays } from '../src/components/Overlay';
import { serializeContextMessage } from '../src/contextRefs';
import { setLangPref, useLang } from '../src/i18n';
import { setState, useStore } from '../src/store';
import { setTheme } from '../src/theme';
import type { HubEvent, SessionInfo } from '../src/types';
import '../src/styles.css';

const TINY_PNG_B64 =
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkWPjfDwAEfQHzbpFsPDAAAAAElFTkSuQmCC';

const calls: Array<{ method: string; params: Record<string, unknown> }> = [];
let refresh = () => {};

const MSG1_TEXT = serializeContextMessage(
  '帮我把用户消息气泡下面加一个重新编辑按钮，记得保留引用的代码片段和图片。',
  [
    {
      id: 'ref-chatview',
      kind: 'code',
      path: 'app/src/components/ChatView.tsx',
      fromLine: 2414,
      toLine: 2469,
      content: 'case "user": {\n  return <div className="msg-user">...</div>;\n}',
    },
  ],
);

let events: HubEvent[] = [
  {
    seq: 1,
    ts: 1732068000000,
    kind: 'user_message',
    text: MSG1_TEXT,
    images: [{ id: 'img-1', mimeType: 'image/png' }],
  },
  {
    seq: 2,
    ts: 1732068002000,
    kind: 'assistant_text',
    id: 'a-1',
    text: '已确认 `ChatView.tsx` 中的用户气泡操作栏结构，可以新增重新编辑入口并支持历史图片还原。',
  },
  {
    seq: 3,
    ts: 1732068003000,
    kind: 'turn_end',
    model: 'claude-sonnet-4-6',
    stopReason: 'end_turn',
  },
  {
    seq: 4,
    ts: 1732068060000,
    kind: 'user_message',
    text: '顺便再确认一下暗色模式和手机端显示是否正常。',
  },
  {
    seq: 5,
    ts: 1732068063000,
    kind: 'assistant_text',
    id: 'a-2',
    text: '暗色主题与移动端 28×28 触控热区已按双端规范对齐，可直接点击操作栏验证。',
  },
  {
    seq: 6,
    ts: 1732068064000,
    kind: 'turn_end',
    model: 'claude-sonnet-4-6',
    stopReason: 'end_turn',
  },
];

let nextSeq = 7;

const info: SessionInfo = {
  id: 'reedit-e2e-session',
  title: '优化用户消息操作栏',
  cwd: 'D:\\repo\\yi-console',
  tool: 'claude',
  mode: 'structured',
  status: 'idle',
  model: 'claude-sonnet-4-6',
  permissionMode: 'default',
  createdAt: 1732068000000,
  updatedAt: 1732068064000,
  lastSeq: 6,
  usage: {
    inputTokens: 4200,
    outputTokens: 640,
    cacheReadTokens: 8000,
    cacheWriteTokens: 0,
    costUsd: 0.032,
    credits: 0,
  },
  contextPercent: 18,
  queued: [],
};

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
    if (method === 'session.image') {
      setTimeout(() => {
        this.onmessage?.({
          data: JSON.stringify({
            id: req.id,
            result: { mimeType: 'image/png', data: TINY_PNG_B64 },
          }),
        });
      }, 25);
      return;
    }
    if (method === 'session.send') {
      const imgs = Array.isArray(params.images)
        ? params.images.map((im: { mimeType: string }, idx: number) => ({
            id: `sent-img-${nextSeq}-${idx}`,
            mimeType: im.mimeType || 'image/png',
          }))
        : [];
      events = [
        ...events,
        {
          seq: nextSeq++,
          ts: Date.now(),
          kind: 'user_message',
          text: String(params.text ?? ''),
          ...(imgs.length > 0 ? { images: imgs } : {}),
        },
      ];
      refresh();
      queueMicrotask(() => {
        this.onmessage?.({ data: JSON.stringify({ id: req.id, result: { ok: true } }) });
      });
      return;
    }
    queueMicrotask(() => {
      this.onmessage?.({ data: JSON.stringify({ id: req.id, result: {} }) });
    });
  }
}

Object.assign(window, {
  WebSocket: FixtureSocket,
  fixtureCalls: calls,
  setLangPref,
  setTheme,
  setChatStyle,
});

function Toasts() {
  const toasts = useStore((s) => s.toasts);
  return (
    <div className="toasts">
      {toasts.map((t) => (
        <div key={t.id} className={`toast toast-${t.kind}`}>
          {t.kind === 'loading' && <span className="spinner" />}
          <span>{t.text}</span>
          {t.action && (
            <button
              className="link-btn"
              onClick={() => {
                t.action!.run();
                setState((s) => ({ toasts: s.toasts.filter((x) => x.id !== t.id) }));
              }}
            >
              {t.action.label}
            </button>
          )}
        </div>
      ))}
    </div>
  );
}

function Fixture() {
  useLang();
  const [, render] = useState(0);
  refresh = () => render((n) => n + 1);

  return (
    <div className="app" style={{ height: '100vh', minHeight: 0 }}>
      <aside className="sidebar hide-mobile" style={{ width: 210, padding: '14px 10px', display: 'flex', flexDirection: 'column', gap: 4 }}>
        <div className="host-name" style={{ padding: '4px 10px 10px' }}>Yi Console</div>
        <div className="project-name" style={{ padding: '8px 10px 4px' }}>yi-console</div>
        <div className="session-row active" style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '6px 10px', borderRadius: 8, background: 'var(--side-active)' }}>
          <ToolBadge tool="claude" size={16} />
          <span className="session-title truncate">{info.title}</span>
        </div>
      </aside>
      <main className="main" style={{ minHeight: 0, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
        <div className="session-view" style={{ flex: 1, minHeight: 0, display: 'flex', flexDirection: 'column' }}>
          <header className="session-head">
            <ToolBadge tool="claude" size={20} />
            <div className="session-meta">
              <div className="session-name truncate">{info.title}</div>
              <div className="muted small truncate">Claude Code · ~/yi-console</div>
            </div>
            <button className="icon-btn" type="button" title="More">
              <Icon name="more" size={18} />
            </button>
          </header>
          <div className="chat-stage" style={{ flex: 1, minHeight: 0, display: 'flex', flexDirection: 'column' }}>
            <ChatView
              events={events}
              live={new Map()}
              liveThinking={new Map()}
              info={info}
              hostId="local"
              onAnswer={() => {}}
              onReedit={(msg) => startReeditToComposer('local', info.id, msg)}
            />
          </div>
          <div className="composer-wrap">
            <Composer
              hostId="local"
              info={info}
              connected
              onPrewarm={() => {}}
              onInterrupt={() => {}}
              onExport={() => {}}
              onRename={() => {}}
            />
          </div>
        </div>
      </main>
      <Toasts />
      <Overlays />
    </div>
  );
}

setLangPref('zh');
setChatStyle('modern');
createRoot(document.getElementById('root')!).render(<Fixture />);
