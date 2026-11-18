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
  '宽表格自适应与全屏查看测试：',
  '',
  '| 层级 | 代表标准 / 规范 | 维护方 / 生态 | 标准化了什么 | 局限 / 未覆盖部分 |',
  '| :--- | :--- | :--- | :--- | :--- |',
  '| 1. Markdown 文本级标准 | • Agent Skills (`SKILL.md`)<br>• `AGENTS.md`<br>• Agent SOP (`.sop.md`)<br>• GitHub Agentic Workflows (`.md`) | • AAIF（Linux 基金会，2026-10 接管 `agentskills.io`）<br>• AWS (`strands-agents/agent-sop`)<br>• GitHub Next (`gh-aw`) | • `SKILL.md` 已成事实统一标准（Claude Code / Codex / Cursor / Copilot 全支持），用目录打包指令与脚本。<br>• `.sop.md` 用 `Overview / Parameters / Steps` + **RFC 2119 (`MUST / SHOULD / MUST NOT`)** 规范步骤，还能直接编译成 `SKILL.md`。 | 偏文档约定与单步约束，标准本身不包含“状态机持久化、人工暂停卡片、失败打回重跑”。 |',
  '| 2. AI 原生声明式规范 | • Open Agent Spec (Agent Spec)<br>• OSSA | Oracle 等开源联盟（类似 AI 工作流界的 ONNX） | 纯 YAML 声明任务链（`depends_on`）、模型、MCP 工具与数据流，试图让一份配置跨 LangGraph / CrewAI 等框架运行。 | 主要停留在 Python/后端框架互操作圈，在 Coding Agent 工具生态里普及度远不如 `SKILL.md`。 |',
  '| 3. 协议层交互原语标准 | • MCP Elicitation<br>• A2A (Agent2Agent) | • Anthropic / AAIF 生态 | • MCP Elicitation 把“工具执行中途停下来向人要参数/要确认”标准化了（返回 JSON Schema 表单，用户答复 `accept / decline / cancel` 后继续）。 | 它们标准化的只是**“单次人机弹窗”**和**“Agent 间通信握手”**，不是整条多步 SOP 的编排语法。 |',
  '| 4. 传统企业 / 云原生工作流标准 | • BPMN 2.0 / DMN (ISO/IEC 19510)<br>• CNCF Serverless Workflow | Camunda / Flowable / CNCF | 极其严谨的网关、并发、计时器、人工审批节点与合规审计图。 | 太重，不适合个人/研发团队用自然语言对话生成或快速迭代，多见于金融/医疗强合规后台。 |',
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
