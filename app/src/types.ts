// Types mirroring crates/proto

/** files: no process, the session page is just the workspace file panel */
export type ToolKind = 'claude' | 'codex' | 'cursor' | 'kiro' | 'qoder' | 'codebuddy' | 'shell' | 'files';
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
  /** Percentage of the context window used */
  contextPercent?: number | null;
  modelConfigOptions?: ModelConfigOption[];
  /** default / acceptEdits / plan / bypass */
  permissionMode?: string | null;
  /** Thinking level; empty means the tool's default */
  effort?: string | null;
  pinned?: boolean;
  /** Messages to send after this turn ends (kept across daemon restarts) */
  queued?: QueuedMessage[];
  /** The queue is held, nothing goes out at turn end until resumed: restart / interrupt / user / handoff */
  queuePaused?: 'restart' | 'interrupt' | 'user' | 'handoff' | string | null;
  /** A chat turn was running when the daemon stopped and was cut off; `session.interrupted` continues or dismisses */
  restartInterrupted?: boolean;
  /** Tasks the tool runs in the background (Claude's background shells, subagents) */
  backgroundTasks?: BackgroundTask[];
  /** Terminal opened in the session page's bottom panel: the chat session it belongs to; not listed separately in the sidebar etc. */
  parentId?: string;
  /** Number of user messages already in the tool session when switching from chat to terminal (switching back only imports the ones after) */
  syncedUsers?: number;
  /** The tool hit its usage limit; "continue" is sent automatically at `at` (chat mode, Claude / Codex) */
  autoResume?: AutoResume;
  /** Usage limit hits in a row since the last normal turn end */
  limitHits?: number;
  computerUse?: boolean;
}

export interface AutoResume {
  /** When the continue message is sent (ms) */
  at: number;
  /** When the tool says the limit resets (ms) */
  resetsAt: number;
  /** five_hour / seven_day / … when the tool says */
  limit?: string;
}

export interface BackgroundTask {
  id: string;
  /** local_bash, local_agent, etc. */
  kind: string;
  description: string;
  startedAt: number;
  /** Path of the output file on the host */
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
  /** git status code (M, A, D…); untracked new files are new */
  status: string;
  diff: string;
}

export interface ModelConfigOption {
  id: string;
  name: string;
  description: string;
  currentValue: string;
  options: { value: string; name: string }[];
}

export interface ModelOption {
  id: string;
  name: string;
  description?: string;
  /** The model an alias actually maps to */
  resolved?: string;
  /** Supported thinking levels */
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
  /** Millisecond timestamp or ISO string */
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

/** A question the AI asks the user, sent the same way as approvals: permission_request carries questions */
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
  /** The user may skip the options and write their own answer */
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

export type ComputerUseEvent = { kind: 'computer_use'; actionId: string; tool: string; status: string; message: string; durationMs: number; images: ImageRef[] };

export type EventBody =
  | ComputerUseEvent
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
  /** /clear: all events before this one have been deleted on yi-agentd */
  | { kind: 'cleared' }
  /** Continue with another tool: `to` in the original session (the work went to sessionId), else in the new one */
  | { kind: 'handoff'; sessionId: string; tool: ToolKind; to: boolean }
  /** A turn cut off because the daemon stopped (recorded when it started again) */
  | { kind: 'interrupted'; reason: string }
  /** Usage limit: hit (from the adapter) / scheduled / skipped (reason off|unknown|far|repeated) /
   *  resumed (reason auto|now) / cancelled (reason user|manual|mode|off|busy) */
  | {
      kind: 'rate_limit';
      phase: 'hit' | 'scheduled' | 'skipped' | 'resumed' | 'cancelled';
      resetsAt?: number;
      limit?: string;
      resumeAt?: number;
      reason?: string;
    }
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

/** A tool upgrade on a host (`tools.update`); versions are what `--version` printed before and after */
export interface ToolJob {
  running: boolean;
  ok: boolean | null;
  error: string | null;
  from: string | null;
  to: string | null;
  startedAt: number;
  finishedAt: number | null;
  output: string[];
}

/** `tools.latest`, per tool: newest published version (npm tools only), install method, and whether it can be upgraded */
export interface ToolLatest {
  latest?: string;
  error?: string;
  via?: 'npm' | 'native';
  canUpdate: boolean;
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

/** proxy: forwarded through a paired computer's yi-agentd to a host configured on that computer */
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
  /** Allow paired devices to reach this host through this machine */
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

/** One commit from git.log (time is in seconds; refs come from %D, e.g. "HEAD -> main", "tag: v1") */
export interface CommitInfo {
  hash: string;
  short: string;
  parents: string[];
  author: string;
  time: number;
  refs: string[];
  subject: string;
}

/** A file changed by a commit, from git.show: status is the --name-status letter; diff is only filled for the requested paths */
export interface CommitFile {
  path: string;
  status: string;
  added?: number | null;
  deleted?: number | null;
  diff: string;
}

export interface StatusLineSettings {
  items: string[];
  template: string;
  command: string;
  useClaude: boolean;
}

export interface McpServerConfig {
  name: string;
  enabled: boolean;
  transport: 'stdio' | 'http';
  command: string;
  args: string[];
  env: Record<string, string>;
  url: string;
  headers: Record<string, string>;
  tools: ToolKind[];
}

export interface DaemonSettings {
  mcpServers?: McpServerConfig[];
  tools: Record<string, { command: string; extraArgs: string[] }>;
  statusLine: StatusLineSettings;
  snippets: Snippet[];
  /** Minutes idle before the tool process is ended; 30 if unset, 0 never releases */
  idleReleaseMinutes?: number;
  /** Max concurrent chat-mode tool processes; 5 if unset, 0 means unlimited */
  maxLiveProcesses?: number;
  /** Run terminal sessions in tmux; on if unset */
  terminalTmux?: boolean;
  /** Send "continue" automatically after a usage limit resets; on if unset */
  autoResume?: boolean;
  /** UI language last reported by a client (zh / en) */
  uiLang?: string;
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
    files: '文件',
  },
  {
    get(target, prop: string) {
      const en = typeof document !== 'undefined' && document.documentElement.lang === 'en';
      if (prop === 'shell' && en) return 'Terminal';
      if (prop === 'files' && en) return 'Files';
      return target[prop as ToolKind];
    },
  },
);

export interface NotifyPrefs {
  enabled: boolean;
  onTurnEnd: boolean;
  onPermission: boolean;
  /** away: notify only when the window isn't in front or another session is being viewed; always: always notify */
  when: 'away' | 'always';
}

export interface ClientSettings {
  proxy: { mode: string; url: string };
  notify: NotifyPrefs;
}
