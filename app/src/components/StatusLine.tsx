import { useEffect, useMemo, useState } from 'react';
import { isEn, tr } from '../i18n';
import { hostStatus } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type DaemonSettings, type HubEvent, type Quota, type SessionInfo, type StatusLineSettings } from '../types';
import { basename, fmtTokens, shortPath, STATUS_LABEL } from '../util';

export const DEFAULT_STATUS_LINE: StatusLineSettings = {
  items: ['model', 'context', 'branch', 'tokens', 'cost'],
  template: '',
  command: '',
  useClaude: true,
};

const STATUS_FIELDS_ZH: [string, string, string][] = [
  ['model', '模型', '当前模型'],
  ['context', '上下文', '上下文窗口已用百分比'],
  ['tokens', 'Token', '本会话累计 token'],
  ['cost', '费用', '本会话累计费用 / credits'],
  ['lastTurn', '上一轮耗时', '最近一轮从发出到回复完的时间'],
  ['branch', 'Git 分支', '分支名、未提交改动数、领先/落后'],
  ['dir', '目录名', '工作目录的最后一级'],
  ['cwd', '完整目录', '工作目录（~ 开头）'],
  ['status', '状态', '运行中 / 空闲 / 等待审批…'],
  ['tool', '工具', 'Claude Code / Codex …'],
  ['quota', '账号额度', '最紧的额度窗口已用百分比（Cursor 没有）'],
];

const STATUS_FIELDS_EN: [string, string, string][] = [
  ['model', 'Model', 'Current model'],
  ['context', 'Context', 'Context window usage %'],
  ['tokens', 'Tokens', 'Total tokens in session'],
  ['cost', 'Cost', 'Total cost / credits in session'],
  ['lastTurn', 'Last Turn', 'Duration of the last turn'],
  ['branch', 'Git Branch', 'Branch name, uncommitted changes, ahead/behind'],
  ['dir', 'Folder Name', 'Last segment of working directory'],
  ['cwd', 'Full Path', 'Working directory (~ prefix)'],
  ['status', 'Status', 'Running / Idle / Waiting approval…'],
  ['tool', 'Tool', 'Claude Code / Codex …'],
  ['quota', 'Account Quota', 'Tightest quota window usage % (not available for Cursor)'],
];

/** Built-in fields: id, name shown in settings, description */
export function getStatusFields(): [string, string, string][] {
  return isEn() ? STATUS_FIELDS_EN : STATUS_FIELDS_ZH;
}

export const STATUS_FIELDS = STATUS_FIELDS_ZH;

/** Variables usable in the template */
export const TEMPLATE_VARS = ['model', 'context', 'tokens', 'cost', 'lastTurn', 'branch', 'dirty', 'changed', 'ahead', 'behind', 'dir', 'cwd', 'status', 'tool', 'title', 'quota', 'command'];

const GIT_KEYS = ['branch', 'dirty', 'changed', 'ahead', 'behind'];

// ---------- Host settings cache: fetched once per host, invalidated when the settings page saves ----------

const settingsCache = new Map<string, Promise<StatusLineSettings>>();
const listeners = new Set<() => void>();

function hostStatusLine(hostId: string) {
  let p = settingsCache.get(hostId);
  if (!p) {
    p = rpc<DaemonSettings>(hostId, 'settings.get')
      .then((s) => ({ ...DEFAULT_STATUS_LINE, ...(s.statusLine ?? {}) }))
      .catch(() => DEFAULT_STATUS_LINE);
    settingsCache.set(hostId, p);
  }
  return p;
}

export function invalidateStatusLine(hostId: string) {
  settingsCache.delete(hostId);
  for (const f of listeners) f();
}

// ---------- ANSI colors ----------

const ANSI_COLORS = ['#3b3f45', '#d1453b', '#1f9d55', '#b7791f', '#2f6fdb', '#a347ba', '#1b9aa8', '#8b929d'];
const ANSI_BRIGHT = ['#6b7280', '#ef5f55', '#2fbf71', '#d69e2e', '#4c8dff', '#c26ad6', '#2bb8c6', '#c9ced6'];

function xterm256(n: number): string {
  if (n < 8) return ANSI_COLORS[n];
  if (n < 16) return ANSI_BRIGHT[n - 8];
  if (n >= 232) {
    const v = 8 + (n - 232) * 10;
    return `rgb(${v},${v},${v})`;
  }
  const i = n - 16;
  const c = (x: number) => (x === 0 ? 0 : 55 + x * 40);
  return `rgb(${c(Math.floor(i / 36))},${c(Math.floor(i / 6) % 6)},${c(i % 6)})`;
}

interface Span {
  text: string;
  style: React.CSSProperties;
}

