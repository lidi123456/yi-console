// Browser E2E fixture for the refreshed compact chat dialog across multiple real scenarios.
import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import '@fontsource-variable/inter';
import '../src/back';
import { ChatView, PermissionCard, setChatStyle } from '../src/components/ChatView';
import { Composer } from '../src/components/Composer';
import { Icon, ToolBadge } from '../src/components/Icon';
import { Overlays, useContextMenu } from '../src/components/Overlay';
import { setLangPref } from '../src/i18n';
import { setTheme } from '../src/theme';
import type { HubEvent, SessionInfo } from '../src/types';
import '../src/styles.css';

const REPLY_MD = [
  '已完成改造：在 `app/src/components/ChatView.tsx` 中新增了 `summarizeGroup()`，按读取、搜索、编辑与命令聚合语义摘要，并在底部展示本轮改动文件统计。',
  '',
  '```typescript',
  '// Unified turn spacing & semantic activity summary',
  'export function summarizeSteps(steps: Step[]): string {',
  "  const reads = steps.filter((s) => s.kind === 'read').length;",
  "  const edits = steps.filter((s) => s.kind === 'edit').length;",
  '  return [reads && `读取 ${reads} 文件`, edits && `编辑 ${edits} 处`]',
  "    .filter(Boolean).join(' · ');",
  '}',
  '```',
  '',
  '类型检查、i18n 双语校验与桌面/移动端回归测试均已通过。',
].join('\n');

const TABLE_MD = [
  '已按三种主流产品的设计特征整理对比如下：',
  '',
  '| 模块 | 改造前 | 改造后（方案 C） | 状态 |',
  '| :--- | :--- | :--- | :---: |',
  '| 工具折叠组 | 机械数字 `N 次工具调用` | 语义动作工作卡 + 文件胶囊 | 已完成 |',
  '| 回复底栏 | 整行铺满 Token / 模型 | 变更胶囊 `+add -del` + 极简耗时/费用 | 已完成 |',
  '| 代码块 | 纯 `<pre>` 容器 | 语言顶栏 + 悬浮 sticky 复制按钮 | 已完成 |',
  '| 输入框与队列 | 独立方框堆叠 | 2 行圆角输入区 + 同宽抽屉附着条 | 已完成 |',
].join('\n');

const SCENARIOS: Record<
  string,
  { events: HubEvent[]; info: SessionInfo; live?: Record<string, string>; liveThinking?: Record<string, string>; pendingPerm?: boolean }
