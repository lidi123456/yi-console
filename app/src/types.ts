// 与 crates/proto 对应的类型

export type ToolKind = 'claude' | 'codex' | 'cursor' | 'kiro' | 'qoder' | 'codebuddy' | 'shell';
export type SessionMode = 'structured' | 'terminal';
export type SessionStatus = 'starting' | 'idle' | 'running' | 'waiting-permission' | 'exited' | 'error';

export interface Usage {
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  costUsd: number;
  credits: number;
}

export interface SessionInfo {
  id: string;
  tool: ToolKind;
  mode: SessionMode;
  cwd: string;
  title: string;
  status: SessionStatus;
  externalId: string | null;
  createdAt: number;
  updatedAt: number;
  lastSeq: number;
  pendingPermissions: number;
  usage: Usage;
  model?: string | null;
  command?: string | null;
  /** 上下文窗口已用百分比 */
  contextPercent?: number | null;
  /** default / acceptEdits / plan / bypass */
  permissionMode?: string | null;
  /** 思考等级，空表示工具默认 */
  effort?: string | null;
  pinned?: boolean;
  /** 等这一轮结束再发的消息 */
  queued?: QueuedMessage[];
  /** 工具在后台跑着的任务（Claude 的后台 shell、子代理） */
  backgroundTasks?: BackgroundTask[];
  /** 会话页底部终端面板里开的终端：所属对话会话；不在侧边栏等列表里单独显示 */
  parentId?: string;
  /** 从对话切到终端时工具会话里已有的用户消息数（切回对话时只导入之后的） */
  syncedUsers?: number;
}

export interface BackgroundTask {
  id: string;
  /** local_bash、local_agent 等 */
  kind: string;
  description: string;
  startedAt: number;
  /** 输出文件在主机上的路径 */
  outputFile?: string;
}

export interface QueuedMessage {
  id: string;
  text: string;
  images: number;
}

export interface SlashCommand {
  name: string;
  description?: string;
  hint?: string;
}

export interface Snippet {
  name: string;
  text: string;
}

export interface SearchHit {
  sessionId: string;
  seq: number;
  ts: number;
  kind: 'user' | 'assistant';
  title: string;
  tool: ToolKind;
  cwd: string;
  snippet: string;
}

export interface GitFile {
  path: string;
  /** git status 代码（M、A、D…），未跟踪的新文件是 new */
  status: string;
  diff: string;
}

export interface ModelOption {
  id: string;
  name: string;
  description?: string;
  /** 别名实际对应的模型 */
  resolved?: string;
  /** 支持的思考等级 */
  efforts?: string[];
  defaultEffort?: string;
}

export interface ImageRef {
  id: string;
  mimeType: string;
}

export interface QuotaWindow {
  label: string;
  usedPercent: number | null;
  /** 毫秒时间戳或 ISO 字符串 */
  resetsAt?: number | string | null;
  detail?: string;
}

export interface Quota {
  tool: ToolKind;
  account?: { email?: string | null; plan?: string | null };
  windows: QuotaWindow[];
  note?: string | null;
  fetchedAt: number;
}

export interface FileDiff {
  path: string;
  oldText?: string;
  newText?: string;
  unified?: string;
}

export interface PermissionOption {
  id: string;
  label: string;
  kind: string;
}

/** AI 向用户提的问题，跟审批走同一条路：permission_request 带 questions */
export interface QuestionOption {
  id: string;
  label: string;
  description?: string;
}

export interface Question {
  id: string;
  header?: string;
  question: string;
  options: QuestionOption[];
  multiSelect?: boolean;
  /** 可以不选选项、自己填 */
  allowOther?: boolean;
  secret?: boolean;
}

export interface QuestionAnswer {
  questionId: string;
  selected: string[];
  text?: string;
}

export interface PlanEntry {
  content: string;
  status: string;
}