/** Split a line with ANSI SGR escapes into styled segments; other escapes (cursor, OSC hyperlinks, etc.) are dropped */
export function parseAnsi(s: string): Span[] {
  const out: Span[] = [];
  let style: React.CSSProperties = {};
  // OSC (e.g. 8 for hyperlinks): ESC ] ... (BEL | ESC \)
  s = s.replace(/\x1b\][^\x07\x1b]*(\x07|\x1b\\)/g, '');
  const re = /\x1b\[([0-9;]*)([A-Za-z])/g;
  let last = 0;
  let m: RegExpExecArray | null;
  const push = (t: string) => t && out.push({ text: t, style });
  while ((m = re.exec(s))) {
    push(s.slice(last, m.index));
    last = re.lastIndex;
    if (m[2] !== 'm') continue;
    const codes = (m[1] || '0').split(';').map(Number);
    const st: React.CSSProperties = { ...style };
    for (let i = 0; i < codes.length; i++) {
      const c = codes[i];
      if (c === 0) Object.keys(st).forEach((k) => delete (st as Record<string, unknown>)[k]);
      else if (c === 1) st.fontWeight = 600;
      else if (c === 2) st.opacity = 0.65;
      else if (c === 3) st.fontStyle = 'italic';
      else if (c === 4) st.textDecoration = 'underline';
      else if (c === 22) {
        delete st.fontWeight;
        delete st.opacity;
      } else if (c === 23) delete st.fontStyle;
      else if (c === 24) delete st.textDecoration;
      else if (c >= 30 && c <= 37) st.color = ANSI_COLORS[c - 30];
      else if (c >= 90 && c <= 97) st.color = ANSI_BRIGHT[c - 90];
      else if (c === 39) delete st.color;
      else if (c >= 40 && c <= 47) st.background = ANSI_COLORS[c - 40];
      else if (c >= 100 && c <= 107) st.background = ANSI_BRIGHT[c - 100];
      else if (c === 49) delete st.background;
      else if (c === 38 || c === 48) {
        const key = c === 38 ? 'color' : 'background';
        if (codes[i + 1] === 5) {
          st[key] = xterm256(codes[i + 2] ?? 0);
          i += 2;
        } else if (codes[i + 1] === 2) {
          st[key] = `rgb(${codes[i + 2] ?? 0},${codes[i + 3] ?? 0},${codes[i + 4] ?? 0})`;
          i += 4;
        }
      }
    }
    style = st;
  }
  push(s.slice(last));
  return out;
}

function Ansi({ text }: { text: string }) {
  const spans = useMemo(() => parseAnsi(text), [text]);
  return (
    <span className="sl-cmd">
      {spans.map((p, i) => (
        <span key={i} style={p.style}>
          {p.text}
        </span>
      ))}
    </span>
  );
}

// ---------- Values ----------

interface Git {
  branch: string;
  ahead: number;
  behind: number;
  changed: number;
}

function lastTurnMs(events: HubEvent[]): number | null {
  for (let i = events.length - 1; i >= 0; i--) {
    if (events[i].kind !== 'turn_end') continue;
    const end = events[i].ts;
    let start: number | null = null;
    for (let j = i - 1; j >= 0 && events[j].kind !== 'turn_end'; j--) if (events[j].kind === 'user_message') start = events[j].ts;
    return start == null ? null : end - start;
  }
  return null;
}

const fmtMs = (ms: number) => (ms < 60_000 ? `${(ms / 1000).toFixed(1)}s` : `${Math.floor(ms / 60_000)}m${String(Math.round((ms % 60_000) / 1000)).padStart(2, '0')}s`);

function values(info: SessionInfo, events: HubEvent[], git: Git | null, quota: Quota | null, cmd: string, home?: string) {
  const u = info.usage;
  const tokens = u.inputTokens + u.outputTokens + u.cacheReadTokens + u.cacheWriteTokens;
  const turn = lastTurnMs(events);
  const q = quota?.windows.filter((w) => w.usedPercent != null).sort((a, b) => (b.usedPercent ?? 0) - (a.usedPercent ?? 0))[0];
  const cost = [u.costUsd ? `$${u.costUsd.toFixed(u.costUsd < 1 ? 3 : 2)}` : '', u.credits ? `${u.credits.toFixed(2)} credits` : '']
    .filter(Boolean)
    .join(' ');
  const v: Record<string, string> = {
    model: info.model ?? '',
    context: info.contextPercent != null ? `${tr('上下文', 'Context')} ${Math.round(info.contextPercent)}%` : '',
    tokens: tokens ? `${fmtTokens(tokens)} tokens` : '',
    cost,
    lastTurn: turn != null ? `${tr('上一轮', 'Last')} ${fmtMs(turn)}` : '',
    branch: git?.branch ?? '',
    dirty: git && git.changed > 0 ? '*' : '',
    changed: git ? String(git.changed) : '',
    ahead: git?.ahead ? `↑${git.ahead}` : '',
    behind: git?.behind ? `↓${git.behind}` : '',
    dir: basename(info.cwd),
    cwd: shortPath(info.cwd, home),
    status: STATUS_LABEL[info.status] ?? info.status,
    tool: TOOL_LABEL[info.tool] ?? info.tool,
    title: info.title,
    quota: q ? `${q.label} ${Math.round(q.usedPercent ?? 0)}%` : '',
    command: cmd,
  };
  return v;
}

