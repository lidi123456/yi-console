// Types mirroring crates/proto

/** files: no process, the session page is just the workspace file panel */
export type ToolKind = 'claude' | 'codex' | 'cursor' | 'kiro' | 'qoder' | 'codebuddy' | 'antigravity' | 'shell' | 'files';
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
  lastProcessErrorAt?: number;
  lastSeq: number;
  pendingPermissions: number;
  usage: Usage;
  model?: string | null;
  command?: string | null;
  /** Percentage of the context window used */
  contextPercent?: number | null;
  modelConfigOptions?: ModelConfigOption[];
  modelConfigPending?: boolean;
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
  /** True when this child session (`parentId` set) is a hidden side chat */
  sideChat?: boolean;
  /** Number of user messages already in the tool session when switching from chat to terminal (switching back only imports the ones after) */
  syncedUsers?: number;
  /** The tool hit its usage limit; "continue" is sent automatically at `at` (chat mode, Claude / Codex) */
  autoResume?: AutoResume;
  /** Usage limit hits in a row since the last normal turn end */
  limitHits?: number;
  computerUse?: boolean;
  /** The AI browser (`yi_browser`) is on */
  browserUse?: boolean;
  /** The oldest approval / question still waiting (overview board) */
  pending?: PendingBrief;
  /** The current or last chat turn (chat mode only) */
  turn?: TurnInfo;
  /** Set on a Bot's own session and on its tasks (see `bots.ts`) */
  bot?: BotLink;
  /** Selected account profile id and display label for AI sessions */
  accountId?: string | null;
  accountLabel?: string | null;
  /** MCP servers served by a desktop client (the Bot tools) */
  clientMcp?: { name: string; clientId: string }[];
  appendSystemPrompt?: string;
  /** Per-turn hidden git checkpoints captured around structured turns */
  checkpoints?: TurnCheckpoint[];
  /** Metadata for a read-only cross-tool AI code review side chat */
  review?: ReviewSessionMeta | null;
  /** Managed Git worktree bound to this session */
  worktree?: SessionWorktree | null;
}

export interface ReviewSessionMeta {
  scope: 'uncommitted' | 'branch' | 'commits' | 'range' | string;
  uncommittedFilter?: 'all' | 'tracked' | 'staged' | string;
  base?: string;
  target?: string;
  includeUncommitted?: boolean;
  mergeBase?: string;
  baseRev?: string;
  endRev?: string;
  commits?: string[];
  contiguous?: boolean;
  rangeExpr?: string;
  scopeLabel?: string;
  includedFiles?: string[];
  excludedFiles?: string[];
  totalFilesCount?: number;
}

export interface SessionWorktree {
  repoRoot: string;
  branch: string;
  baseBranch?: string | null;
  baseCommit?: string | null;
}

export interface TurnCheckpoint {
  turn: number;
  userSeq: number;
  turnEndSeq?: number | null;
  baseRef: string;
  endRef?: string | null;
  filesChanged: number;
  added: number;
  deleted: number;
  paths?: string[];
}

export interface BotLink {
  botId: string;
  botName: string;
  /** bot / task / assistant */
  role: string;
  taskId?: string;
  /** The desktop app running the Bot */
  clientId: string;
}

/** Ordinary top-level host session: neither a bottom-panel terminal (`parentId`) nor a Bot / Bot-task session (`bot`) */
export function isHostSession(s: SessionInfo): boolean {
  return !s.parentId && !s.bot;
}

/** Main repository directory for grouping a session (resolves a bound worktree back to its main repository) */
export function sessionProjectDir(s: SessionInfo): string {
  return s.worktree?.repoRoot || s.cwd;
}

/** Normalize a local directory path for case/separator-insensitive matching across platforms */
export function normDirPath(p: string): string {
  const s = p.trim().replace(/\\/g, '/').replace(/\/+$/, '');
  return /^[A-Za-z]:/.test(s) ? s.toLowerCase() : s;
}

export interface PendingBrief {
  requestId: string;
  toolName: string;
  title: string;
  /** The concrete command or file path, truncated */
  detail?: string;
  detailKind?: 'command' | 'file' | 'other' | string;
  at: number;
  /** Asks questions: answered on the session page */
  question?: boolean;
  options?: PermissionOption[];
}

export interface TurnInfo {
  startedAt: number;
  endedAt?: number;
  /** Distinct files the tool reported editing in this turn */
  filesChanged?: number;
  /** Latest tool call of a running turn (broadcast at most every 5 s) */
  activity?: { name: string; title: string; at: number };
  /** The last approval the user rejected in this turn (command / path / title) */
  rejected?: string;
}

/** git.status */
export interface GitStatus {
  branch: string;
  ahead: number;
  behind: number;
  changed: number;
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
  /** local_bash, local_agent, workflow, timer, etc. */
  kind: string;
  description: string;
  startedAt: number;
  /** Path of the output file on the host */
  outputFile?: string;
  /** True when the tool provides no way to stop this task from outside */
  unstoppable?: boolean;
}

export interface QueuedMessage {
  id: string;
  text: string;
  images: number;
}