> = {
  default: {
    events: [
      {
        seq: 1,
        ts: 1732068000000,
        kind: 'user_message',
        text: '帮我把工具调用折叠组改成语义化摘要，并把编辑行数统计和本轮文件变更胶囊也加上。',
      },
      {
        seq: 2,
        ts: 1732068001000,
        kind: 'tool_call',
        id: 't-read',
        name: 'Read',
        title: 'Read ChatView.tsx',
        input: { file_path: 'app/src/components/ChatView.tsx' },
      },
      {
        seq: 3,
        ts: 1732068002500,
        kind: 'tool_result',
        id: 't-read',
        output: 'Read 180 lines from app/src/components/ChatView.tsx',
        isError: false,
      },
      {
        seq: 4,
        ts: 1732068003000,
        kind: 'tool_call',
        id: 't-grep',
        name: 'Grep',
        title: 'Grep ActGroup',
        input: { pattern: 'ActGroup', path: 'app/src' },
      },
      {
        seq: 5,
        ts: 1732068004500,
        kind: 'tool_result',
        id: 't-grep',
        output: 'Found 4 matches in app/src/components/ChatView.tsx',
        isError: false,
      },
      {
        seq: 6,
        ts: 1732068005000,
        kind: 'tool_call',
        id: 't-edit',
        name: 'Edit',
        title: 'Edit ChatView.tsx',
        input: { file_path: 'app/src/components/ChatView.tsx' },
        diffs: [
          {
            path: 'app/src/components/ChatView.tsx',
            oldText: 'function ActGroup() {\n  return <div>4 tool calls</div>;\n}\n',
            newText:
              'function summarizeGroup(tools) {\n  return "Read 1 file · Edited ChatView.tsx";\n}\n\nfunction ActGroup() {\n  return <div className="act-group">semantic card</div>;\n}\n',
          },
        ],
      },
      {
        seq: 7,
        ts: 1732068009000,
        kind: 'tool_result',
        id: 't-edit',
        output: 'Updated app/src/components/ChatView.tsx',
        isError: false,
      },
      {
        seq: 8,
        ts: 1732068009500,
        kind: 'tool_call',
        id: 't-cmd',
        name: 'Bash',
        title: 'npm run build',
        input: { command: 'npm run i18n:check && tsc --noEmit && vite build' },
      },
      {
        seq: 9,
        ts: 1732068013500,
        kind: 'tool_result',
        id: 't-cmd',
        output:
          '✓ Resources interface written to src/locales/resources.d.ts\n✓ en: 100% (2387/2387 keys)\n✓ No new lint issues found\n✓ 2018 modules transformed.\n✓ built in 13.91s',
        isError: false,
      },
      {
        seq: 10,
        ts: 1732068014000,
        kind: 'assistant_text',
        id: 'a-1',
        text: REPLY_MD,
      },
      {
        seq: 11,
        ts: 1732068014500,
        kind: 'usage',
        usage: {
          inputTokens: 12400,
          outputTokens: 1420,
          cacheReadTokens: 30400,
          cacheWriteTokens: 0,
          costUsd: 0.084,
          credits: 0,
        },
      },
      {
        seq: 12,
        ts: 1732068015000,
        kind: 'turn_end',
        model: 'claude-sonnet-4-6',
        stopReason: 'end_turn',
      },
      {
        seq: 13,
        ts: 1732068050000,
        kind: 'user_message',
        text: '亮色和暗色模式都检查一下，确保气泡对比度和代码块复制按钮都正常工作。',
      },
    ],
    info: {
      id: 'sess-refresh',
      title: '优化聊天对话框视觉风格',
      cwd: 'D:\\agent-build\\yi-console',
      tool: 'claude',
      mode: 'structured',
      status: 'running',
      model: 'claude-sonnet-4-6',
      permissionMode: 'acceptEdits',
      contextPercent: 38,
      createdAt: 1732068000000,
      updatedAt: 1732068050000,
      pendingPermissions: 0,
      usage: { inputTokens: 12400, outputTokens: 1420, cacheReadTokens: 30400, cacheWriteTokens: 0, costUsd: 0.084, credits: 0 },
      queued: [{ id: 'q-1', text: '完成后把实机截图同步到 Windows 下载目录', images: 0, ts: 1732068055000 }],
    },
  },

  'tool-expanded': {
    events: [
      {
        seq: 1,
        ts: 1732068000000,
        kind: 'user_message',
        text: '帮我把工作卡里的工具行和构建命令展开看看实际内容长什么样。',
      },
      {
        seq: 2,
        ts: 1732068000500,
        kind: 'thinking',
        id: 'th-1',
        text: '先检查 ChatView.tsx 里的 ActGroup 和 ModernTool 渲染结构，确保展开 diff 与命令输出时保持复制按钮和紧凑排版。',
      },
      {
        seq: 3,
        ts: 1732068002000,
        kind: 'tool_call',
        id: 't-edit-2',
        name: 'Edit',
        title: 'Edit ChatView.tsx',
        input: { file_path: 'app/src/components/ChatView.tsx' },
        diffs: [
          {
            path: 'app/src/components/ChatView.tsx',
            oldText:
              'function ActGroup() {\n  return (\n    <div className="act-group">\n      <span className="ag-sum">4 tool calls</span>\n    </div>\n  );\n}\n',
            newText:
              'function summarizeGroup(tools) {\n  return "读取 1 个文件 · 编辑 ChatView.tsx · 运行 1 条命令";\n}\n\nfunction ActGroup({ tools }) {\n  return (\n    <div className="act-group done">\n      <span className="ag-dot" />\n      <span className="ag-sum">{summarizeGroup(tools)}</span>\n    </div>\n  );\n}\n',
          },
        ],
      },
      {
        seq: 4,
        ts: 1732068005000,
        kind: 'tool_result',
        id: 't-edit-2',
        output: 'Updated app/src/components/ChatView.tsx',
        isError: false,
      },
      {
        seq: 5,
        ts: 1732068006000,
        kind: 'tool_call',
        id: 't-cmd-2',
        name: 'Bash',
        title: 'npm run build',
        input: { command: 'npm run i18n:check && tsc --noEmit && vite build' },
      },
      {
        seq: 6,
        ts: 1732068011000,
        kind: 'tool_result',
        id: 't-cmd-2',
        output:
          '✓ Resources interface written to src/locales/resources.d.ts\n✓ en: 100% (2387/2387 keys)\n✓ No new lint issues found\n✓ 2018 modules transformed.\n✓ built in 13.91s',
        isError: false,
      },
      {
        seq: 7,
        ts: 1732068012000,
        kind: 'assistant_text',
        id: 'a-exp',
        text: '点击工作卡内的任意一行都可以直接展开详情：编辑类工具显示行级 Diff，终端命令显示完整输入与输出，并支持一键复制。',
      },
      {
        seq: 8,
        ts: 1732068012500,
        kind: 'turn_end',
        model: 'claude-sonnet-4-6',
        stopReason: 'end_turn',
      },
    ],
    info: {
      id: 'sess-expanded',
      title: '工具行展开详情演示',
      cwd: 'D:\\agent-build\\yi-console',
      tool: 'claude',
      mode: 'structured',
      status: 'idle',
      model: 'claude-sonnet-4-6',
      permissionMode: 'acceptEdits',
      contextPercent: 41,
      createdAt: 1732068000000,
      updatedAt: 1732068012500,
      pendingPermissions: 0,
      usage: { inputTokens: 8200, outputTokens: 640, cacheReadTokens: 18000, cacheWriteTokens: 0, costUsd: 0.052, credits: 0 },
    },
  },

  'running-error': {
    events: [
      {
        seq: 1,
        ts: 1732068000000,
        kind: 'user_message',
        text: '跑一下前端构建和 E2E 回归，如果报错直接帮我定位修复。',
      },
      {
        seq: 2,
        ts: 1732068001000,
        kind: 'tool_call',
        id: 't-err-cmd',
        name: 'Bash',
        title: 'npm run i18n:check',
        input: { command: 'npm run i18n:check' },
      },
      {
        seq: 3,
        ts: 1732068004500,
        kind: 'tool_result',
        id: 't-err-cmd',
        output: 'exit code: 1\nError: Missing key "chatView:groupReadFiles_one" in en/chatView.json\n  at checkStatus (i18next-cli:142:11)',
        isError: true,
      },
      {
        seq: 4,
        ts: 1732068005000,
        kind: 'assistant_text',
        id: 'a-err',
        text: '检测到 `en/chatView.json` 缺少英文单数复数键 `groupReadFiles_one`，我现在补齐后重新构建验证。',
      },
      {
        seq: 5,
        ts: 1732068005500,
        kind: 'turn_end',
        model: 'claude-sonnet-4-6',
        stopReason: 'end_turn',
      },
      {
        seq: 6,
        ts: 1732068010000,
        kind: 'user_message',
        text: '好的，顺便把英文版语义摘要也跑一遍 E2E 验证。',
      },
      {
        seq: 7,
        ts: Date.now() - 8000,
        kind: 'tool_call',
        id: 't-live-1',
        name: 'Edit',
        title: 'Edit en/chatView.json',
        input: { file_path: 'app/src/locales/en/chatView.json' },
        diffs: [
          {
            path: 'app/src/locales/en/chatView.json',
            oldText: '  "close": "Close"\n}\n',
            newText: '  "close": "Close",\n  "groupReadFiles_one": "Read {{count}} file"\n}\n',
          },
        ],
      },
      {
        seq: 8,
        ts: Date.now() - 5000,
        kind: 'tool_result',
        id: 't-live-1',
        output: 'Updated app/src/locales/en/chatView.json',
        isError: false,
      },
      {
        seq: 9,
        ts: Date.now() - 4000,
        kind: 'tool_call',
        id: 't-live-2',
        name: 'Bash',
        title: 'node app/tests/chat-refresh.mjs',
        input: { command: 'node app/tests/chat-refresh.mjs' },
      },
    ],
    liveThinking: {
      th: '已补齐英文复数键，正在等待 Playwright E2E 校验亮/暗色主题与双语语义摘要…',
    },
    info: {
      id: 'sess-run-err',
      title: '排查 i18n 报错与运行中状态',
      cwd: 'D:\\agent-build\\yi-console',
      tool: 'claude',
      mode: 'structured',
      status: 'running',
      model: 'claude-sonnet-4-6',
      permissionMode: 'acceptEdits',
      contextPercent: 45,
      createdAt: 1732068000000,
      updatedAt: 1732068016000,
      pendingPermissions: 0,
      usage: { inputTokens: 15100, outputTokens: 980, cacheReadTokens: 21000, cacheWriteTokens: 0, costUsd: 0.068, credits: 0 },
    },
  },

  'cards-table': {
    events: [
      {
        seq: 1,
        ts: 1732068000000,
        kind: 'user_message',
        text: '给我列个改造前后对照表，并准备执行本地构建校验命令。',
      },
      {
        seq: 2,
        ts: 1732068001000,
        kind: 'plan',
        entries: [
          { content: '升级语义工作卡与文件芯片样式', status: 'completed' },
          { content: '补齐中英双语 i18n 并执行类型检查', status: 'completed' },
          { content: '在 Windows 上运行 Playwright E2E 回归', status: 'in_progress' },
          { content: '导出最终实机效果图到下载目录', status: 'pending' },
        ],
      },
      {
        seq: 3,
        ts: 1732068003000,
        kind: 'assistant_text',
        id: 'a-tbl',
        text: TABLE_MD,
      },
      {
        seq: 4,
        ts: 1732068004000,
        kind: 'permission_request',
        requestId: 'perm-demo',
        toolName: 'Bash',
        title: '允许执行终端命令？',
        input: { command: 'cd /d D:\\agent-build\\yi-console\\app && npm run build' },
        options: [
          { id: 'allow_once', label: '允许一次', kind: 'allow_once' },
          { id: 'allow_session', label: '本会话始终允许', kind: 'allow_always' },
          { id: 'reject', label: '拒绝', kind: 'reject_once' },
        ],
      },
    ],
    pendingPerm: true,
    info: {
      id: 'sess-cards',
      title: '任务清单、表格与审批卡片',
      cwd: 'D:\\agent-build\\yi-console',
      tool: 'claude',
      mode: 'structured',
      status: 'waiting-permission',
      model: 'claude-sonnet-4-6',
      permissionMode: 'default',
      contextPercent: 52,
      createdAt: 1732068000000,
      updatedAt: 1732068004000,
      pendingPermissions: 1,
      usage: { inputTokens: 18400, outputTokens: 1620, cacheReadTokens: 28000, cacheWriteTokens: 0, costUsd: 0.095, credits: 0 },
    },
  },

  multiturn: {
    events: [
      {
        seq: 1,
        ts: 1732067800000,
        kind: 'user_message',
        text: '先帮我梳理一下 `styles.css` 里聊天区和输入框的样式位置。',
      },
      {
        seq: 2,
        ts: 1732067801000,
        kind: 'tool_call',
        id: 'm-1',
        name: 'Grep',
        title: 'Grep .chat.modern',
        input: { pattern: '\\.chat\\.modern' },
      },
      { seq: 3, ts: 1732067802000, kind: 'tool_result', id: 'm-1', output: 'Found 28 matches', isError: false },
      {
        seq: 4,
        ts: 1732067803000,
        kind: 'tool_call',
        id: 'm-2',
        name: 'Read',
        title: 'Read styles.css',
        input: { file_path: 'app/src/styles.css' },
      },
      { seq: 5, ts: 1732067805000, kind: 'tool_result', id: 'm-2', output: 'Read 220 lines', isError: false },
      {
        seq: 6,
        ts: 1732067806000,
        kind: 'assistant_text',
        id: 'ma-1',
        text: '已定位完毕：聊天区默认紧凑样式位于 `styles.css` 第 7090 行附近，暗色变量分别定义在系统跟随块与 `:root[data-theme="dark"]` 中，Buzz 外壳覆盖在末尾。',
      },
      {
        seq: 7,
        ts: 1732067806500,
        kind: 'usage',
        usage: { inputTokens: 6400, outputTokens: 320, cacheReadTokens: 12000, cacheWriteTokens: 0, costUsd: 0.028, credits: 0 },
      },
      { seq: 8, ts: 1732067807000, kind: 'turn_end', model: 'claude-sonnet-4-6', stopReason: 'end_turn' },
      {
        seq: 9,
        ts: 1732067900000,
        kind: 'user_message',
        text: '很好，按方案 C 把工作卡、变更胶囊、代码块语言栏和输入区都改掉吧。',
      },
      {
        seq: 10,
        ts: 1732067901000,
        kind: 'tool_call',
        id: 'm-3',
        name: 'Edit',
        title: 'Edit ChatView.tsx',
        input: { file_path: 'app/src/components/ChatView.tsx' },
        diffs: [{ path: 'app/src/components/ChatView.tsx', oldText: 'a\n', newText: 'a\nb\nc\nd\n' }],
      },
      { seq: 11, ts: 1732067904000, kind: 'tool_result', id: 'm-3', output: 'ok', isError: false },
      {
        seq: 12,
        ts: 1732067905000,
        kind: 'tool_call',
        id: 'm-4',
        name: 'Edit',
        title: 'Edit styles.css',
        input: { file_path: 'app/src/styles.css' },
        diffs: [{ path: 'app/src/styles.css', oldText: 'x\ny\n', newText: 'x\nz1\nz2\nz3\nz4\n' }],
      },
      { seq: 13, ts: 1732067909000, kind: 'tool_result', id: 'm-4', output: 'ok', isError: false },
      {
        seq: 14,
        ts: 1732067910000,
        kind: 'tool_call',
        id: 'm-5',
        name: 'Bash',
        title: 'npm run build',
        input: { command: 'npm run build' },
      },
      { seq: 15, ts: 1732067916000, kind: 'tool_result', id: 'm-5', output: 'built in 13.9s', isError: false },
      {
        seq: 16,
        ts: 1732067917000,
        kind: 'assistant_text',
        id: 'ma-2',
        text: '已全部修改完成并通过 `npm run build`（含 i18n 检查与 TypeScript 校验）。历史轮次的工作卡会自动收起成一行语义摘要，长对话扫读更清爽。',
      },
      {
        seq: 17,
        ts: 1732067917500,
        kind: 'usage',
        usage: { inputTokens: 14200, outputTokens: 890, cacheReadTokens: 32000, cacheWriteTokens: 0, costUsd: 0.064, credits: 0 },
      },
      { seq: 18, ts: 1732067918000, kind: 'turn_end', model: 'claude-sonnet-4-6', stopReason: 'end_turn' },
      {
        seq: 19,
        ts: 1732068000000,
        kind: 'user_message',
        text: '不错，这个多轮收起之后的节奏清晰多了，帮我提交到新分支。',
      },
      {
        seq: 20,
        ts: 1732068002000,
        kind: 'assistant_text',
        id: 'ma-3',
        text: '好的，我会把本次改动连同回归测试用例一起整理后提交到 `feat/chat-ui-refresh` 分支。',
      },
      {
        seq: 21,
        ts: 1732068002500,
        kind: 'usage',
        usage: { inputTokens: 8100, outputTokens: 160, cacheReadTokens: 34000, cacheWriteTokens: 0, costUsd: 0.019, credits: 0 },
      },
      { seq: 22, ts: 1732068003000, kind: 'turn_end', model: 'claude-sonnet-4-6', stopReason: 'end_turn' },
    ],
    info: {
      id: 'sess-multi',
      title: '多轮长对话（默认收起态）',
      cwd: 'D:\\agent-build\\yi-console',
      tool: 'claude',
      mode: 'structured',
      status: 'idle',
      model: 'claude-sonnet-4-6',
      permissionMode: 'acceptEdits',
      contextPercent: 46,
      createdAt: 1732067800000,
      updatedAt: 1732068003000,
      pendingPermissions: 0,
      usage: { inputTokens: 28700, outputTokens: 1370, cacheReadTokens: 78000, cacheWriteTokens: 0, costUsd: 0.111, credits: 0 },
    },
  },
};