/** Display of built-in fields (the branch field includes change count and ahead/behind) */
function itemText(id: string, v: Record<string, string>, git: Git | null) {
  if (id === 'branch') {
    if (!git) return '';
    return [`⎇ ${git.branch}`, git.changed ? `±${git.changed}` : '', v.ahead, v.behind].filter(Boolean).join(' ');
  }
  return v[id] ?? '';
}

export function StatusLine({ hostId, info, events }: { hostId: string; info: SessionInfo; events: HubEvent[] }) {
  const [cfg, setCfg] = useState<StatusLineSettings | null>(null);
  const [git, setGit] = useState<Git | null>(null);
  const [cmd, setCmd] = useState<{ text: string; source: string } | null>(null);
  const [err, setErr] = useState('');
  const [quota, setQuota] = useState<Quota | null>(null);
  const [tick, setTick] = useState(0);
  const home = hostStatus(hostId).hello?.home;

  useEffect(() => {
    let alive = true;
    const load = () => hostStatusLine(hostId).then((c) => alive && setCfg(c));
    load();
    listeners.add(load);
    return () => {
      alive = false;
      listeners.delete(load);
    };
  }, [hostId]);

  const used = cfg ? (cfg.template.trim() ? TEMPLATE_VARS.filter((k) => cfg.template.includes(`{${k}}`)) : cfg.items) : [];
  const wantGit = used.some((k) => GIT_KEYS.includes(k));
  const wantQuota = used.includes('quota') && info.tool !== 'shell';
  const wantCmd = !!cfg && (!!cfg.command.trim() || (cfg.useClaude && info.tool === 'claude'));

  // Refresh every 20 s (while visible), and 1 s after the session changes (turn ends, usage changes)
  useEffect(() => {
    const t = setInterval(() => document.visibilityState === 'visible' && setTick((x) => x + 1), 20_000);
    return () => clearInterval(t);
  }, []);
  useEffect(() => {
    const t = setTimeout(() => setTick((x) => x + 1), 1000);
    return () => clearTimeout(t);
  }, [info.updatedAt, info.status]);

  useEffect(() => {
    if (!cfg || (!wantGit && !wantCmd)) return;
    let alive = true;
    rpc<{ git?: Git; command?: { text: string; source: string }; error?: string }>(hostId, 'session.statusLine', {
      sessionId: info.id,
      git: wantGit,
    })
      .then((r) => {
        if (!alive) return;
        setGit(r.git ?? null);
        setCmd(r.command ?? null);
        setErr(r.error ?? '');
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId, info.id, cfg, wantGit, wantCmd, tick]);

  useEffect(() => {
    if (!wantQuota) return setQuota(null);
    rpc<Quota>(hostId, 'tools.quota', { tool: info.tool })
      .then(setQuota)
      .catch(() => setQuota(null));
    // yi-agentd caches quotas for 60 s, so refreshing every 3rd tick here is enough
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId, info.tool, wantQuota, Math.floor(tick / 3)]);

  if (!cfg) return null;
  const v = values(info, events, git, quota, cmd?.text ?? '', home);
  let builtin = '';
  if (cfg.template.trim()) {
    builtin = cfg.template.replace(/\{(\w+)\}/g, (_, k: string) => v[k] ?? `{${k}}`);
  } else {
    builtin = cfg.items
      .map((id) => itemText(id, v, git))
      .filter(Boolean)
      .join('  ·  ');
  }
  // If the template references {command}, don't show the command output separately
  const showCmd = cmd?.text && !cfg.template.includes('{command}');
  if (!builtin.trim() && !showCmd && !err) return null;
  return (
    <div
      className="status-line"
      title={cmd?.source === 'claude' ? tr('右侧来自 ~/.claude/settings.json 的 statusLine 命令', 'Right side from statusLine command in ~/.claude/settings.json') : undefined}
    >
      {builtin.trim() && <span className="sl-builtin">{cfg.template.includes('{command}') ? <Ansi text={builtin} /> : builtin}</span>}
      {showCmd && <Ansi text={cmd!.text} />}
      {err && !showCmd && <span className="sl-err">{err}</span>}
    </div>
  );
}
