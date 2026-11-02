import { createContext, memo, useContext, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { hydrateImages, type ImgCtx } from '../chatImages';
import { isEn, tr, useLang } from '../i18n';
import { rpc } from '../transport';
import type { FileDiff, HubEvent, ImageRef, PermissionOption, PlanEntry, Question, QuestionAnswer, SessionInfo, Usage } from '../types';
import { clock, fmtTokens, isMobile, lsGet, lsSet, renderMarkdown } from '../util';
import { writeClipboard } from '../clipboard';
import { toast, useStore } from '../store';
import { DiffView, diffLines } from './DiffView';
import { openImage } from './Lightbox';
import { imageMenu } from './ImageMenu';
import { Icon } from './Icon';
import { parseTextQuestions, QuestionCard, textAnswer } from './QuestionCard';
import { PLAN_TOOLS, PlanTracker, viewEntries, type PlanState } from '../plan';

type Item =
  | { k: 'user'; key: string; text: string; ts: number; images: ImageRef[] }
  | { k: 'text'; key: string; ts: number; text: string }
  | { k: 'thinking'; key: string; ts: number; text: string }
  | {
      k: 'tool';
      key: string;
      ts: number;
      name: string;
      title: string;
      input: unknown;
      diffs: FileDiff[];
      result?: { output: string; isError: boolean };
    }
  | {
      k: 'perm';
      key: string;
      requestId: string;
      title: string;
      toolName: string;
      input: unknown;
      options: PermissionOption[];
      resolved?: string;
      questions?: Question[];
      answers?: QuestionAnswer[];
    }
  | { k: 'plan'; key: string; plan: PlanState }
  | { k: 'note'; key: string; text: string; tone: 'err' | 'muted' }
  | { k: 'bgdone'; key: string; status: 'done' | 'failed' | 'stopped' | 'other'; label: string; what: string }
  | { k: 'compact'; key: string; ev: HubEvent & { kind: 'compact' } }
  | {
      k: 'turn';
      key: string;
      ms: number | null;
      ts: number;
      usage: Usage | null;
      tools: number;
      model?: string | null;
      reason?: string;
      text: string;
    };

/** hideCompactCmd：工具会报压缩进度（Claude、Codex）时，不再显示用户发的「/compact」气泡 */
function buildItems(events: HubEvent[], hideCompactCmd = false): Item[] {
  const items: Item[] = [];
  const tools = new Map<string, Item & { k: 'tool' }>();
  const perms = new Map<string, Item & { k: 'perm' }>();
  let planIdx = -1;
  const planner = new PlanTracker();
  // 当前这一轮：从上一轮结束后的第一条用户消息算起
  let turnStart: number | null = null;
  let turnUsage: Usage | null = null;
  let turnTools = 0;
  let turnTexts: string[] = [];
  let exitNote: Extract<Item, { k: 'note' }> | null = null;
  for (const e of events) {
    const key = `e${e.seq}`;
    // 任务清单只保留最新的一份，位置跟着最后一次更新走；清单被清空时留着上一份
    if (planner.feed(e) && planner.plan) {
      if (planIdx >= 0) items.splice(planIdx, 1);
      items.push({ k: 'plan', key: `p${e.seq}`, plan: planner.plan });
      planIdx = items.length - 1;
    }
    switch (e.kind) {
      case 'user_message':
        if (!(hideCompactCmd && e.text.trim() === '/compact')) {
          items.push({ k: 'user', key, text: e.text, ts: e.ts, images: e.images ?? [] });
        }
        turnStart ??= e.ts;
        break;
      case 'assistant_text':
        items.push({ k: 'text', key, ts: e.ts, text: e.text });
        turnTexts.push(e.text);
        break;
      case 'thinking':
        items.push({ k: 'thinking', key, ts: e.ts, text: e.text });
        break;
      case 'tool_call': {
        const old = tools.get(e.id);
        if (old) {
          old.title = e.title || old.title;
          old.input = e.input ?? old.input;
          if (e.diffs?.length) old.diffs = e.diffs;
        } else {
          const it: Item & { k: 'tool' } = {
            k: 'tool',
            key,
            ts: e.ts,
            name: e.name,
            title: e.title,
            input: e.input,
            diffs: e.diffs ?? [],
          };
          tools.set(e.id, it);
          items.push(it);
          turnTools++;
        }
        break;
      }
      case 'tool_result': {
        const t = tools.get(e.id);
        if (t) t.result = { output: e.output, isError: e.isError };
        break;
      }
      case 'permission_request': {
        const it: Item & { k: 'perm' } = {
          k: 'perm',
          key,
          requestId: e.requestId,
          title: e.title,
          toolName: e.toolName,
          input: e.input,
          options: e.options,
          questions: e.questions,
        };
        perms.set(e.requestId, it);
        items.push(it);
        break;
      }
      case 'permission_resolved': {
        const p = perms.get(e.requestId);
        if (p) {
          p.resolved = e.optionId;
          p.answers = e.answers;
        }
        break;
      }
      case 'error':
        items.push({ k: 'note', key, text: e.message, tone: 'err' });
        break;
      case 'system': {
        // yi-agentd 把 Claude 的 task_notification 记成「后台任务完成：<命令>」，单独画成一行
        const bg = /^后台任务(完成|失败|已停止|\s*\S+?)：([\s\S]*)$/.exec(e.text.trim());
        if (bg) {
          const s = bg[1] === '完成' ? 'done' : bg[1] === '失败' ? 'failed' : bg[1] === '已停止' ? 'stopped' : 'other';
          const label =
            s === 'done' ? tr('后台任务完成', 'Background task done')
            : s === 'failed' ? tr('后台任务失败', 'Background task failed')
            : s === 'stopped' ? tr('后台任务已停止', 'Background task stopped')
            : tr(`后台任务 ${bg[1].trim()}`, `Background task ${bg[1].trim()}`);
          items.push({ k: 'bgdone', key, status: s, label, what: bg[2].replace(/\s+/g, ' ').trim() });
        } else if (e.text.trim()) items.push({ k: 'note', key, text: e.text, tone: 'muted' });
        break;
      }
      case 'compact': {
        // 结束时替换掉「正在压缩」那一行，原地变成结果
        let last: Item | undefined;
        let lastIdx = -1;
        for (let i = items.length - 1; i >= 0 && !last; i--) if (items[i].k === 'compact') { last = items[i]; lastIdx = i; }
        // 旧版 yi-agentd 会把 Claude 压缩期间重复发的 compacting 都记成 start：同一轮里还没结束的就不再加一行
        if (e.phase === 'start' && last?.k === 'compact' && last.ev.phase === 'start' && !items.slice(lastIdx + 1).some((x) => x.k === 'turn' || x.k === 'user')) break;
        if (e.phase !== 'start' && last?.k === 'compact' && last.ev.phase === 'start') {
          // 工具没报耗时（Codex）就用开始到结束的时间差
          last.ev = e.durationMs == null ? { ...e, durationMs: e.ts - last.ev.ts } : e;
        }
        else items.push({ k: 'compact', key, ev: e });
        break;
      }
      case 'status':
        // 进程异常退出那条错误：之后进程又拉起来了（恢复会话、发消息自动恢复），改成灰色的「已恢复」，不再标红
        if (e.status === 'error') {
          const last = items[items.length - 1];
          exitNote = last?.k === 'note' && last.tone === 'err' ? last : null;
        } else if (exitNote && e.status !== 'exited') {
          exitNote.tone = 'muted';
          exitNote.text = `${exitNote.text.split('\n')[0]}${tr('，已恢复', ' (resumed)')}`;
          exitNote = null;
        }
        if (e.status === 'exited') items.push({ k: 'note', key, text: tr('会话进程已结束，发送消息会自动恢复', 'Session process ended. Sending a message will resume it.'), tone: 'muted' });
        break;
      case 'usage': {
        const u = e.usage;
        const t = (turnUsage ??= { inputTokens: 0, outputTokens: 0, cacheReadTokens: 0, cacheWriteTokens: 0, costUsd: 0, credits: 0 });
        t.inputTokens += u.inputTokens;
        t.outputTokens += u.outputTokens;
        t.cacheReadTokens += u.cacheReadTokens;
        t.cacheWriteTokens += u.cacheWriteTokens;
        t.costUsd += u.costUsd;
        t.credits += u.credits;
        break;
      }
      case 'turn_end':
        items.push({
          k: 'turn',
          key,
          ms: turnStart != null ? e.ts - turnStart : null,
          ts: e.ts,
          usage: turnUsage,
          tools: turnTools,
          model: e.model,
          reason: e.stopReason && !NORMAL_END.includes(e.stopReason) ? reasonText(e.stopReason) : undefined,
          text: turnTexts.join('\n\n'),
        });
        turnStart = null;
        turnUsage = null;
        turnTools = 0;
        turnTexts = [];
        break;
    }
  }
  return items;
}

const NORMAL_END = ['end_turn', 'completed', 'stop_sequence'];

/** 回复的显示风格：简约（过程折叠成一组，默认）、Claude Code（左侧圆点时间线）。只存在这台设备上 */
export type ChatStyle = 'modern' | 'claude';
const STYLE_EVENT = 'yi-console:chat-style';

export function chatStyle(): ChatStyle {
  return lsGet<ChatStyle>('chat-style', 'modern') === 'claude' ? 'claude' : 'modern';
}

export function setChatStyle(v: ChatStyle) {
  lsSet('chat-style', v);
  window.dispatchEvent(new Event(STYLE_EVENT));
}

export function useChatStyle() {
  const [v, setV] = useState(chatStyle);
  useEffect(() => {
    const f = () => setV(chatStyle());
    window.addEventListener(STYLE_EVENT, f);
    return () => window.removeEventListener(STYLE_EVENT, f);
  }, []);
  return v;
}

function fmtMs(ms: number) {
  if (ms < 60_000) return isEn() ? `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)}s` : `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)} 秒`;
  const m = Math.floor(ms / 60_000);
  const s = String(Math.round((ms % 60_000) / 1000)).padStart(2, '0');
  return isEn() ? `${m}m ${s}s` : `${m} 分 ${s} 秒`;
}

/** 每轮回复下面的一行：耗时、token、费用、工具调用次数、模型，外加复制整轮回复 */
function TurnFooter({ it }: { it: Item & { k: 'turn' } }) {
  const u = it.usage;
  const parts: string[] = [];
  if (it.ms != null && it.ms >= 0) parts.push(fmtMs(it.ms));
  if (u) {
    if (u.inputTokens || u.cacheReadTokens || u.cacheWriteTokens)
      parts.push(`${tr('输入', 'In')} ${fmtTokens(u.inputTokens + u.cacheReadTokens + u.cacheWriteTokens)}`);
    if (u.outputTokens) parts.push(`${tr('输出', 'Out')} ${fmtTokens(u.outputTokens)}`);
    if (u.costUsd) parts.push(`$${u.costUsd.toFixed(u.costUsd < 1 ? 3 : 2)}`);
    if (u.credits) parts.push(`${u.credits.toFixed(2)} credits`);
  }
  if (it.tools) parts.push(tr(`${it.tools} 次工具调用`, `${it.tools} tool call(s)`));
  if (it.model) parts.push(it.model);
  const tip = u
    ? tr(
        `输入 ${u.inputTokens}（其中缓存读取 ${u.cacheReadTokens}、缓存写入 ${u.cacheWriteTokens} 另计）\n输出 ${u.outputTokens}`,
        `Input ${u.inputTokens} (cache read ${u.cacheReadTokens}, cache write ${u.cacheWriteTokens} counted separately)\nOutput ${u.outputTokens}`,
      )
    : '';
  return (
    <div className="turn-foot" title={tip}>
      {it.reason && <span className="attn-text">{it.reason}</span>}
      <span>{parts.join(' · ')}</span>
      <span className="turn-time">{clock(it.ts)}</span>
      {it.text && (
        <button
          className="turn-copy"
          title={tr('复制这一轮的回复', 'Copy this reply')}
          onClick={async () => toast((await writeClipboard(it.text)) ? tr('已复制', 'Copied') : tr('复制失败', 'Copy failed'))}
        >
          <Icon name="copy" size={13} />
        </button>
      )}
    </div>
  );
}

function reasonText(r: string) {
  const map: Record<string, string> = isEn()
    ? { interrupted: 'Interrupted', cancelled: 'Cancelled', max_tokens: 'Output limit reached', error: 'Error', failed: 'Failed', refusal: 'Refused' }
    : { interrupted: '已中断', cancelled: '已取消', max_tokens: '达到输出上限', error: '出错', failed: '失败', refusal: '模型拒绝' };
  return map[r] ?? r;
}

/** 回复里的图片按哪台主机、哪个目录取（chatImages.ts） */
const ImgContext = createContext<ImgCtx | null>(null);

const Markdown = memo(function Markdown({ text }: { text: string }) {
  const html = useMemo(() => renderMarkdown(text), [text]);
  const ref = useRef<HTMLDivElement>(null);
  const ctx = useContext(ImgContext);
  useLayoutEffect(() => {
    if (ref.current) hydrateImages(ref.current, ctx);
  }, [html, ctx]);
  return <div ref={ref} className="md" dangerouslySetInnerHTML={{ __html: html }} />;
});

/** 用户在这个元素里选中文字（或正按着鼠标拖选） */
function selectingIn(el: HTMLElement | null, mouseDown: boolean) {
  if (!el) return false;
  if (mouseDown) return true;
  const s = window.getSelection();
  return !!s && !s.isCollapsed && !!s.anchorNode && el.contains(s.anchorNode);
}

/** 流式输出的正文：用户正在选中文字时先不刷新，不然每来一段内容整块重绘，选区就没了 */
function LiveMarkdown({ text }: { text: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const latest = useRef(text);
  const down = useRef(false);
  const [shown, setShown] = useState(text);
  latest.current = text;
  useEffect(() => {
    if (!selectingIn(ref.current, down.current)) setShown(text);
  }, [text]);
  useEffect(() => {
    const sync = () => {
      if (!selectingIn(ref.current, down.current)) setShown(latest.current);
    };
    const up = () => {
      down.current = false;
      sync();
    };
    document.addEventListener('selectionchange', sync);
    window.addEventListener('mouseup', up);
    return () => {
      document.removeEventListener('selectionchange', sync);
      window.removeEventListener('mouseup', up);
    };
  }, []);
  return (
    <div
      ref={ref}
      className={`tl tl-text msg-assistant streaming ${shown !== text ? 'paused' : ''}`}
      onMouseDown={() => (down.current = true)}
    >
      <span className="tl-dot" />
      <Markdown text={shown} />
    </div>
  );
}

/** 工具显示名：尽量用 Claude Code 的叫法，各家工具统一成一套 */
function toolName(name: string) {
  const n = name.toLowerCase();
  if (n === 'powershell') return 'PowerShell';
  if (n === 'toolsearch' || n.startsWith('mcp__')) return name;
  if (['bash', 'shell', 'execute', 'terminal', 'command'].some((x) => n.includes(x))) return 'Bash';
  if (n === 'write' || n === 'fs_write' || n === 'create') return 'Write';
  if (['edit', 'patch', 'notebook', 'str_replace'].some((x) => n.includes(x))) return 'Update';
  if (['read', 'view'].some((x) => n.includes(x))) return 'Read';
  if (n === 'grep') return 'Grep';
  if (n === 'glob') return 'Glob';
  if (n.includes('websearch') || n.includes('web_search')) return 'Web Search';
  if (['fetch', 'web'].some((x) => n.includes(x))) return 'Fetch';
  if (['search', 'find', 'grep', 'glob'].some((x) => n.includes(x))) return 'Search';
  if (n === 'task' || n === 'agent') return 'Task';
  if (n.includes('todo')) return tr('任务清单', 'Todo');
  if (n === 'delete') return 'Delete';
  if (n === 'move') return 'Move';
  if (n === 'think') return 'Think';
  return name;
}

type ToolKind = 'bash' | 'read' | 'edit' | 'search' | 'other';
function toolKind(shown: string, it: Item & { k: 'tool' }): ToolKind {
  if (shown === 'Bash' || shown === 'PowerShell') return 'bash';
  if (shown === 'Read') return 'read';
  if (shown === 'Update' || shown === 'Write' || it.diffs.length > 0) return 'edit';
  if (shown === 'Grep' || shown === 'Glob' || shown === 'Search') return 'search';
  return 'other';
}

/** 括号里的关键参数：路径相对工作目录；Cursor 的标题自带 "Read xxx" 这类前缀，去掉免得重复 */
const ARG_KEYS = ['command', 'pattern', 'query', 'file_path', 'notebook_path', 'path', 'url', 'description', 'skill', 'prompt'];

/** 括号里的关键参数：优先取输入里的常见字段（Cursor 的标题一开始只有 "Read File""Terminal"），
 *  再退回标题；标题是整段 JSON 时取第一个短字符串 */
function toolArg(it: Item & { k: 'tool' }, shown: string, cwd?: string) {
  const input = it.input && typeof it.input === 'object' ? (it.input as Record<string, unknown>) : {};
  // 改文件的工具先看路径：Kiro 的 write 把操作类型（strReplace / create）也放在 command 里
  const keys = shown === 'Update' || shown === 'Write' ? ['file_path', 'notebook_path', 'path', ...ARG_KEYS] : ARG_KEYS;
  const key = keys.find((k) => typeof input[k] === 'string' && input[k]);
  const opPath = Array.isArray(input.operations) ? (input.operations[0] as { path?: string } | undefined)?.path : undefined;
  let t = key ? String(input[key]) : opPath || it.title || it.diffs[0]?.path || '';
  // Codex 的命令带着 /bin/bash -lc '…' 外壳，标题是剥掉外壳的版本
  const bare = it.title.replace(/^`(.*)`$/s, '$1');
  if (key === 'command' && bare && !bare.startsWith('{') && t !== bare && t.includes(bare)) t = bare;
  if (!key && t.startsWith('{')) t = (Object.values(input).find((v) => typeof v === 'string' && v.length < 200) as string) ?? t;
  const low = t.toLowerCase();
  for (const w of [shown, it.name]) if (w && low.startsWith(w.toLowerCase() + ' ')) t = t.slice(w.length + 1);
  t = t.replace(/^(Reading|Running:?|Searching for|Writing|Editing|Executing)\s+/, '').replace(/^`(.*)`$/s, '$1').replace(/^"(.*?)"?$/s, '$1').replace(/\s+/g, ' ');
  if (cwd) {
    const base = cwd.replace(/[\\/]+$/, '');
    if (base) {
      const esc = base.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
      t = t.replace(new RegExp(esc + '[\\\\/]', 'g'), '').replace(new RegExp(esc + '(?=$|[\\s"\'])', 'g'), '.');
    }
  }
  return t;
}

/** 统一工具输出：Cursor 把结果包成 JSON（有时还多套一层字符串），拆成文本、退出码、命中数 */
interface ParsedOut {
  text: string;
  exitCode?: number;
  count?: number;
  tasks?: { done: boolean; text: string }[];
}

function parseOutput(raw: string): ParsedOut {
  let v: unknown = raw;
  for (let i = 0; i < 2 && typeof v === 'string' && /^\s*["{]/.test(v); i++) {
    try {
      v = JSON.parse(v);
    } catch {
      break;
    }
  }
  if (typeof v === 'string') {
    // Kiro 的命令失败只回一行 "exit code: N"
    const ec = /^exit code: (\d+)\s*/.exec(v);
    if (ec) return { text: v.slice(ec[0].length), exitCode: +ec[1] };
    // Claude 的 Grep / Glob：没有结果、或者 files_with_matches 模式开头的 "Found N files"
    if (/^No (files|matches) found/i.test(v)) return { text: v, count: 0 };
    const m = /^Found (\d+) (files?|matches?|lines?)/i.exec(v);
    return { text: v, count: m ? +m[1] : undefined };
  }
  if (!v || typeof v !== 'object') return { text: raw };
  let o = v as Record<string, unknown>;
  // Kiro：{"items":[{"Json":{…}}]} 或 {"items":[{"Text":"…"}]}
  const first = Array.isArray(o.items) ? (o.items[0] as Record<string, unknown> | undefined) : undefined;
  if (first && typeof first.Text === 'string') return parseOutput(first.Text);
  if (first?.Json && typeof first.Json === 'object') o = first.Json as Record<string, unknown>;
  if (typeof o.content === 'string') return { text: o.content };
  if (Array.isArray(o.tasks)) {
    const tasks = (o.tasks as Record<string, unknown>[]).map((t) => ({ done: !!t.completed, text: String(t.task_description ?? t.content ?? '') }));
    return { text: raw, tasks };
  }
  if ('exitCode' in o || 'stdout' in o) {
    const text = [o.stdout, o.stderr].filter((x) => typeof x === 'string' && x).join('\n');
    return { text, exitCode: typeof o.exitCode === 'number' ? o.exitCode : undefined };
  }
  const n = o.totalMatches ?? o.numMatches ?? o.totalFiles ?? o.numFiles;
  if (typeof n === 'number') return { text: raw, count: n };
  return { text: raw };
}

const firstLine = (s: string) => s.split('\n').find((l) => l.trim()) ?? '';
const nonEmptyLines = (s: string) => {
  const ls = s.replace(/\s+$/, '').split('\n');
  return ls.length === 1 && !ls[0] ? [] : ls;
};

/** 编辑类工具的小 diff：只看第一段改动，最多 10 行，带行号 */
function MiniDiff({ diff }: { diff: FileDiff }) {
  const lines = useMemo(() => diffLines(diff).filter((l) => l.t !== 'hunk'), [diff]);
  const first = lines.findIndex((l) => l.t !== 'ctx');
  const shown = lines.slice(Math.max(0, first - 1), Math.max(0, first - 1) + 10);
  return (
    <div className="tl-diff mono">
      {shown.map((l, i) => (
        <div key={i} className={`dl dl-${l.t}`}>
          <span className="dl-num">{l.n ?? ''}</span>
          <span className="dl-sign">{l.t === 'add' ? '+' : l.t === 'del' ? '-' : ' '}</span>
          <span className="dl-text">{l.text || ' '}</span>
        </div>
      ))}
    </div>
  );
}

/** 一次工具调用：● 工具名(关键参数)，下面 ⎿ 一段结果摘要，点开看完整输入输出 */
const ToolLine = memo(function ToolLine({ it, active, cwd }: { it: Item & { k: 'tool' }; active: boolean; cwd?: string }) {
  useLang();
  const [open, setOpen] = useState(false);
  const [full, setFull] = useState(false);
  const generic = (it.name === 'other' || it.name === 'think') && !!it.title;
  let shown = generic ? it.title : toolName(it.name);
  // 改的是已有内容就叫 Update，和 Claude Code 一致
  if (shown === 'Write' && it.diffs.some((d) => d.oldText)) shown = 'Update';
  const kind = toolKind(shown, it);
  const arg = generic || shown === tr('任务清单', 'Todo') ? '' : toolArg(it, shown, cwd);
  const raw = it.result?.output ?? '';
  const parsed = useMemo(() => parseOutput(raw), [raw]);
  const out = parsed.text;
  const err = !!it.result?.isError || (parsed.exitCode ?? 0) !== 0;
  const lines = useMemo(() => nonEmptyLines(out), [out]);
  const state = !it.result ? (active ? 'run' : 'idle') : err ? 'err' : 'ok';

  let summary: React.ReactNode;
  if (!it.result) summary = <span className="muted">{active ? tr('运行中…', 'Running…') : tr('（没有返回结果）', '(no result)')}</span>;
  else if (err)
    summary = (
      <span className="tl-err">
        {parsed.exitCode ? tr(`退出码 ${parsed.exitCode}`, `exit ${parsed.exitCode}`) : ''}
        {parsed.exitCode && firstLine(out) ? (isEn() ? ': ' : '：') : ''}
        {firstLine(out) || (parsed.exitCode ? '' : tr('失败', 'Failed'))}
      </span>
    );
  else if (kind === 'edit' && it.diffs.length) {
    const all = it.diffs.flatMap((d) => diffLines(d));
    const add = all.filter((l) => l.t === 'add').length;
    const del = all.filter((l) => l.t === 'del').length;
    summary = (
      <>
        <span className="muted">
          {[add && tr(`新增 ${add} 行`, `+${add} lines`), del && tr(`删除 ${del} 行`, `-${del} lines`)].filter(Boolean).join(isEn() ? ', ' : '，') || tr('没有改动', 'No changes')}
          {it.diffs.length > 1 && tr(`，共 ${it.diffs.length} 个文件`, `, ${it.diffs.length} files`)}
        </span>
        {!open && <MiniDiff diff={it.diffs[0]} />}
      </>
    );
  } else if (parsed.tasks)
    summary = parsed.tasks.length ? (
      <>
        {parsed.tasks.map((t, i) => (
          <div key={i} className={`plan-item ${t.done ? 'completed' : ''}`}>
            <span className="plan-box">{t.done ? '☑' : '☐'}</span>
            <span>{t.text}</span>
          </div>
        ))}
      </>
    ) : (
      <span className="muted">{tr('已清空', 'Cleared')}</span>
    );
  else if (kind === 'read') summary = <span className="muted">{tr(`读取 ${lines.length} 行`, `Read ${lines.length} lines`)}</span>;
  else if (kind === 'search') {
    const n = parsed.count ?? lines.length;
    summary = <span className="muted">{n ? tr(`找到 ${n} 项`, `${n} matches`) : tr('没有结果', 'No matches')}</span>;
  }
  else if (!lines.length) summary = <span className="muted">{tr('（无输出）', '(no output)')}</span>;
  else if (open) summary = <span className="muted">{tr(`输出 ${lines.length} 行`, `${lines.length} lines`)}</span>;
  else
    summary = (
      <>
        <div className="tl-out mono">{lines.slice(0, 3).join('\n')}</div>
        {lines.length > 3 && <span className="muted">{tr(`… +${lines.length - 3} 行`, `… +${lines.length - 3} lines`)}</span>}
      </>
    );

  const hasInput = kind !== 'edit' && it.input !== undefined && it.input !== null && JSON.stringify(it.input) !== '{}';
  const cmd = kind === 'bash' && it.input && typeof it.input === 'object' ? (it.input as { command?: string }).command : undefined;
  return (
    <div className={`tl tl-tool ${state}`}>
      <span className="tl-dot" />
      <div className="tl-main">
        <button className="tl-head" onClick={() => setOpen(!open)} title={it.title || it.name}>
          <span className="tl-name">{shown}</span>
          {arg && <span className="tl-arg">({arg})</span>}
        </button>
        <div className="tl-res">
          <span className="tl-elbow" />
          <div className="tl-res-body">{summary}</div>
        </div>
        {open && (
          <div className="tl-detail">
            {kind === 'edit' && it.diffs.map((d, i) => <DiffView key={i} diff={d} />)}
            {cmd ? <pre className="mono">{cmd}</pre> : hasInput && <pre className="mono">{typeof it.input === 'string' ? it.input : JSON.stringify(it.input, null, 2)}</pre>}
            {it.result && kind !== 'edit' && (
              <>
                <pre className="mono">{(full ? out : out.split('\n').slice(0, 200).join('\n')) || tr('（无输出）', '(no output)')}</pre>
                {!full && out.split('\n').length > 200 && (
                  <button className="link-btn small" onClick={() => setFull(true)}>
                    {tr(`显示全部 ${out.split('\n').length} 行`, `Show all ${out.split('\n').length} lines`)}
                  </button>
                )}
              </>
            )}
          </div>
        )}
      </div>
    </div>
  );
});

/** 零宽字符：模型偶尔会输出一长串，看不见，不能当成"已经有回复了" */
const INVISIBLE = /[\u200b-\u200d\u2060\ufeff]/g;

/** 回合进行中但一直没有新内容时，显示已经等了多久 */
function Waiting({ since, starting }: { since: number; starting?: boolean }) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);
  const sec = Math.floor((now - since) / 1000);
  return (
    <div className="typing">
      <span />
      <span />
      <span />
      {(sec >= 5 || (starting && sec >= 1)) && (
        <em className="typing-wait">
          {starting ? tr('正在启动工具', 'Starting tool') : tr('等待回复', 'Waiting for reply')} · {sec >= 60 ? (isEn() ? `${Math.floor(sec / 60)}m ${sec % 60}s` : `${Math.floor(sec / 60)} 分 ${sec % 60} 秒`) : isEn() ? `${sec}s` : `${sec} 秒`}
          {sec >= 60 && tr('，模型响应较慢，可以按 Esc 中断后重发', '. The model is slow — press Esc to interrupt and resend')}
        </em>
      )}
    </div>
  );
}

/** 思考：折成一行"✻ 思考"，点开看全文；流式中显示最后几行 */
const ktok = (n: number) => (n >= 1000 ? `${(n / 1000).toFixed(n >= 10000 ? 0 : 1)}k` : String(n));

/** 上下文压缩：进行中转圈，结束后原地换成结果（自动/手动、前后 token、耗时） */
function CompactNote({ ev, live }: { ev: HubEvent & { kind: 'compact' }; live: boolean }) {
  if (ev.phase === 'start') {
    return (
      <div className="note note-muted compact-note">
        {live ? (
          <>
            <span className="spinner" /> {tr('正在压缩上下文…', 'Compacting context…')}
          </>
        ) : (
          tr('上下文压缩未完成', 'Compaction did not finish')
        )}
      </div>
    );
  }
  if (ev.phase === 'failed') return <div className="note note-err">{tr('上下文压缩失败', 'Context compaction failed')}</div>;
  const how = ev.trigger === 'auto' ? tr('自动', 'auto') : ev.trigger === 'manual' ? tr('手动', 'manual') : '';
  const parts = [isEn() ? `Context compacted${how ? ` (${how})` : ''}` : `上下文已${how}压缩`];
  if (ev.preTokens != null && ev.postTokens != null) parts.push(`${ktok(ev.preTokens)} → ${ktok(ev.postTokens)} tokens`);
  else if (ev.preTokens != null) parts.push(tr(`压缩前 ${ktok(ev.preTokens)} tokens`, `${ktok(ev.preTokens)} tokens before`));
  if (ev.durationMs != null) parts.push(tr(`用时 ${Math.max(1, Math.round(ev.durationMs / 1000))} 秒`, `${Math.max(1, Math.round(ev.durationMs / 1000))}s`));
  return <div className="note note-muted compact-note">{parts.join(' · ')}</div>;
}

function Thinking({ text, live }: { text: string; live?: boolean }) {
  const [open, setOpen] = useState(false);
  const tail = live ? text.split('\n').slice(-6).join('\n') : text;
  return (
    <div className={`tl tl-think ${live ? 'live' : ''}`}>
      <span className="tl-star">✻</span>
      <div className="tl-main">
        <button className="tl-head" onClick={() => setOpen(!open)}>
          <span>{live ? tr('思考中…', 'Thinking…') : tr('思考', 'Thought')}</span>
          {!open && !live && <span className="tl-arg truncate">{text.replace(/\s+/g, ' ').slice(0, 120)}</span>}
        </button>
        {(open || live) && <div className="thinking-body">{open ? text : tail}</div>}
      </div>
    </div>
  );
}

type RunItem = Item & ({ k: 'tool' } | { k: 'thinking' });
type Block = Item | { k: 'fold'; key: string; items: RunItem[] };

/** 连续超过 8 次工具调用时，只留开头 2 个和最后 4 个，中间收成一行 */
function foldRuns(items: Item[]): Block[] {
  const out: Block[] = [];
  let run: RunItem[] = [];
  const flush = () => {
    const tools = run.filter((x) => x.k === 'tool').length;
    if (tools > 8 && run.length > 6) out.push(...run.slice(0, 2), { k: 'fold', key: `f${run[0].key}`, items: run.slice(2, -4) }, ...run.slice(-4));
    else out.push(...run);
    run = [];
  };
  for (const it of items) {
    if (it.k === 'tool' || it.k === 'thinking') run.push(it);
    else {
      flush();
      out.push(it);
    }
  }
  flush();
  return out;
}

function Fold({ items, render }: { items: RunItem[]; render: (it: RunItem) => React.ReactNode }) {
  const [open, setOpen] = useState(false);
  if (open) return <>{items.map(render)}</>;
  const n = items.filter((x) => x.k === 'tool').length;
  return (
    <button className="tl-fold" onClick={() => setOpen(true)}>
      {tr(`… 还执行了 ${n} 次工具（展开）`, `… ${n} more tool calls (expand)`)}
    </button>
  );
}

/** 图片按需取回并缓存，同一张图只请求一次 */
const imageCache = new Map<string, Promise<string>>();

function loadImage(hostId: string, sessionId: string, img: ImageRef) {
  const key = `${hostId}/${sessionId}/${img.id}`;
  let p = imageCache.get(key);
  if (!p) {
    p = rpc<{ mimeType: string; data: string }>(hostId, 'session.image', { sessionId, imageId: img.id }).then(
      (r) => `data:${r.mimeType};base64,${r.data}`,
    );
    p.catch(() => imageCache.delete(key));
    imageCache.set(key, p);
  }
  return p;
}

function MsgImage({ hostId, sessionId, img }: { hostId: string; sessionId: string; img: ImageRef }) {
  const [src, setSrc] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    let alive = true;
    loadImage(hostId, sessionId, img)
      .then((s) => alive && setSrc(s))
      .catch(() => alive && setFailed(true));
    return () => {
      alive = false;
    };
  }, [hostId, sessionId, img.id]);
  if (failed) return <div className="msg-img msg-img-missing">{tr('图片不可用', 'Image unavailable')}</div>;
  if (!src) return <div className="msg-img msg-img-loading" />;
  return (
    <button className="msg-img" title={tr('点击放大，右键复制或保存', 'Click to zoom, right-click to copy or save')} onClick={() => openImage(src)} onContextMenu={(e) => imageMenu(e, src)}>
      <img src={src} alt="" />
    </button>
  );
}