function Fixture() {
  useContextMenu();
  const [sceneName, setSceneName] = useState<string>('default');
  const scene = SCENARIOS[sceneName] ?? SCENARIOS.default;
  const [events, setEvents] = useState<HubEvent[]>(scene.events);
  const [info, setInfo] = useState<SessionInfo>(scene.info);
  const [live, setLive] = useState<Map<string, string>>(new Map());
  const [liveThinking, setLiveThinking] = useState<Map<string, string>>(new Map());
  const [showPendingPerm, setShowPendingPerm] = useState(false);

  const loadScenario = (name: string) => {
    const sc = SCENARIOS[name] ?? SCENARIOS.default;
    setSceneName(name);
    setEvents(sc.events);
    setInfo(sc.info);
    setLive(new Map(Object.entries(sc.live ?? {})));
    setLiveThinking(new Map(Object.entries(sc.liveThinking ?? {})));
    setShowPendingPerm(!!sc.pendingPerm);
  };

  Object.assign(window, {
    setEvents,
    setInfo,
    setChatStyle,
    setTheme,
    setLangPref,
    loadScenario,
  });

  const pendingPermEv = showPendingPerm
    ? events.find((e): e is HubEvent & { kind: 'permission_request' } => e.kind === 'permission_request')
    : undefined;

  return (
    <div className="app" style={{ height: '100vh', display: 'grid', gridTemplateColumns: '220px minmax(0, 1fr)' }}>
      <aside className="sidebar hide-mobile" style={{ width: 220, padding: '14px 10px', display: 'flex', flexDirection: 'column', justifyContent: 'flex-start', gap: 4 }}>
        <div className="host-name" style={{ padding: '4px 10px 10px' }}>Yi Console</div>
        <div className="nav-item">总览</div>
        <div className="project-name" style={{ padding: '10px 10px 4px' }}>yi-console</div>
        <div className="session-row active" style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '6px 10px', borderRadius: 8, background: 'var(--side-active)' }}>
          <ToolBadge tool="claude" size={16} />
          <span className="session-title truncate">{info.title}</span>
        </div>
        <div className="session-row" style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '6px 10px', opacity: 0.72 }}>
          <ToolBadge tool="codex" size={16} />
          <span className="session-title truncate">修复 MCP 环境变量传递</span>
        </div>
        <div className="session-row" style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '6px 10px', opacity: 0.65 }}>
          <ToolBadge tool="cursor" size={16} />
          <span className="session-title truncate">Android 返回键交互回归</span>
        </div>
      </aside>
      <main className="main" style={{ margin: '8px 8px 8px 0', minHeight: 0, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
        <div className="session-view" style={{ flex: 1, minHeight: 0, display: 'flex', flexDirection: 'column' }}>
          <header className="session-head">
            <ToolBadge tool="claude" size={20} />
            <div className="session-meta">
              <div className="session-name truncate">{info.title}</div>
              <div className="muted small truncate">Claude Code · 默认账号 · ~/yi-console · 上下文 {info.contextPercent ?? 38}%</div>
            </div>
            <button className="btn small hide-mobile" type="button">
              <Icon name="chart" size={14} />
              <span>${info.usage.costUsd.toFixed(3)}</span>
            </button>
            <button className="icon-btn" type="button" title="查看变更">
              <Icon name="diff" size={17} />
            </button>
            <button className="icon-btn" type="button" title="工作区文件">
              <Icon name="folder" size={17} />
            </button>
            <button className="icon-btn hide-mobile" type="button" title="终端">
              <Icon name="terminal" size={17} />
            </button>
            <button className="icon-btn" type="button" title="更多">
              <Icon name="more" size={18} />
            </button>
          </header>
          <div className="chat-stage" style={{ flex: 1, minHeight: 0, display: 'flex', flexDirection: 'column' }}>
            <ChatView
              key={sceneName}
              events={events}
              live={live}
              liveThinking={liveThinking}
              info={info}
              hostId="local"
              pendingOutside={showPendingPerm}
              onAnswer={() => {}}
            />
          </div>
          <div className="composer-wrap">
            {pendingPermEv && (
              <div className="pending-perms">
                <PermissionCard
                  it={{
                    k: 'perm',
                    key: `e${pendingPermEv.seq}`,
                    requestId: pendingPermEv.requestId,
                    title: pendingPermEv.title,
                    toolName: pendingPermEv.toolName,
                    input: pendingPermEv.input,
                    options: pendingPermEv.options,
                  }}
                  onAnswer={() => {}}
                  compact
                />
              </div>
            )}
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
      <Overlays />
    </div>
  );
}

setLangPref('zh');
setChatStyle('modern');
createRoot(document.getElementById('root')!).render(<Fixture />);
