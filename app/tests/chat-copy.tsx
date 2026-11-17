// Browser regression fixture for chat code/table/tool copy buttons and mobile text selection.
import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import '../src/back';
import { ChatView, setChatStyle } from '../src/components/ChatView';
import { Overlays, useContextMenu } from '../src/components/Overlay';
import { setLangPref } from '../src/i18n';
import { setTheme } from '../src/theme';
import type { HubEvent, SessionInfo } from '../src/types';
import '../src/styles.css';

const SAMPLE_MD = [
  '排查完成，可以执行下面这段构建与校验脚本：',
  '',
  '```ts',
  'export async function verifyCopy(input: string): Promise<boolean> {',
  '  const trimmed = input.trim();',
  '  return trimmed.length > 0;',
  '}',
  '```',
  '',
  '各项平台实测对比表：',
  '',
  '| 平台 | 对齐 `:---:` | 命令示例 |',
  '| :--- | :---: | ---: |',
  '| Windows | `x86_64` | `cargo test \\| tee` |',
  '| Android | **aarch64** | `yc-android.cmd` |',
  '',
  '长代码块滚动测试：',
  '',
  '```bash',
  ...Array.from({ length: 48 }, (_, i) => `echo "step-${i + 1}: verifying sticky copy button"`),
  '```',
].join('\n');

const LONG_TOOL_OUT = Array.from({ length: 450 }, (_, i) => `line-${i + 1}: ok`).join('\n');

const INITIAL_EVENTS: HubEvent[] = [
  { seq: 1, ts: 1700000000000, kind: 'user_message', text: '帮我检查一下复制按钮和手机端选词体验' },
  {
    seq: 2,
    ts: 1700000001000,
    kind: 'tool_call',
    id: 'tool-1',
    name: 'Bash',
    title: 'cargo test --workspace',
    input: { command: 'cargo test --workspace -- --nocapture' },
  },
  {
    seq: 3,
    ts: 1700000002000,
    kind: 'tool_result',
    id: 'tool-1',
    output: LONG_TOOL_OUT,
    isError: false,
  },
  {
    seq: 4,
    ts: 1700000003000,
    kind: 'assistant_text',
    id: 'msg-1',
    text: SAMPLE_MD,
  },
  {
    seq: 5,
    ts: 1700000004000,
    kind: 'permission_request',
    requestId: 'perm-1',
    title: '允许执行部署检查命令？',
    toolName: 'Bash',
    input: { command: 'git status --porcelain -b' },
    options: [
      { id: 'allow', label: '允许一次', kind: 'allow_once' },
      { id: 'deny', label: '拒绝', kind: 'reject_once' },
    ],
  },
  {
    seq: 6,
    ts: 1700000005000,
    kind: 'turn_end',
    model: 'claude-sonnet-4-5',
    stopReason: 'end_turn',
  },
];

const INFO: SessionInfo = {
  id: 'sess-copy-test',
  title: 'Copy & Selection Fixture',
  cwd: 'D:\\repo\\yi-console',
  tool: 'claude',
  mode: 'structured',
  status: 'running',
  createdAt: 1700000000000,
  updatedAt: 1700000005000,
  pendingPermissions: 1,
};

function Fixture() {
  useContextMenu();
  const [events, setEvents] = useState<HubEvent[]>(INITIAL_EVENTS);
  const [liveText, setLiveText] = useState('');
  Object.assign(window, {
    setEvents,
    setLiveText,
    setChatStyle,
    setTheme,
     setLangPref,
  });
  const live = new Map<string, string>();
  if (liveText) live.set('live-1', liveText);
  return (
    <div className="app" style={{ height: '100vh', display: 'flex', flexDirection: 'column' }}>
      <main className="main" style={{ flex: 1, minHeight: 0, display: 'flex', flexDirection: 'column' }}>
        <ChatView events={events} live={live} info={INFO} onAnswer={() => {}} />
        <textarea aria-label="fixture-input" defaultValue="测试输入框长按原生菜单" style={{ margin: 8, height: 40 }} />
      </main>
      <Overlays />
    </div>
  );
}

setLangPref('zh');
createRoot(document.getElementById('root')!).render(<Fixture />);