export function PlanList({ entries }: { entries: PlanEntry[] }) {
  return (
    <>
      {entries.map((e, i) => (
        <div key={i} className={`plan-item ${e.status}`}>
          <span className="plan-box">{e.status === 'completed' ? '☑' : e.status === 'in_progress' ? '◼' : '☐'}</span>
          <span>{e.content}</span>
        </div>
      ))}
    </>
  );
}

/** 时间线里的任务清单卡片：全部完成后收成一行，点开看明细 */
function Plan({ plan, busy }: { plan: PlanState; busy: boolean }) {
  const entries = viewEntries(plan, busy);
  const done = entries.filter((e) => e.status === 'completed').length;
  const all = entries.length > 0 && done === entries.length;
  const [open, setOpen] = useState<boolean | null>(null);
  const shown = open ?? !all;
  return (
    <div className="tl tl-tool ok tl-plan">
      <span className="tl-dot" />
      <div className="tl-main">
        <button className="tl-head plan-head" onClick={() => setOpen(!shown)} title={shown ? tr('收起', 'Collapse') : tr('展开', 'Expand')}>
          <span className="tl-name">{tr('任务清单', 'Todo')}</span>
          {plan.title && <span className="plan-title truncate">{plan.title}</span>}
          <span className="tl-arg">{all ? tr(`${done}/${entries.length} 已完成`, `${done}/${entries.length} done`) : `(${done}/${entries.length})`}</span>
          <span className={`chev ${shown ? 'open' : ''}`}>
            <Icon name="chevron" size={11} />
          </span>
        </button>
        {shown && (
          <div className="tl-res">
            <span className="tl-elbow" />
            <div className="tl-res-body">
              <PlanList entries={entries} />
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

export function PermissionCard({
  it,
  onAnswer,
  compact,
}: {
  it: {
    requestId: string;
    title: string;
    toolName: string;
    input: unknown;
    options: PermissionOption[];
    resolved?: string;
    questions?: Question[];
    answers?: QuestionAnswer[];
  };
  onAnswer?: (requestId: string, optionId: string, answers?: QuestionAnswer[]) => void | Promise<void>;
  compact?: boolean;
}) {
  if (it.questions?.length)
    return (
      <QuestionCard
        title={it.title}
        questions={it.questions}
        compact={compact}
        resolved={it.resolved ? { optionId: it.resolved, answers: it.answers ?? [] } : undefined}
        onSubmit={onAnswer ? (a) => onAnswer(it.requestId, 'answer', a) : undefined}
        onSkip={onAnswer ? () => onAnswer(it.requestId, 'skip') : undefined}
      />
    );
  return <PermissionBody it={it} onAnswer={onAnswer} compact={compact} />;
}

function PermissionBody({
  it,
  onAnswer,
  compact,
}: {
  it: { requestId: string; title: string; toolName: string; input: unknown; options: PermissionOption[]; resolved?: string };
  onAnswer?: (requestId: string, optionId: string) => void | Promise<void>;
  compact?: boolean;
}) {
  const [busy, setBusy] = useState(false);
  const [showInput, setShowInput] = useState(false);
  const [folded, setFolded] = useState(false);
  const resolvedLabel = it.resolved
    ? it.resolved === 'cancelled'
      ? tr('已取消', 'Cancelled')
      : (it.options.find((o) => o.id === it.resolved)?.label ?? it.resolved)
    : null;
  const cmd = (it.input as any)?.command;
  return (
    <div className={`perm-card ${it.resolved ? 'resolved' : ''} ${compact ? 'compact' : ''} ${folded ? 'folded' : ''}`}>
      <div className="perm-head" onClick={!it.resolved && compact ? () => setFolded(!folded) : undefined}>
        <Icon name="shield" size={16} />
        <span className="perm-title">{it.title}</span>
        {resolvedLabel && <span className="pill">{resolvedLabel}</span>}
        {!it.resolved && compact && (
          <button
            type="button"
            className="icon-btn card-fold-btn"
            title={folded ? tr('展开审批', 'Expand') : tr('暂时收起', 'Collapse')}
            onClick={(e) => {
              e.stopPropagation();
              setFolded(!folded);
            }}
          >
            <Icon name={folded ? 'up' : 'down'} size={14} />
          </button>
        )}
      </div>
      {!folded && (
        <>
      {!it.resolved && typeof cmd === 'string' && <pre className="mono perm-cmd">{cmd}</pre>}
      {!it.resolved && it.input !== undefined && it.input !== null && typeof cmd !== 'string' && (
        <>
          <button className="link-btn small" onClick={() => setShowInput(!showInput)}>
            {showInput ? tr('收起详情', 'Hide details') : tr('查看详情', 'Show details')}
          </button>
          {showInput && <pre className="mono perm-cmd">{JSON.stringify(it.input, null, 2)}</pre>}
        </>
      )}
      {!it.resolved && onAnswer && (
        <div className="perm-actions">
          {it.options.map((o) => (
            <button
              key={o.id}
              disabled={busy}
              className={o.kind.startsWith('allow') ? (o.kind === 'allow_once' ? 'btn primary' : 'btn') : 'btn danger-ghost'}
              onClick={async () => {
                setBusy(true);
                try {
                  await onAnswer(it.requestId, o.id);
                } finally {
                  setBusy(false);
                }
              }}
            >
              {o.label}
            </button>
          ))}
        </div>
      )}
        </>
      )}
    </div>
  );
}

// ---------- 简约风格：连续的思考和工具调用收成一组，标题一行概括，点开看每一步 ----------

/** 工具的界面叫法（简约风格用） */
function toolUiLabel(shown: string): string {
  const en: Record<string, string> = {
    Bash: 'Command',
    PowerShell: 'Command',
    Read: 'Read file',
    Write: 'Write file',
    Update: 'Edit file',
    Grep: 'Search text',
    Glob: 'Find files',
    Search: 'Search',
    'Web Search': 'Web search',
    Fetch: 'Open page',
    Task: 'Subagent',
    Delete: 'Delete file',
    Move: 'Move file',
    Think: 'Think',
  };
  const zh: Record<string, string> = {
    Bash: '终端命令',
    PowerShell: '终端命令',
    Read: '读取文件',
    Write: '写入文件',
    Update: '编辑文件',
    Grep: '搜索内容',
    Glob: '查找文件',
    Search: '搜索',
    'Web Search': '搜索网页',
    Fetch: '打开网页',
    Task: '子 Agent',
    Delete: '删除文件',
    Move: '移动文件',
    Think: '思考',
  };
  return (isEn() ? en : zh)[shown] ?? shown;
}

const KIND_ICON: Record<ToolKind, string> = { bash: 'terminal', read: 'file', edit: 'edit', search: 'search', other: 'settings' };

const baseName = (p: string) => p.replace(/[\\/]+$/, '').split(/[\\/]/).pop() || p;

/** 一次工具调用要显示的东西，两种风格共用的解析放在这里 */
function toolView(it: Item & { k: 'tool' }, cwd?: string) {
  const generic = (it.name === 'other' || it.name === 'think') && !!it.title;
  let shown = generic ? it.title : toolName(it.name);
  if (shown === 'Write' && it.diffs.some((d) => d.oldText)) shown = 'Update';
  const kind = toolKind(shown, it);
  const arg = generic || shown === tr('任务清单', 'Todo') ? '' : toolArg(it, shown, cwd);
  const parsed = parseOutput(it.result?.output ?? '');
  const err = !!it.result?.isError || (parsed.exitCode ?? 0) !== 0;
  return { shown, kind, arg, parsed, err, label: generic ? it.title : toolUiLabel(shown) };
}

function fmtDur(ms: number) {
  const s = Math.max(0, Math.round(ms / 1000));
  if (isEn()) return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`;
  return s < 60 ? `${s}秒` : `${Math.floor(s / 60)}分${String(s % 60).padStart(2, '0')}秒`;
}

/** 每秒刷新一次的当前时间，只在需要计时的时候开 */
function useNow(on: boolean) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!on) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [on]);
  return now;
}

/** 组里的一次工具调用：图标 + 中文名 + 关键参数，点开看输入输出 */
const ModernTool = memo(function ModernTool({ it, active, cwd }: { it: Item & { k: 'tool' }; active: boolean; cwd?: string }) {
  useLang();
  const [open, setOpen] = useState(false);
  const v = useMemo(() => toolView(it, cwd), [it, it.result, it.title, it.input, it.diffs, cwd]);
  const running = !it.result && active;
  const desc = v.kind === 'read' || v.kind === 'edit' ? baseName(v.arg) : v.arg;
  const stat = useMemo(() => {
    if (v.kind !== 'edit' || !it.diffs.length) return null;
    const all = it.diffs.flatMap((d) => diffLines(d));
    return { add: all.filter((l) => l.t === 'add').length, del: all.filter((l) => l.t === 'del').length };
  }, [v.kind, it.diffs]);
  const out = v.parsed.text;
  const input =
    v.kind === 'bash' && it.input && typeof it.input === 'object' && typeof (it.input as { command?: unknown }).command === 'string'
      ? String((it.input as { command: string }).command)
      : it.input === undefined || it.input === null || JSON.stringify(it.input) === '{}'
        ? ''
        : typeof it.input === 'string'
          ? it.input
          : JSON.stringify(it.input, null, 2);
  return (
    <div className={`mt-row ${v.err ? 'err' : ''}`}>
      <button className="mt-head" onClick={() => setOpen(!open)} title={v.arg || it.title || it.name} aria-expanded={open}>
        <span className="mt-icon">{running ? <span className="spinner mini" /> : <Icon name={v.err ? 'x' : KIND_ICON[v.kind]} size={13} />}</span>
        <span className="mt-name">{v.label}</span>
        {desc && <span className="mt-desc">{desc}</span>}
        {stat && (stat.add > 0 || stat.del > 0) && (
          <span className="mt-stat">
            {stat.add > 0 && <span className="add">+{stat.add}</span>}
            {stat.del > 0 && <span className="del">-{stat.del}</span>}
          </span>
        )}
        {v.err && <span className="mt-fail">{v.parsed.exitCode ? tr(`退出码 ${v.parsed.exitCode}`, `exit ${v.parsed.exitCode}`) : tr('失败', 'Failed')}</span>}
      </button>
      {open && (
        <div className="mt-detail">
          {v.kind === 'edit' && it.diffs.length > 0 ? (
            it.diffs.map((d, i) => <DiffView key={i} diff={d} />)
          ) : (
            <div className="mt-box mono">
              {input && (
                <>
                  <div className="mt-box-label">{tr('输入', 'Input')}</div>
                  <pre>{input}</pre>
                </>
              )}
              {it.result ? (
                <>
                  <div className="mt-box-label">{tr('输出', 'Output')}</div>
                  <pre className={v.err ? 'err' : ''}>{v.parsed.tasks ? v.parsed.tasks.map((t) => `${t.done ? '☑' : '☐'} ${t.text}`).join('\n') : out.split('\n').slice(0, 400).join('\n') || tr('（无输出）', '(no output)')}</pre>
                </>
              ) : (
                <div className="mt-box-label">{running ? tr('运行中…', 'Running…') : tr('没有返回结果', 'No result')}</div>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
});

/** 组里的一段思考：转圈或对勾 + 已思考 · 开头一句，点开看全文 */
function ModernThinking({ text, live }: { text: string; live?: boolean }) {
  const [open, setOpen] = useState(false);
  const has = !!text.trim();
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (live && box.current) box.current.scrollTop = box.current.scrollHeight;
  }, [live, text]);
  return (
    <div className="mt-row mt-think">
      <button className="mt-head" disabled={!has} onClick={() => setOpen(!open)} aria-expanded={open}>
        <span className="mt-icon">{live ? <span className="spinner mini" /> : <span className="ok-icon"><Icon name="check" size={12} /></span>}</span>
        <span className="mt-name">{live ? tr('思考中', 'Thinking') : tr('已思考', 'Thought')}</span>
        {has && <span className="mt-desc">· {text.replace(/\s+/g, ' ').slice(0, 120)}</span>}
      </button>
      {has && (open || live) && (
        <div className="mt-box mono mt-think-box" ref={box}>
          <pre>{text}</pre>
        </div>
      )}
    </div>
  );
}

/** 一组过程：标题一行（步数框 + 概括 + 耗时），进行中自动展开，结束后收起 */
function ActGroup({
  items,
  running,
  end,
  liveThought,
  cwd,
}: {
  items: RunItem[];
  running: boolean;
  /** 这组后面第一条内容的时间，算耗时用 */
  end: number | null;
  liveThought?: string;
  cwd?: string;
}) {
  const [toggled, setToggled] = useState<boolean | null>(null);
  const open = toggled ?? running;
  const tools = items.filter((x): x is Item & { k: 'tool' } => x.k === 'tool');
  const failed = tools.filter((t) => toolView(t, cwd).err).length;
  const start = items[0]?.ts;
  const now = useNow(running);
  const dur = start ? (running ? now : end) : null;
  let summary: string;
  if (running) {
    const cur = [...tools].reverse().find((t) => !t.result);
    if (cur) {
      const v = toolView(cur, cwd);
      const d = v.kind === 'read' || v.kind === 'edit' ? baseName(v.arg) : v.arg;
      summary = isEn() ? `${v.label}${d ? ` · ${d}` : ''}` : `正在${v.label.replace(/^正在/, '')}${d ? ` · ${d}` : ''}`;
    } else summary = liveThought != null || items[items.length - 1]?.k === 'thinking' ? tr('正在思考', 'Thinking') : tr('正在执行', 'Working');
  } else if (!tools.length) summary = tr('已思考', 'Thought');
  else
    summary = failed
      ? tr(`执行工具 ${tools.length} 次，其中 ${failed} 次失败`, `${tools.length} tool calls, ${failed} failed`)
      : tr(`执行工具 ${tools.length} 次`, `${tools.length} tool calls`);
  return (
    <div className="act-group">
      <button className="ag-head" onClick={() => setToggled(!open)} aria-expanded={open}>
        <span className={`ag-box ${tools.length ? 'has-count' : ''} ${open ? 'open' : ''}`}>
          {tools.length > 0 && <span className="ag-count">{tools.length}</span>}
          <span className="ag-chev">
            <Icon name="chevron" size={12} />
          </span>
        </span>
        <span className="ag-sum">
          {summary}
          {start && dur != null && dur > start && <span className="ag-dur"> · {fmtDur(dur - start)}</span>}
        </span>
      </button>
      {open && (
        <div className="ag-body">
          {items.map((x) =>
            x.k === 'tool' ? <ModernTool key={x.key} it={x} active={running} cwd={cwd} /> : <ModernThinking key={x.key} text={x.text} />,
          )}
          {liveThought != null && <ModernThinking text={liveThought} live />}
        </div>
      )}
    </div>
  );
}

type MBlock = Item | { k: 'group'; key: string; items: RunItem[]; end: number | null };

/** 简约风格：相邻的思考和工具调用合成一组，组的结束时间取后面第一条带时间的内容 */
function groupRuns(items: Item[]): MBlock[] {
  const out: MBlock[] = [];
  let run: RunItem[] = [];
  const flush = (next?: Item) => {
    if (!run.length) return;
    const end = next && 'ts' in next ? next.ts : null;
    out.push({ k: 'group', key: `g${run[0].key}`, items: run, end });
    run = [];
  };
  for (const it of items) {
    if (it.k === 'tool' || it.k === 'thinking') run.push(it);
    else {
      flush(it);
      out.push(it);
    }
  }
  flush();
  return out;
}

const PHRASES_ZH = ['正在思考', '琢磨一下', '整理思路', '马上就好', '稍等片刻', '推敲中', '连点成线', '酝酿中'];
const PHRASES_EN = ['Thinking', 'Working on it', 'Putting it together', 'Almost there', 'One moment', 'Considering', 'Connecting the dots', 'Mulling it over'];

/** 简约风格的等待提示：点阵动画 + 轮换的短句 + 已等待秒数 */
function ModernWaiting({ since, starting }: { since: number; starting?: boolean }) {
  const phrases = isEn() ? PHRASES_EN : PHRASES_ZH;
  const now = useNow(true);
  const [i, setI] = useState(() => Math.floor(Math.random() * phrases.length));
  useEffect(() => {
    const t = setInterval(() => setI((x) => (x + 1 + Math.floor(Math.random() * (phrases.length - 1))) % phrases.length), 10_000);
    return () => clearInterval(t);
  }, [phrases.length]);
  const sec = Math.floor((now - since) / 1000);
  return (
    <div className="m-waiting" role="status">
      <span className="dot-matrix" aria-hidden>
        {Array.from({ length: 9 }, (_, k) => (
          <i key={k} style={{ animationDelay: `${[0, 1, 2, 1, 2, 3, 2, 3, 4][k] * 0.14}s` }} />
        ))}
      </span>
      <span>{starting ? tr('正在启动工具', 'Starting tool') : phrases[i % phrases.length]}</span>
      {sec >= 1 && <span className="m-waiting-sec">{sec >= 60 ? (isEn() ? `${Math.floor(sec / 60)}m ${sec % 60}s` : `${Math.floor(sec / 60)}分${sec % 60}秒`) : isEn() ? `${sec}s` : `${sec}秒`}</span>}
      {sec >= 60 && (
        <span className="m-waiting-sec">
          {isMobile()
            ? tr('，模型响应较慢，可以点停止按钮中断后重发', '. Slow response — tap Stop to interrupt and resend')
            : tr('，模型响应较慢，可以按 Esc 中断后重发', '. Slow response — press Esc to interrupt and resend')}
        </span>
      )}
    </div>
  );
}

function fullTime(ms: number) {
  const t = new Date(ms);
  const p = (n: number) => String(n).padStart(2, '0');
  return `${t.getFullYear()}-${p(t.getMonth() + 1)}-${p(t.getDate())} ${p(t.getHours())}:${p(t.getMinutes())}`;
}

/** 简约风格每轮回复下面的操作行：复制、耗时和用量，时间在鼠标移上去时显示 */
function ModernTurnFooter({ it, latest }: { it: Item & { k: 'turn' }; latest: boolean }) {
  const u = it.usage;
  const parts: string[] = [];
  if (it.ms != null && it.ms >= 0) parts.push(`${tr('耗时', 'Took')} ${fmtDur(it.ms)}`);
  if (u) {
    if (u.inputTokens || u.cacheReadTokens || u.cacheWriteTokens) parts.push(`${tr('输入', 'In')} ${fmtTokens(u.inputTokens + u.cacheReadTokens + u.cacheWriteTokens)}`);
    if (u.outputTokens) parts.push(`${tr('输出', 'Out')} ${fmtTokens(u.outputTokens)}`);
    if (u.costUsd) parts.push(`$${u.costUsd.toFixed(u.costUsd < 1 ? 3 : 2)}`);
    if (u.credits) parts.push(`${u.credits.toFixed(2)} credits`);
  }
  if (it.model) parts.push(it.model);
  const tip = u
    ? tr(
        `输入 ${u.inputTokens}（其中缓存读取 ${u.cacheReadTokens}、缓存写入 ${u.cacheWriteTokens} 另计）\n输出 ${u.outputTokens}`,
        `Input ${u.inputTokens} (cache read ${u.cacheReadTokens}, cache write ${u.cacheWriteTokens} counted separately)\nOutput ${u.outputTokens}`,
      )
    : '';
  return (
    <div className={`m-actions ${latest ? 'latest' : ''}`}>
      {it.reason && <span className="attn-text">{it.reason}</span>}
      {it.text && (
        <button
          className="m-act-btn"
          title={tr('复制这一轮的回复', 'Copy this reply')}
          onClick={async () => toast((await writeClipboard(it.text)) ? tr('已复制', 'Copied') : tr('复制失败', 'Copy failed'))}
        >
          <Icon name="copy" size={14} />
        </button>
      )}
      <span className="m-act-meta" title={tip}>
        {parts.join(' · ')}
      </span>
      <span className="m-act-time">{fullTime(it.ts)}</span>
    </div>
  );
}

const PAGE = 300;

export function ChatView({
  events,
  live,
  liveThinking,
  info,
  hostId,
  onAnswer,
  hasMore,
  loadingOlder,
  loading,
  onLoadOlder,
  focusSeq,
  pendingOutside,
  onSendText,
}: {
  events: HubEvent[];
  live: Map<string, string>;
  liveThinking?: Map<string, string>;
  info?: SessionInfo;
  /** 有它才能显示用户消息里的图片 */
  hostId?: string;
  onAnswer: (requestId: string, optionId: string, answers?: QuestionAnswer[]) => void | Promise<void>;
  /** Kiro 正文里的提问，回答拼成一条消息发出去 */
  onSendText?: (text: string) => Promise<void>;
  /** yi-agentd 上还有更早的事件没取 */
  hasMore?: boolean;
  loadingOlder?: boolean;
  /** 正在从主机取会话记录（第一次打开、重新加载） */
  loading?: boolean;
  onLoadOlder?: () => void;
  /** 滚到这条消息并高亮（全局搜索跳过来） */
  focusSeq?: number;
  /** 未处理的授权请求已经在别处（输入框上方）显示 */
  pendingOutside?: boolean;
}) {
  const hideCompactCmd = info?.tool === 'claude' || info?.tool === 'codex';
  const items = useMemo(() => buildItems(events, hideCompactCmd), [events, events.length, hideCompactCmd]);
  const [limit, setLimit] = useState(PAGE);
  const ref = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const liveText = [...live.values()].join('').replace(INVISIBLE, '');
  const liveThought = [...(liveThinking?.values() ?? [])].join('');
  const shown = items.slice(Math.max(0, items.length - limit));
  const modern = useChatStyle() === 'modern';
  const blocks: (Block | MBlock)[] = useMemo(() => {
    // 任务类工具的调用由任务清单代替显示，出错的照常显示
    const list = shown.filter((x) => !(x.k === 'tool' && PLAN_TOOLS.has(x.name) && !x.result?.isError));
    return modern ? groupRuns(list) : foldRuns(list);
  }, [items, limit, modern]);
  // 最后一轮（还没有 turn 结尾）里的工具才可能在运行
  const lastTurnEnd = useMemo(() => shown.map((x) => x.k).lastIndexOf('turn'), [shown]);
  const liveKeys = useMemo(() => new Set(shown.slice(lastTurnEnd + 1).map((x) => x.key)), [shown, lastTurnEnd]);
  const busy = info && (info.status === 'running' || info.status === 'starting');
  // 最近一次有新内容的时间（按本机时间算，不受主机时钟偏差影响）
  const lastChange = useRef(Date.now());
  const changeKey = `${events.length}:${liveText.length}:${liveThought.length}:${busy}`;
  const prevKey = useRef(changeKey);
  if (prevKey.current !== changeKey) {
    prevKey.current = changeKey;
    lastChange.current = Date.now();
  }

  // 往前翻时记下翻之前的内容高度，插入更早的消息后把滚动位置补回来，不让画面跳
  const keepFrom = useRef<number | null>(null);
  const older = () => {
    const el = ref.current;
    keepFrom.current = el ? el.scrollHeight - el.scrollTop : null;
    if (items.length > shown.length) setLimit(limit + PAGE);
    else if (hasMore && onLoadOlder) {
      setLimit(limit + PAGE);
      onLoadOlder();
    }
  };
  const canOlder = items.length > shown.length || !!hasMore;

  const [away, setAway] = useState(false);
  const toBottom = (smooth = false) => {
    const el = ref.current;
    if (!el) return;
    stick.current = true;
    el.scrollTo({ top: el.scrollHeight, behavior: smooth ? 'smooth' : 'auto' });
  };
  const onScroll = () => {
    const el = ref.current;
    if (!el) return;
    const gap = el.scrollHeight - el.scrollTop - el.clientHeight;
    stick.current = gap < 80;
    const far = gap > el.clientHeight * 0.6;
    if (far !== away) setAway(far);
    if (el.scrollTop < 40 && canOlder && !loadingOlder && keepFrom.current == null) older();
  };

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    if (keepFrom.current != null) {
      el.scrollTop = el.scrollHeight - keepFrom.current;
      if (!loadingOlder) keepFrom.current = null;
    } else if (stick.current && !selectingIn(el, false)) el.scrollTop = el.scrollHeight;
  });

  // 对话区变矮（手机弹出输入法、输入框长高）时，原来在底部的继续贴着底部
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const ro = new ResizeObserver(() => {
      if (stick.current && keepFrom.current == null && !selectingIn(el, false)) el.scrollTop = el.scrollHeight;
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // 自己刚发出的消息：不管现在翻到哪里，都回到底部
  const lastUser = useMemo(() => {
    for (let i = items.length - 1; i >= 0; i--) if (items[i].k === 'user') return items[i].key;
    return '';
  }, [items]);
  const seenUser = useRef(lastUser);
  useEffect(() => {
    if (lastUser && seenUser.current && lastUser !== seenUser.current) toBottom();
    seenUser.current = lastUser;
  }, [lastUser]);

  useEffect(() => {
    stick.current = true;
    setLimit(PAGE);
  }, [info?.id]);

  // 跳到指定消息：还没加载到就继续往前取，直到取到或到头
  const focused = useRef<number | null>(null);
  useEffect(() => {
    if (focusSeq == null || focused.current === focusSeq) return;
    const el = ref.current?.querySelector(`[data-key="e${focusSeq}"]`);
    if (el) {
      focused.current = focusSeq;
      stick.current = false;
      el.scrollIntoView({ block: 'center' });
      el.classList.add('flash');
      setTimeout(() => el.classList.remove('flash'), 2400);
      return;
    }
    const first = events.find((e) => e.seq > 0)?.seq ?? Infinity;
    if (first > focusSeq && hasMore && !loadingOlder && onLoadOlder) {
      setLimit(Infinity);
      onLoadOlder();
    } else if (items.length > shown.length) setLimit(Infinity);
  });

  // 简约风格：最后一组还在本轮里、正文还没开始输出时，算进行中（展开、计时）
  const lastBlock = blocks[blocks.length - 1];
  const tailGroup = modern && lastBlock?.k === 'group' && !!busy && !liveText.trim() && liveKeys.has(lastBlock.items[0].key) ? lastBlock : null;
  const lastTurnKey = useMemo(() => {
    for (let i = blocks.length - 1; i >= 0; i--) if (blocks[i].k === 'turn') return blocks[i].key;
    return '';
  }, [blocks]);

  // Kiro 没有提问工具：最后一条回复里列了编号问题和 a) b) 选项、并且之后没再发消息时，给一张可以点选的卡片
  const [dismissedAsk, setDismissedAsk] = useState('');
  const textAsk = useMemo(() => {
    if (info?.tool !== 'kiro' || busy || !onSendText) return null;
    let last: (Item & { k: 'text' }) | undefined;
    for (let i = items.length - 1; i >= 0; i--) {
      const x = items[i];
      if (x.k === 'user') return null;
      if (x.k === 'text') {
        last = x;
        break;
      }
    }
    if (!last || last.key === dismissedAsk) return null;
    const qs = parseTextQuestions(last.text);
    return qs.length ? { key: last.key, questions: qs } : null;
  }, [items, info?.tool, busy, onSendText, dismissedAsk]);

  const host = useStore((st) => (hostId ? st.hosts.find((h) => h.config.id === hostId)?.config : undefined));
  const imgCtx = useMemo<ImgCtx | null>(
    () => (hostId ? { hostId, hostName: host?.name ?? hostId, local: host?.kind === 'local', cwd: info?.cwd } : null),
    [hostId, host?.name, host?.kind, info?.cwd],
  );
  const runItem = (x: RunItem) =>
    x.k === 'tool' ? (
      <ToolLine key={x.key} it={x} active={!!busy && liveKeys.has(x.key)} cwd={info?.cwd} />
    ) : (
      <Thinking key={x.key} text={x.text} />
    );

  return (
    <ImgContext.Provider value={imgCtx}>
    <div
      className={`chat ${modern ? 'modern' : ''}`}
      ref={ref}
      onScroll={onScroll}
      onClick={(e) => {
        // 回复里的图片点一下放大
        const t = e.target as HTMLElement;
        if (t.tagName === 'IMG' && t.closest('.md')) openImage((t as HTMLImageElement).src);
      }}
      onContextMenu={(e) => {
        const t = e.target as HTMLElement;
        if (t.tagName === 'IMG' && t.closest('.md')) imageMenu(e, (t as HTMLImageElement).src);
      }}
    >
      <div className="chat-inner">
        {loading && (
          <div className={`chat-loading ${events.length ? 'top' : ''}`} role="status">
            <span className="spinner" />
            {tr('正在从主机抓取会话消息…', 'Fetching messages from host…')}
          </div>
        )}
        {canOlder && (
          <button className="link-btn small center" disabled={loadingOlder} onClick={older}>
            {loadingOlder ? tr('正在加载更早的消息…', 'Loading earlier messages…') : tr('显示更早的消息', 'Show earlier messages')}
          </button>
        )}
        {blocks.map((it) => {
          switch (it.k) {
            case 'fold':
              return <Fold key={it.key} items={it.items} render={runItem} />;
            case 'group':
              return (
                <ActGroup
                  key={it.key}
                  items={it.items}
                  running={it === tailGroup}
                  end={it.end}
                  liveThought={it === tailGroup && liveThought ? liveThought : undefined}
                  cwd={info?.cwd}
                />
              );
            case 'tool':
            case 'thinking':
              return runItem(it);
            case 'user':
              return (
                <div key={it.key} data-key={it.key} className="msg-user">
                  {it.images.length > 0 && hostId && info && (
                    <div className="msg-imgs">
                      {it.images.map((img) => (
                        <MsgImage key={img.id} hostId={hostId} sessionId={info.id} img={img} />
                      ))}
                    </div>
                  )}
                  {it.text.trim() && <div className="bubble">{it.text}</div>}
                  {modern ? (
                    <div className="msg-user-actions">
                      <span className="msg-time">{fullTime(it.ts)}</span>
                      {it.text.trim() && (
                        <button
                          className="m-act-btn"
                          title={tr('复制消息', 'Copy message')}
                          onClick={async () => toast((await writeClipboard(it.text)) ? tr('已复制', 'Copied') : tr('复制失败', 'Copy failed'))}
                        >
                          <Icon name="copy" size={13} />
                        </button>
                      )}
                    </div>
                  ) : (
                    <div className="msg-time">{clock(it.ts)}</div>
                  )}
                </div>
              );
            case 'text':
              return (
                <div key={it.key} data-key={it.key} className="tl tl-text msg-assistant">
                  <span className="tl-dot" />
                  <Markdown text={it.text} />
                </div>
              );
            case 'perm':
              // 还没处理的授权由外面固定在输入框上方显示，这里不再重复
              if (!it.resolved && pendingOutside) return null;
              return <PermissionCard key={it.key} it={it} onAnswer={onAnswer} />;
            case 'plan':
              return <Plan key={it.key} plan={it.plan} busy={!!busy} />;
            case 'turn':
              return modern ? <ModernTurnFooter key={it.key} it={it} latest={it.key === lastTurnKey} /> : <TurnFooter key={it.key} it={it} />;
            case 'bgdone':
              return <BgTaskNote key={it.key} it={it} />;
            case 'note':
              return (
                <div key={it.key} className={`note note-${it.tone}`}>
                  {it.text}
                </div>
              );
            case 'compact':
              return <CompactNote key={it.key} ev={it.ev} live={!!busy} />;
          }
        })}
        {textAsk && onSendText && (
          <QuestionCard
            key={textAsk.key}
            title={tr('Kiro 在等你选择', 'Kiro is waiting for your choice')}
            questions={textAsk.questions}
            submitLabel={tr('发送回答', 'Send answer')}
            onSubmit={async (a) => {
              await onSendText(textAnswer(textAsk.questions, a));
              setDismissedAsk(textAsk.key);
            }}
            onSkip={() => setDismissedAsk(textAsk.key)}
          />
        )}
        {liveThought && !liveText && !modern && <Thinking text={liveThought} live />}
        {liveThought && !liveText && modern && !tailGroup && <ActGroup key="live-think" items={[]} running end={null} liveThought={liveThought} />}
        {liveText.trim() && <LiveMarkdown text={liveText} />}
        {busy && !liveText.trim() && !liveThought && !modern && <Waiting key={lastChange.current} since={lastChange.current} starting={info?.status === 'starting'} />}
        {busy && !liveText.trim() && !liveThought && modern && !tailGroup?.items.some((x) => x.k === 'tool' && !x.result) && (
          <ModernWaiting key={lastChange.current} since={lastChange.current} starting={info?.status === 'starting'} />
        )}
        {items.length === 0 && !busy && !loading && <div className="empty">{tr('发送一条消息开始', 'Send a message to start')}</div>}
        {away && (
          <button className="jump-bottom" title={tr('回到底部', 'Jump to bottom')} onClick={() => toBottom(true)}>
            <Icon name="arrow-down" size={16} />
            {busy && <span className="jump-live" />}
          </button>
        )}
      </div>
    </div>
    </ImgContext.Provider>
  );
}

/** 后台任务结束的提示：图标 + 状态 + 一行命令（放不下省略，点开看全文） */
function BgTaskNote({ it }: { it: Extract<Item, { k: 'bgdone' }> }) {
  const [open, setOpen] = useState(false);
  const icon = it.status === 'done' ? 'check' : it.status === 'failed' ? 'x' : it.status === 'stopped' ? 'stop' : 'terminal';
  return (
    <button
      type="button"
      className={`bgdone bgdone-${it.status} ${open ? 'open' : ''}`}
      title={open ? undefined : it.what}
      onClick={() => setOpen((v) => !v)}
    >
      <span className="bgdone-icon"><Icon name={icon} size={12} /></span>
      <span className="bgdone-label">{it.label}</span>
      {it.what && <code className="bgdone-what">{it.what}</code>}
    </button>
  );
}