export interface UserMessageSummary {
  seq: number;
  ts: number;
  text: string;
  images?: number;
  origin?: string;
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
  category?: string;
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
  accountId?: string | null;
  accountLabel?: string | null;
  authKind?: AccountAuthKind | null;
  isolation?: AccountIsolation | null;
  stale?: boolean;
  needsActivationToProbe?: boolean;
  account?: { email?: string | null; plan?: string | null };
  windows: QuotaWindow[];
  note?: string | null;
  fetchedAt: number | null;
}

export type AccountAuthKind = 'oauth' | 'apiKey';
export type AccountIsolation = 'isolated' | 'hostActive';
export type AccountState = 'ready' | 'needsLogin' | 'limited' | 'error';

export interface AccountIdentity {
  email?: string | null;
  plan?: string | null;
  workspace?: string | null;
  accountKey?: string | null;
}

export interface ApiKeyConfig {
  apiKey: string;
  authToken: string;
  baseUrl: string;
  defaultModel: string;
  providerName: string;
  extraEnv: Record<string, string>;
}

export interface AccountProfileView {
  id: string;
  tool: ToolKind;
  label: string;
  authKind: AccountAuthKind;
  isolation: AccountIsolation;
  active: boolean;
  inUseSessions: number;
  allowAutoFailover: boolean;
  priority: number;
  identity: AccountIdentity;
  apiConfig?: ApiKeyConfig | null;
  state: AccountState;
  stateDetail?: string | null;
  quota?: Quota | null;
  updatedAt: number;
  /** What the tool really authenticates with (Codex: a custom provider selected in config.toml) */
  actual?: ActualAuth | null;
}

/** Codex `model_provider` from config.toml when it isn't the built-in OpenAI one */
export interface ActualAuth {
  method: 'providerApiKey' | 'providerOpenaiAuth' | 'providerNoAuth' | string;
  provider: string;
  baseUrl?: string | null;
  envKey?: string | null;
  keySet: boolean;
}

export interface ToolNativeStatus {
  loggedIn: boolean;
  authMethod?: string | null;
  email?: string | null;
  plan?: string | null;
  envOverride?: string | null;
  matchedAccountId?: string | null;
  envApiKeyMatchedId?: string | null;
  hasLocalOauth?: boolean;
  detail?: string | null;
  actual?: ActualAuth | null;
}

export interface ToolAccountsSummary {
  tool: ToolKind;
  oauthIsolation: AccountIsolation;
  supportsOauth: boolean;
  supportsApiKey: boolean;
  supportsDeviceAuth: boolean;
  supportsInteractiveLogin: boolean;
  autoFailover: boolean;
  activeAccountId?: string | null;
  nativeStatus?: ToolNativeStatus | null;
  accounts: AccountProfileView[];
}

export interface AccountLoginUpdate {
  jobId: string;
  tool: ToolKind;
  accountId?: string | null;
  mode: string;
  running: boolean;
  ok?: boolean | null;
  url?: string | null;
  code?: string | null;
  prompt?: string | null;
  needsInput: boolean;
  output: string[];
  error?: string | null;
  /** When the device code expires (ms since the epoch) */
  expiresAt?: number | null;
}

export interface AccountQuotaEntry {
  accountId: string;
  label: string;
  authKind: AccountAuthKind;
  isolation: AccountIsolation;
  active: boolean;
  quota?: Quota | null;
  error?: string | null;
}

export interface ToolQuotasGroup {
  tool: ToolKind;
  oauthIsolation: AccountIsolation;
  activeAccountId?: string | null;
  accounts: AccountQuotaEntry[];
  nativeQuota?: Quota | null;
  error?: string | null;
}

export interface ToolsQuotasResponse {
  tools: ToolQuotasGroup[];
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
/** An AI browser (`yi_browser`) action; `url` is the page it ended on */
export type BrowserUseEvent = { kind: 'browser_use'; actionId: string; tool: string; status: string; message: string; durationMs: number; url?: string; images: ImageRef[] };
/** Personal long-term memory change made or proposed by the AI via `yi_memory` */
export type MemoryEvent = {
  kind: 'memory';
  action: 'saved' | 'deleted' | 'pending_save' | 'pending_delete' | string;
  scope: MemoryScope;
  projectKey?: string | null;
  projectName?: string | null;
  id: string;
  title: string;
  description?: string;
  mode?: MemoryMode;
  undoId?: string | null;
  pendingId?: string | null;
};

export type EventBody =
  | ComputerUseEvent
  | BrowserUseEvent
  | MemoryEvent
  /** `origin`: who sent it when not the person (`bot-engine`: the desktop app's Bot engine) */
  | { kind: 'user_message'; text: string; images?: ImageRef[]; origin?: string }
  | { kind: 'assistant_text'; id: string; text: string }
  /** Questions on the reply `id` that don't block the turn (Codex request_user_input_async); answered with an ordinary message */
  | { kind: 'async_question'; id: string; questions: Question[] }
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
  | { kind: 'plan'; entries: PlanEntry[]; sessionScoped?: boolean }
  | { kind: 'status'; status: SessionStatus; message?: string }
  | { kind: 'turn_end'; stopReason?: string | null; model?: string | null }
  | { kind: 'usage'; usage: Usage }
  | { kind: 'error'; message: string }
  | { kind: 'system'; text: string }
  /** /clear: all events before this one have been deleted on yi-agentd */
  | { kind: 'cleared' }
  /** Continue with another tool: `to` in the original session (the work went to sessionId), else in the new one */
  | { kind: 'handoff'; sessionId: string; tool: ToolKind; to: boolean }
  | {
      kind: 'account_switched';
      fromId?: string | null;
      fromLabel?: string | null;
      toId: string;
      toLabel: string;
      reason: string;
      contextCarried: string;
    }
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
  oauthIsolation?: AccountIsolation;
  accountCount?: number;
  activeAccountId?: string | null;
  activeAccountLabel?: string | null;
}