export type EventBody =
  | { kind: 'user_message'; text: string; images?: ImageRef[] }
  | { kind: 'assistant_text'; id: string; text: string }
  | { kind: 'assistant_delta'; id: string; delta: string }
  | { kind: 'thinking'; id: string; text: string }
  | { kind: 'thinking_delta'; id: string; delta: string }
  | { kind: 'tool_call'; id: string; name: string; title: string; input?: unknown; diffs?: FileDiff[] }
  | { kind: 'tool_result'; id: string; output: string; isError: boolean }
  | {
      kind: 'permission_request';
      requestId: string;
      toolName: string;
      title: string;
      input?: unknown;
      options: PermissionOption[];
      questions?: Question[];
    }
  | { kind: 'permission_resolved'; requestId: string; optionId: string; answers?: QuestionAnswer[] }
  | { kind: 'plan'; entries: PlanEntry[] }
  | { kind: 'status'; status: SessionStatus; message?: string }
  | { kind: 'turn_end'; stopReason?: string | null; model?: string | null }
  | { kind: 'usage'; usage: Usage }
  | { kind: 'error'; message: string }
  | { kind: 'system'; text: string }
  /** /clear：这条之前的事件都已在 yi-agentd 上删除 */
  | { kind: 'cleared' }
  | {
      kind: 'compact';
      phase: 'start' | 'done' | 'failed';
      trigger?: 'auto' | 'manual';
      preTokens?: number;
      postTokens?: number;
      durationMs?: number;
    };

export type HubEvent = EventBody & { seq: number; ts: number };

export interface ToolInfo {
  kind: ToolKind;
  available: boolean;
  path: string | null;
  version: string | null;
  structured: boolean;
}

export interface Hello {
  protocol: number;
  daemonVersion: string;
  hostname: string;
  os: string;
  arch: string;
  home: string;
  tools: ToolInfo[];
}

/** proxy：经已配对电脑的 yi-agentd 转发到那台电脑上配的主机 */
export type HostKind = 'local' | 'ssh' | 'relay' | 'proxy';

export interface SshTarget {
  host: string;
  port: number;
  user: string;
  auth: 'agent' | 'key' | 'password';
  keyPath: string;
  keyText: string;
  passphrase: string;
  password: string;
  jumpHostId: string;
}

export interface HostConfig {
  id: string;
  name: string;
  kind: HostKind;
  ssh?: SshTarget | null;
  pairing?: string | null;
  autoConnect: boolean;
  installService: boolean;
  remoteOs?: string | null;
  remoteArch?: string | null;
  /** 允许已配对的设备经本机访问这台主机 */
  relayShare?: boolean;
  viaHostId?: string | null;
  targetId?: string | null;
}

export interface HostStatus {
  status?: 'connecting' | 'connected' | 'disconnected' | 'error';
  error?: string;
  code?: string | null;
  hello?: Hello;
  versionMismatch?: boolean;
  clientVersion?: string;
}

export interface HistoryEntry {
  tool: ToolKind;
  id: string;
  cwd: string;
  title: string;
  updatedAt: number;
  messageCount?: number | null;
  readable: boolean;
}

export interface DirEntry {
  name: string;
  path: string;
  isDir: boolean;
  size: number;
  modified: number;
}

export interface StatusLineSettings {
  items: string[];
  template: string;
  command: string;
  useClaude: boolean;
}

export interface DaemonSettings {
  tools: Record<string, { command: string; extraArgs: string[] }>;
  statusLine: StatusLineSettings;
  snippets: Snippet[];
  /** 闲置多少分钟后结束工具进程，不填按 30，0 不释放 */
  idleReleaseMinutes?: number;
  /** 对话模式的工具进程最多同时开几个，不填按 5，0 不限 */
  maxLiveProcesses?: number;
  /** 终端会话放进 tmux，不填按开启 */
  terminalTmux?: boolean;
}

export const TOOL_LABEL: Record<ToolKind, string> = new Proxy(
  {
    claude: 'Claude Code',
    codex: 'Codex',
    cursor: 'Cursor',
    kiro: 'Kiro',
    qoder: 'Qoder',
    codebuddy: 'CodeBuddy',
    shell: '终端',
  },
  {
    get(target, prop: string) {
      if (prop === 'shell' && typeof document !== 'undefined' && document.documentElement.lang === 'en') {
        return 'Terminal';
      }
      return target[prop as ToolKind];
    },
  },
);

export interface NotifyPrefs {
  enabled: boolean;
  onTurnEnd: boolean;
  onPermission: boolean;
  /** away：窗口不在前台或看的不是这个会话时才通知；always：总是通知 */
  when: 'away' | 'always';
}

export interface ClientSettings {
  proxy: { mode: string; url: string };
  notify: NotifyPrefs;
}