export const supportsAccountsTool = (tool: ToolKind): boolean => tool !== 'shell' && tool !== 'files';

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
  /** Where `git.worktreeAdd` puts worktrees (Bot tasks); `~/.yi-console/worktrees` if unset */
  worktreeRoot?: string;
  /** The AI browser (`yi_browser`) */
  browser?: BrowserSettings;
  /** Host personal long-term memory (`yi_memory`) */
  memory?: MemorySettings;
}

export type MemoryScope = 'global' | 'project';
export type MemoryMode = 'on-demand' | 'always';
export type MemoryWritePolicy = 'direct' | 'confirm' | 'readonly';

export interface MemorySettings {
  enabled: boolean;
  writePolicy: MemoryWritePolicy | string;
}

export interface MemoryMeta {
  id: string;
  scope: MemoryScope;
  projectKey?: string | null;
  title: string;
  description: string;
  mode: MemoryMode;
  tags?: string[];
  updatedAt: number;
  size: number;
}

export interface MemoryEntry extends MemoryMeta {
  content: string;
}

export interface MemoryProjectInfo {
  key: string;
  displayName: string;
  rootPath: string;
  gitCommonDir?: string | null;
  aliases?: string[];
  count: number;
}

export interface PendingMemory {
  pendingId: string;
  action: 'save' | 'delete' | string;
  scope: MemoryScope;
  projectKey?: string | null;
  projectName?: string | null;
  id: string;
  title: string;
  description: string;
  mode: MemoryMode;
  tags?: string[];
  content: string;
  sessionId?: string | null;
  createdAt: number;
}

export interface MemoryImportCandidate {
  source: string;
  path: string;
  suggestedScope: MemoryScope;
  suggestedId: string;
  title: string;
  description: string;
  suggestedMode: MemoryMode;
  content: string;
  alreadyExists: boolean;
}

export interface MemoryListResponse {
  resolvedProject?: MemoryProjectInfo | null;
  projects: MemoryProjectInfo[];
  global: MemoryMeta[];
  project: MemoryMeta[];
  byProject: Record<string, MemoryMeta[]>;
  pending: PendingMemory[];
  settings: MemorySettings;
}

export interface MemoryPreviewResponse {
  prompt: string;
  project?: MemoryProjectInfo | null;
  alwaysCount: number;
  indexCount: number;
  omittedCount: number;
  bytes: number;
}

export interface BrowserSettings {
  /** Chrome / Edge / Chromium to use instead of the one found on the host */
  executablePath?: string;
  /** One shared profile that keeps logins (one session at a time) */
  persistentProfile?: boolean;
  /** Hosts pages may reach (`example.com`, `*.example.com`); empty: no limit */
  allowedDomains?: string[];
  /** Action categories always refused (`eval`, `upload`, `download`, `network`, `storage`) */
  deniedActions?: string[];
  /** Accept invalid HTTPS certificates on every site (localhost always accepts them) */
  ignoreHttpsErrors?: boolean;
  /** Sharp (2x) live view; only offered for the local host */
  hiDpi?: boolean;
}

/** `browser.status`: where a watched AI browser is */
export interface BrowserViewStatus {
  sessionId: string;
  enabled: boolean;
  paused: boolean;
  running: boolean;
  headless: boolean;
  url: string;
  title: string;
  loading: boolean;
  tabs: { id: string; url: string; title: string; current: boolean }[];
  dialog?: { kind: string; message: string; defaultPrompt: string } | null;
  frame?: string | null;
  busy?: string | null;
  /** The AI's mouse in viewport CSS pixels; `clicks` grows with every press */
  pointer?: { x: number; y: number; clicks: number } | null;
  /** Device preset in force (the size menu or the AI's browser_set) */
  device?: string | null;
}

export const TOOL_LABEL: Record<ToolKind, string> = new Proxy(
  {
    claude: 'Claude Code',
    codex: 'Codex',
    cursor: 'Cursor',
    kiro: 'Kiro',
    qoder: 'Qoder',
    codebuddy: 'CodeBuddy',
    antigravity: 'Antigravity',
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
  /** Desktop: closing the window keeps the app in the tray while a Bot is enabled (on if unset) */
  closeToTray?: boolean;
}
