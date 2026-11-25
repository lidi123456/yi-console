import { ChatMarkdown } from './ChatMarkdown';
import { createContext, memo, useContext, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useBack } from '../back';
import { ENGINE_ORIGIN, EngineNote } from './EngineNote';
import { ContextRefBar } from './ContextRefBar';
import { parseContextMessage } from '../contextRefs';
import { parseFindingFollowUp } from '../reviewState';
import { type ImgCtx } from '../chatImages';
import { getT, isEn, trErr, useLang } from '../i18n';
import { rpc } from '../transport';
import { TOOL_LABEL, type FileDiff, type HubEvent, type ImageRef, type MemoryEntry, type MemoryEvent, type MemoryListResponse, type PermissionOption, type PlanEntry, type Question, type QuestionAnswer, type SessionInfo, type TurnCheckpoint, type Usage } from '../types';
import { clock, fmtTokens, isMobile, lsGet, lsSet } from '../util';
import { writeClipboard } from '../clipboard';
import { errorToast, navigate, toast, useStore } from '../store';
import { handoffSession } from './SessionActions';
import { MemoryEditorModal, type MemoryEditorInit } from './MemoryView';
import { DiffView, diffLines } from './DiffView';
import { openImage } from './Lightbox';
import { imageMenu } from './ImageMenu';
import { Icon } from './Icon';
import { asyncAnswer, asyncAnswers, QuestionCard } from './QuestionCard';
import { isPlanTool, PlanTracker, viewEntries, type PlanState } from '../plan';
import { limitHead, limitOutcome, type LimitEvent } from '../limit';

const t = getT('chatView');

type Item =
  | { k: 'desktop'; key: string; ev: GatewayEvent }
  | { k: 'user'; key: string; seq: number; text: string; ts: number; images: ImageRef[]; origin?: string }
  | { k: 'text'; key: string; ts: number; text: string; id?: string; ask?: AsyncAsk }
  | { k: 'thinking'; key: string; ts: number; text: string }
  | {
      k: 'tool';
      key: string;
      /** The tool call id */
      id: string;
      ts: number;
      name: string;
      title: string;
      input: unknown;
      diffs: FileDiff[];
      result?: { output: string; isError: boolean };
      /** The gateway's record of this desktop tool call: status and screenshots */
      desktop?: GatewayEvent;
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
  /** One usage-limit episode: the hit plus the latest decision about the automatic continue */
  | { k: 'limit'; key: string; hit?: LimitEvent; last: LimitEvent }
  | { k: 'handoff'; key: string; ev: HubEvent & { kind: 'handoff' } }
  | { k: 'account_switched'; key: string; ev: HubEvent & { kind: 'account_switched' } }
  | { k: 'memory'; key: string; ev: HubEvent & { kind: 'memory' } }
  | {
      k: 'turn';
      key: string;
      seq: number;
      ms: number | null;
      ts: number;
      usage: Usage | null;
      tools: number;
      model?: string | null;
      reason?: string;
      text: string;
    };

const DESKTOP_SERVER = 'yi_computer_use';
const BROWSER_SERVER = 'yi_browser';

/** A gateway's record of an AI tool's MCP call: the desktop (`yi_computer_use`) or the AI browser (`yi_browser`) */
type GatewayEvent = HubEvent & { kind: 'computer_use' | 'browser_use' };

/** The tool row is a call of the gateway's MCP server (Claude/Qoder mcp__yi_computer_use__x, Codex yi_computer_use.x, ACP titles) */
function isDesktopTool(it: Item & { k: 'tool' }, ev: GatewayEvent) {
  // CodeBuddy calls MCP tools through DeferExecuteTool, whose input names the real tool
  const wrapped = (it.input as { toolName?: unknown } | null)?.toolName;
  const id = `${it.name} ${it.title} ${typeof wrapped === 'string' ? wrapped : ''}`;
  return id.includes(ev.kind === 'browser_use' ? BROWSER_SERVER : DESKTOP_SERVER) ? id.includes(ev.tool) : false;
}

/** Texts of a gateway event, from the namespace of its gateway (literal calls, so i18n:check sees every key) */
function gatewayText(ev: GatewayEvent) {
  const seconds = (ev.durationMs / 1000).toFixed(1);
  if (ev.kind === 'browser_use') {
    const tx = getT('browserUse');
    return {
      notes: { enable: tx('noteEnabled'), pause: tx('notePaused'), resume: tx('noteResumed'), stop: tx('noteStopped'), disable: tx('noteStopped'), headed: tx('noteHeaded'), headless: tx('noteHeadless') } as Record<string, string>,
      status: { running: tx('running'), completed: tx('completed'), failed: tx('failed'), control: tx('control'), unverified: tx('unverified'), interrupted: tx('interrupted') } as Record<string, string>,
      action: tx('action', { tool: ev.tool }),
      elapsed: tx('elapsed', { seconds }),
    };
  }
  const tx = getT('computerUse');
  return {
    notes: { enable: tx('noteEnabled'), pause: tx('notePaused'), resume: tx('noteResumed'), stop: tx('noteStopped'), disable: tx('noteStopped') } as Record<string, string>,
    status: { running: tx('running'), completed: tx('completed'), failed: tx('failed'), control: tx('control'), unverified: tx('unverified'), interrupted: tx('interrupted') } as Record<string, string>,
    action: tx('action', { tool: ev.tool }),
    elapsed: tx('elapsed', { seconds }),
  };
}

function desktopControlText(ev: GatewayEvent) {
  const x = gatewayText(ev);
  return x.notes[ev.tool] ?? x.status.control;
}

/** Host and session for screenshots inside tool rows */
const DesktopCtx = createContext<{ hostId: string; sessionId: string } | null>(null);

function DesktopShots({ ev }: { ev: GatewayEvent }) {
  const ctx = useContext(DesktopCtx);
  if (!ev.images?.length || !ctx) return null;
  return <div className="computer-use-images">{ev.images.map((img) => <MsgImage key={img.id} hostId={ctx.hostId} sessionId={ctx.sessionId} img={img} />)}</div>;
}

/** Status worth flagging on a desktop tool row; a plain return is not proof, so "unverified" is shown too */
function desktopStatus(ev?: GatewayEvent) {
  if (!ev) return null;
  const tx = getT('computerUse');
  if (ev.status === 'unverified') return tx('unverified');
  if (ev.status === 'interrupted') return tx('interrupted');
  return null;
}

/** hideCompactCmd: when the tool reports compaction progress (Claude, Codex, Cursor's emulated one, Antigravity), don't show the user's "/compact" bubble */
function buildItems(events: HubEvent[], hideCompactCmd = false): Item[] {
  const items: Item[] = [];
  const tools = new Map<string, Item & { k: 'tool' }>();
  const desktopTools = new Map<string, Item & { k: 'tool' }>();
  // Gateway actions that started before their tool row arrived (Codex reports the MCP call after it began)
  const earlyRuns: GatewayEvent[] = [];
  const perms = new Map<string, Item & { k: 'perm' }>();
  let planIdx = -1;
  const planner = new PlanTracker();
  // The current turn: starts at the first user message after the previous turn ended
  let turnStart: number | null = null;
  let turnUsage: Usage | null = null;
  let turnTools = 0;
  let turnTexts: string[] = [];
  let exitNote: Extract<Item, { k: 'note' }> | null = null;
  let openAsk: AsyncAsk | null = null;
  for (const e of events) {
    const key = `e${e.seq}`;
    // Keep only the latest task list, positioned at its last update; if the list is cleared, keep the previous one
    if (planner.feed(e) && planner.plan) {
      if (planIdx >= 0) items.splice(planIdx, 1);
      items.push({ k: 'plan', key: `p${e.seq}`, plan: planner.plan });
      planIdx = items.length - 1;
    }
    switch (e.kind) {
      case 'computer_use':
      case 'browser_use': {
        // A desktop action is the AI tool's own MCP call seen from the gateway: fold it into that tool row
        // (OpenBot shows them as ordinary tool steps). Match by server and tool name first; otherwise take the
        // latest call of this turn still waiting for its result, since the gateway action runs inside it
        // (CodeBuddy calls MCP tools through a DeferExecuteTool wrapper whose name and input don't say which).
        // Unmatched ones are control changes or manual captures.
        let tool = desktopTools.get(e.actionId);
        if (!tool && e.status !== 'control') {
          let pending: (Item & { k: 'tool' }) | undefined;
          for (let i = items.length - 1; i >= 0; i--) {
            const it = items[i];
            if (it.k === 'user') break;
            if (it.k !== 'tool' || it.desktop) continue;
            if (isDesktopTool(it, e)) {
              tool = it;
              break;
            }
            if (!it.result) pending ??= it;
          }
          tool ??= pending;
        }
        if (tool) {
          tool.desktop = e;
          desktopTools.set(e.actionId, tool);
          break;
        }
        if (e.status === 'control') {
          items.push({ k: 'note', key: `desktop-${e.actionId}`, tone: 'muted', text: desktopControlText(e) });
          break;
        }
        const old = items.findIndex((item) => item.k === 'desktop' && item.ev.actionId === e.actionId);
        const next: Item = { k: 'desktop', key: `desktop-${e.actionId}`, ev: e };
        if (old >= 0) items[old] = next; else if (e.images?.length || e.status !== 'running') items.push(next);
        else earlyRuns.push(e);
        break;
      }
      case 'user_message':
        // The first message after an async question is its answer
        if (openAsk && e.origin !== ENGINE_ORIGIN) {
          openAsk.answer = e.text;
          openAsk = null;
        }
        if (!(hideCompactCmd && e.text.trim() === '/compact')) {
          items.push({ k: 'user', key, seq: e.seq, text: e.text, ts: e.ts, images: e.images ?? [], origin: e.origin });
        }
        turnStart ??= e.ts;
        break;
      case 'assistant_text':
        items.push({ k: 'text', key, ts: e.ts, text: e.text, id: e.id });
        turnTexts.push(e.text);
        break;
      case 'async_question': {
        // Attach to the reply it came with: the answered card replaces that reply in the timeline
        let it: (Item & { k: 'text' }) | undefined;
        for (let i = items.length - 1; i >= 0 && !it; i--) {
          const x = items[i];
          if (x.k === 'text' && x.id === e.id) it = x;
        }
        const ask: AsyncAsk = { id: e.id, questions: e.questions };
        if (it) it.ask = ask;
        else items.push({ k: 'text', key, ts: e.ts, text: e.questions.map((q) => q.question).join('\n\n'), id: e.id, ask });
        openAsk = ask;
        break;
      }
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
            id: e.id,
            ts: e.ts,
            name: e.name,
            title: e.title,
            input: e.input,
            diffs: e.diffs ?? [],
          };
          tools.set(e.id, it);
          items.push(it);
          turnTools++;
          const early = earlyRuns.findIndex((r) => desktopTools.get(r.actionId) === undefined && isDesktopTool(it, r));
          if (early >= 0) {
            it.desktop = earlyRuns[early];
            desktopTools.set(earlyRuns[early].actionId, it);
            earlyRuns.splice(early, 1);
          }
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
        // yi-agentd's own wording for a failed automatic continue is Chinese; tool errors are shown as they are
        items.push({ k: 'note', key, text: /^(自动继续失败|子 agent 在等待审批)/.test(e.message) ? trErr(e.message) : e.message, tone: 'err' });
        break;
      case 'system': {
        // yi-agentd records Claude's task_notification as "后台任务完成：<command>"; draw it as a single line
        const bg = /^后台任务(完成|失败|已停止|\s*\S+?)：([\s\S]*)$/.exec(e.text.trim());
        if (bg) {
          const s = bg[1] === '完成' ? 'done' : bg[1] === '失败' ? 'failed' : bg[1] === '已停止' ? 'stopped' : 'other';
          // Codex / Antigravity subagents: "后台任务完成：子 agent <name>"
          const sub = /^子 agent /.test(bg[2]);
          const label =
            s === 'done' ? t(sub ? 'subagentTaskDone' : 'backgroundTaskDone')
            : s === 'failed' ? t(sub ? 'subagentTaskFailed' : 'backgroundTaskFailed')
            : s === 'stopped' ? t(sub ? 'subagentTaskStopped' : 'backgroundTaskStopped')
            : t('backgroundTask', { v: bg[1].trim() });
          const what = (sub ? bg[2].replace(/^子 agent /, '') : bg[2]).replace(/\s+/g, ' ').trim();
          items.push({ k: 'bgdone', key, status: s, label, what });
        } else if (e.text.trim()) {
          const text = /^已切入独立 Worktree/.test(e.text) ? trErr(e.text) : e.text;
          items.push({ k: 'note', key, text, tone: 'muted' });
        }
        break;
      }
      case 'rate_limit': {
        // Later phases update the line of the last hit in place
        let last: (Item & { k: 'limit' }) | undefined;
        for (let i = items.length - 1; i >= 0 && !last; i--) {
          const x = items[i];
          if (x.k === 'limit') last = x;
          else if (x.k === 'user') break;
        }
        if (e.phase === 'hit' || !last) items.push({ k: 'limit', key, hit: e.phase === 'hit' ? e : undefined, last: e });
        else last.last = e;
        break;
      }
      case 'handoff':
        items.push({ k: 'handoff', key, ev: e });
        break;
      case 'account_switched':
        items.push({ k: 'account_switched', key, ev: e });
        break;
      case 'memory':
        items.push({ k: 'memory', key, ev: e });
        break;
      case 'interrupted':
        items.push({ k: 'note', key, text: t('turnCutOffByRestart'), tone: 'muted' });
        break;
      case 'compact': {
        // On completion, replace the "compacting" line in place with the result
        let last: Item | undefined;
        let lastIdx = -1;
        for (let i = items.length - 1; i >= 0 && !last; i--) if (items[i].k === 'compact') { last = items[i]; lastIdx = i; }
        // Older yi-agentd recorded every repeated compacting event from Claude as a start: skip it if one in the same turn hasn't finished yet
        if (e.phase === 'start' && last?.k === 'compact' && last.ev.phase === 'start' && !items.slice(lastIdx + 1).some((x) => x.k === 'turn' || x.k === 'user')) break;
        if (e.phase !== 'start' && last?.k === 'compact' && last.ev.phase === 'start') {
        // If the tool didn't report a duration (Codex), use the time between start and end
          last.ev = e.durationMs == null ? { ...e, durationMs: e.ts - last.ev.ts } : e;
        }
        else items.push({ k: 'compact', key, ev: e });
        break;
      }
      case 'status':
        // The error for an abnormal process exit: if the process was started again afterwards (session resumed, or auto-restarted on send), turn it into a grey "recovered" line instead of red
        if (e.status === 'error') {
          const last = items[items.length - 1];
          exitNote = last?.k === 'note' && last.tone === 'err' ? last : null;
        } else if (exitNote && e.status !== 'exited') {
          exitNote.tone = 'muted';
          exitNote.text = `${exitNote.text.split('\n')[0]}${t('resumed')}`;
          exitNote = null;
        }
        if (e.status === 'exited') items.push({ k: 'note', key, text: t('sessionProcessEndedSendingA'), tone: 'muted' });
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
          seq: e.seq,
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

/** Reply display style: compact (steps folded into a group, default) or Claude Code (dot timeline on the left). Stored on this device only */
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

/** Copy button with brief checkmark feedback (1.5s), shared by tool detail blocks and approval command blocks */
function InlineCopyBtn({
  text,
  label,
  className = '',
  size = 13,
}: {
  text: string;
  label: string;
  className?: string;
  size?: number;
}) {
  useLang();
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current);
    },
    [],
  );
  const title = copied ? t('copied') : label;
  return (
    <button
      type="button"
      className={`inline-copy-btn ${copied ? 'copied' : ''} ${className}`.trim()}
      title={title}
      aria-label={title}
      onClick={async (e) => {
        e.stopPropagation();
        if (!(await writeClipboard(text))) {
          toast(t('copyFailed'), 'error');
          return;
        }
        setCopied(true);
        if (timer.current) clearTimeout(timer.current);
        timer.current = setTimeout(() => setCopied(false), 1500);
      }}
    >
      <Icon name={copied ? 'check' : 'copy'} size={size} />
    </button>
  );
}

/** `<pre>` with a floating copy button positioned outside `<pre>` so manual drag-selection doesn't include it */
function CopyPre({
  text,
  label,
  className,
  children,
}: {
  text: string;
  label: string;
  className?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="copy-pre-wrap">
      {text ? <InlineCopyBtn text={text} label={label} className="copy-float-btn" size={13} /> : null}
      <pre className={className}>{children}</pre>
    </div>
  );
}

/** Mobile-only plain-text selection sheet: clean text with no interactive links or cards, making partial drag-selection easy on phones */
function SelectTextModal({ text, onClose }: { text: string; onClose: () => void }) {
  useLang();
  useBack(true, onClose);
  const bodyRef = useRef<HTMLDivElement>(null);
  const closeBtn = useRef<HTMLButtonElement>(null);
  const [selected, setSelected] = useState('');
  useLayoutEffect(() => {
    const prev = document.activeElement as HTMLElement | null;
    closeBtn.current?.focus();
    return () => prev?.focus();
  }, []);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || e.isComposing) return;
      e.preventDefault();
      e.stopImmediatePropagation();
      onClose();
    };
    const onSel = () => {
      const box = bodyRef.current;
      const s = window.getSelection();
      if (!box || !s || s.isCollapsed) {
        setSelected('');
        return;
      }
      const inBox = (!!s.anchorNode && box.contains(s.anchorNode)) || (!!s.focusNode && box.contains(s.focusNode));
      setSelected(inBox ? s.toString() : '');
    };
    window.addEventListener('keydown', onKey, true);
    document.addEventListener('selectionchange', onSel);
    return () => {
      window.removeEventListener('keydown', onKey, true);
      document.removeEventListener('selectionchange', onSel);
    };
  }, [onClose]);
  return (
    <div className="select-text-modal" role="dialog" aria-modal="true" aria-label={t('selectText')}>
      <div className="select-text-head">
        <button ref={closeBtn} type="button" className="icon-btn" title={t('close')} aria-label={t('close')} onClick={onClose}>
          <Icon name="back" size={18} />
        </button>
        <div className="select-text-titles">
          <strong>{t('selectText')}</strong>
          <span className="muted small">{t('selectTextHint')}</span>
        </div>
        <button
          type="button"
          className={`btn small ${selected.trim() ? 'primary' : ''}`}
          // Keep the user's text selection intact when tapping the copy button
          onPointerDown={(e) => e.preventDefault()}
          onMouseDown={(e) => e.preventDefault()}
          onClick={async () => {
            const payload = selected.trim() ? selected : text;
            toast((await writeClipboard(payload)) ? t('copied') : t('copyFailed'));
          }}
        >
          <Icon name="copy" size={13} />
          <span>{selected.trim() ? t('copySelection') : t('copyAll')}</span>
        </button>
      </div>
      <div ref={bodyRef} className="select-text-body">
        {text}
      </div>
    </div>
  );
}

/** The line under each reply: duration, tokens, cost, tool call count, model, plus copying the whole turn */
function TurnFooter({
  it,
  checkpoint,
  onOpenTurnDiff,
  onBringBack,
  onSelectText,
}: {
  it: Item & { k: 'turn' };
  checkpoint?: TurnCheckpoint;
  onOpenTurnDiff?: (turn: number) => void;
  onBringBack?: (text: string) => void;
  onSelectText?: (text: string) => void;
}) {
  const u = it.usage;
  const parts: string[] = [];
  if (it.ms != null && it.ms >= 0) parts.push(fmtMs(it.ms));
  if (u) {
    if (u.inputTokens || u.cacheReadTokens || u.cacheWriteTokens)
      parts.push(`${t('in')} ${fmtTokens(u.inputTokens + u.cacheReadTokens + u.cacheWriteTokens)}`);
    if (u.outputTokens) parts.push(`${t('out')} ${fmtTokens(u.outputTokens)}`);
    if (u.costUsd) parts.push(`$${u.costUsd.toFixed(u.costUsd < 1 ? 3 : 2)}`);
    if (u.credits) parts.push(`${u.credits.toFixed(2)} credits`);
  }
  if (it.tools) parts.push(t('toolCallS', { tools: it.tools }));
  if (it.model) parts.push(it.model);
  const tip = u
    ? t('inputCacheReadCacheWrite', { inputTokens: u.inputTokens, cacheReadTokens: u.cacheReadTokens, cacheWriteTokens: u.cacheWriteTokens, outputTokens: u.outputTokens })
    : '';
  return (
    <div className="turn-foot" title={tip}>
      {it.reason && <span className="attn-text">{it.reason}</span>}
      <span>{parts.join(' · ')}</span>
      {checkpoint && checkpoint.filesChanged > 0 && checkpoint.endRef && onOpenTurnDiff && (
        <button type="button" className="turn-ckpt-btn" title={t('viewTurnDiff')} onClick={() => onOpenTurnDiff(checkpoint.turn)}>
          <span>{t('turnFilesChanged', { count: checkpoint.filesChanged })}</span>
          <span className="ls-a">+{checkpoint.added}</span>
          <span className="ls-d">−{checkpoint.deleted}</span>
        </button>
      )}
      <span className="turn-time">{clock(it.ts)}</span>
      {it.text && onBringBack && (
        <button
          type="button"
          className="turn-bring-btn"
          title={t('bringBackThisReply')}
          onClick={() => onBringBack(it.text)}
        >
          <Icon name="back" size={12} />
          <span>{t('bringBackThisReply')}</span>
        </button>
      )}
      {it.text && (
        <>
          <button
            className="turn-copy"
            title={t('copyThisReply')}
            aria-label={t('copyThisReply')}
            onClick={async () => toast((await writeClipboard(it.text)) ? t('copied') : t('copyFailed'))}
          >
            <Icon name="copy" size={13} />
          </button>
          {onSelectText && (
            <button
              type="button"
              className="turn-copy mobile-select-btn"
              title={t('selectText')}
              aria-label={t('selectText')}
              onClick={() => onSelectText(it.text)}
            >
              <Icon name="text" size={13} />
            </button>
          )}
        </>
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

/** Which host and directory images in replies are loaded from (chatImages.ts) */
const ImgContext = createContext<ImgCtx | null>(null);

function Markdown({ text, streaming = false }: { text: string; streaming?: boolean }) {
  const ctx = useContext(ImgContext);
  return <ChatMarkdown text={text} streaming={streaming} ctx={ctx} />;
}

/** The user is selecting text in this element (or is pressing / dragging with mouse or touch) */
function selectingIn(el: HTMLElement | null, pointerDown: boolean) {
  if (!el) return false;
  if (pointerDown) return true;
  const s = window.getSelection();
  if (!s || s.isCollapsed) return false;
  return (!!s.anchorNode && el.contains(s.anchorNode)) || (!!s.focusNode && el.contains(s.focusNode));
}

/** Streaming body: don't refresh while the user is selecting text (or holding touch), otherwise each new chunk redraws the block and breaks selection */
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
    window.addEventListener('pointerup', up);
    window.addEventListener('pointercancel', up);
    return () => {
      document.removeEventListener('selectionchange', sync);
      window.removeEventListener('pointerup', up);
      window.removeEventListener('pointercancel', up);
    };
  }, []);
  return (
    <div
      ref={ref}
      className={`tl tl-text msg-assistant streaming ${shown !== text ? 'paused' : ''}`}
      onPointerDown={() => (down.current = true)}
    >
      <span className="tl-dot" />
      <Markdown text={shown} streaming />
    </div>
  );
}

/** Tool display names: use Claude Code's names where possible so every tool shares one set */
function toolName(name: string, input?: unknown) {
  const n = name.toLowerCase();
  if (n === 'powershell') return 'PowerShell';
  // MCP calls keep their own name: Claude's mcp__server__tool and Codex's server.tool (the substring guesses below
  // would misread them, e.g. yi_review contains "view", memory_read contains "read")
  if (n === 'toolsearch' || n.startsWith('mcp__') || /^[\w-]+\.[\w-]+$/.test(n)) return name;
  if (['bash', 'shell', 'execute', 'terminal', 'command'].some((x) => n.includes(x))) return 'Bash';
  if (n === 'write' || n === 'fs_write' || n === 'create' || n === 'write_file' || n === 'write_to_file') return 'Write';
  if (['edit', 'patch', 'notebook', 'str_replace', 'replace_file_content', 'sed_file'].some((x) => n.includes(x))) return 'Update';
  // Antigravity: read_url_content / search_web / grep_search / find_by_name / list_dir / subagent tools
  if (n === 'read_url_content') return 'Fetch';
  if (n === 'search_web') return 'Web Search';
  if (n === 'grep_search') return 'Grep';
  if (n === 'find_by_name') return 'Glob';
  if (n === 'list_dir' || n === 'ls') return 'LS';
  if (['read', 'view'].some((x) => n.includes(x))) return 'Read';
  if (n === 'grep') return 'Grep';
  if (n === 'glob') return 'Glob';
  if (n.includes('websearch') || n.includes('web_search')) return 'Web Search';
  if (['fetch', 'web'].some((x) => n.includes(x))) return 'Fetch';
  if (['search', 'find', 'grep', 'glob'].some((x) => n.includes(x))) return 'Search';
  if (n === 'task' || n === 'agent' || n === 'invoke_subagent' || n === 'dispatch_agent' || n === 'subagent' || n === 'delegate' || n === 'spawn_agent') return 'Task';
  if (n === 'swarm') return 'Task';
  if (n === 'send_message' && input && typeof input === 'object' && ('Recipient' in input || 'recipient' in input)) return 'Task';
  if (n === 'ask_question' || n === 'run ask_question' || n === 'askuserquestion' || n === 'request_user_input') return 'AskUserQuestion';
  if (n.includes('todo')) return t('todo');
  if (n === 'delete') return 'Delete';
  if (n === 'move') return 'Move';
  if (n === 'think') return 'Think';
  const obj = input && typeof input === 'object' ? (input as Record<string, unknown>) : null;
  if (obj && typeof obj.CommandLine === 'string') return 'Bash';
  return name;
}

type ToolKind = 'bash' | 'read' | 'edit' | 'search' | 'other';
function toolKind(shown: string, it: Item & { k: 'tool' }): ToolKind {
  if (shown === 'Bash' || shown === 'PowerShell') return 'bash';
  if (shown === 'Read' || shown === 'ReviewDiff') return 'read';
  if (shown === 'Update' || shown === 'Write' || it.diffs.length > 0) return 'edit';
  if (shown === 'Grep' || shown === 'Glob' || shown === 'Search' || shown === 'LS') return 'search';
  return 'other';
}

/** Extract concise summary & readable detail for subagent/task calls (Claude/Codex/Antigravity) */
function formatTaskPrompt(input: unknown): { summary: string; detail: string } {
  if (!input || typeof input !== 'object') return { summary: '', detail: '' };
  const o = input as Record<string, unknown>;
  // Antigravity batch invoke_subagent: { Subagents: [{ TypeName, Role, Prompt }], TaskMode }
  if (Array.isArray(o.Subagents) && o.Subagents.length > 0) {
    const items = o.Subagents.filter((x): x is Record<string, unknown> => !!x && typeof x === 'object');
    const summary = items
      .map((s) => {
        const role = String(s.Role ?? s.TypeName ?? '').trim();
        const prompt = String(s.Prompt ?? '').trim();
        return role ? (prompt ? `[${role}] ${prompt}` : role) : prompt;
      })
      .filter(Boolean)
      .join(' | ');
    const detail = items
      .map((s, i) => {
        const role = String(s.Role ?? '').trim();
        const type = String(s.TypeName ?? '').trim();
        const head = role && type && role !== type ? `${role} (${type})` : role || type || `#${i + 1}`;
        const prompt = String(s.Prompt ?? '').trim();
        return `[${head}]\n${prompt}`.trim();
      })
      .join('\n\n');
    return { summary, detail };
  }
  // Antigravity single invoke_subagent: { TypeName, Role, Prompt }
  if (typeof o.Role === 'string' || typeof o.TypeName === 'string' || typeof o.Prompt === 'string') {
    const role = String(o.Role ?? '').trim();
    const type = String(o.TypeName ?? '').trim();
    const shortRole = role || type;
    const fullRole = role && type && role !== type ? `${role} (${type})` : shortRole;
    const prompt = String(o.Prompt ?? o.prompt ?? o.description ?? '').trim();
    const summary = shortRole ? (prompt ? `[${shortRole}] ${prompt}` : shortRole) : prompt;
    const detail = fullRole ? (prompt ? `[${fullRole}]\n${prompt}` : `[${fullRole}]`) : prompt;
    return { summary, detail };
  }
  // Antigravity send_message: { Recipient, Message }
  if (typeof o.Recipient === 'string' || typeof o.recipient === 'string') {
    const recipient = String(o.Recipient ?? o.recipient ?? '').trim();
    const msg = String(o.Message ?? o.message ?? '').trim();
    const shortId = recipient.slice(0, 8);
    const summary = shortId ? `[→ ${shortId}] ${msg}`.trim() : msg;
    const detail = recipient ? `[→ ${recipient}]\n${msg}`.trim() : msg;
    return { summary, detail };
  }
  // Antigravity swarm: { action, name, brief, tasks }
  if (typeof o.brief === 'string' || typeof o.action === 'string') {
    const action = String(o.action ?? '').trim();
    const name = String(o.name ?? '').trim();
    const brief = String(o.brief ?? '').trim();
    const head = ['swarm', action, name].filter(Boolean).join(' ');
    const summary = brief ? `[${head}] ${brief}` : `[${head}]`;
    const tasks = Array.isArray(o.tasks) ? o.tasks.map((t, i) => `${i + 1}. ${String(t)}`).join('\n') : '';
    const detail = [summary, tasks].filter(Boolean).join('\n');
    return { summary, detail };
  }
  const fallback = String(o.description ?? o.prompt ?? o.task ?? '').trim();
  return { summary: fallback, detail: fallback };
}

/** Extract concise summary & readable detail for question tool calls (Claude/Codex/Cursor/Antigravity) */
function formatQuestionPrompt(input: unknown): { summary: string; detail: string } {
  if (!input || typeof input !== 'object') return { summary: '', detail: '' };
  const qs = (input as { questions?: unknown }).questions;
  if (!Array.isArray(qs) || !qs.length) return { summary: '', detail: '' };
  const items = qs.filter((q): q is Record<string, unknown> => !!q && typeof q === 'object');
  if (!items.length) return { summary: '', detail: '' };
  const first = String(items[0].header ?? items[0].question ?? items[0].prompt ?? '').trim();
  const summary = items.length > 1 && first ? `${first} (+${items.length - 1})` : first;
  const detail = items
    .map((q, i) => {
      const qText = String(q.question ?? q.prompt ?? q.header ?? `#${i + 1}`).trim();
      const opts = Array.isArray(q.options)
        ? q.options
            .map((o) => (typeof o === 'string' ? o : o && typeof o === 'object' ? String((o as Record<string, unknown>).label ?? (o as Record<string, unknown>).text ?? '') : ''))
            .filter(Boolean)
        : [];
      return [qText, ...opts.map((o) => `  - ${o}`)].join('\n');
    })
    .join('\n\n');
  return { summary, detail };
}

/** Tools with no real name show their title as is. Only subagent / question titles (Antigravity's invoke_subagent /
 *  send_message / swarm / ask_question when a replay leaves the name out) are mapped, so other tools' titles stay as the tool wrote them */
function isGenericTool(it: Item & { k: 'tool' }) {
  if ((it.name !== 'other' && it.name !== 'think') || !it.title || reviewToolName(it)) return false;
  const mapped = toolName(it.title, it.input);
  return mapped !== 'Task' && mapped !== 'AskUserQuestion';
}

/** Name shown for a tool call; a generic one maps to Task when its title is a subagent tool */
function shownToolName(it: Item & { k: 'tool' }) {
  const review = reviewToolName(it);
  if (review) return REVIEW_SHOWN[review];
  return isGenericTool(it) ? it.title : toolName((it.name === 'other' || it.name === 'think') && it.title ? it.title : it.name, it.input);
}

/** Subagents' own tool calls: Antigravity's, read from their transcripts (`acp.rs` gives them `agy-sub:` ids), and
 *  Codex's, from their threads' notifications (`codex.rs`, `cx-sub:`) */
const isSubagentTool = (it: Item & { k: 'tool' }) => it.id.startsWith('agy-sub:') || it.id.startsWith('cx-sub:');

/** Key argument in parentheses: paths relative to the working directory; Cursor titles carry prefixes like "Read xxx", strip them to avoid repetition */
const ARG_KEYS = ['command', 'pattern', 'query', 'file_path', 'notebook_path', 'path', 'url', 'description', 'skill', 'prompt', 'toolSummary', 'ImageName', 'Name'];

/** Key argument in parentheses: prefer common fields from the input (Cursor titles start out as just "Read File" / "Terminal"),
 *  then fall back to the title; if the title is a JSON blob, take the first short string */
function toolArg(it: Item & { k: 'tool' }, shown: string, cwd?: string) {
  const review = reviewToolName(it);
  if (review) return reviewToolArg(it, review);
  if (shown === 'Task') {
    const sub = formatTaskPrompt(it.input).summary;
    if (sub) return sub.replace(/\s+/g, ' ');
  }
  if (shown === 'AskUserQuestion') {
    const sub = formatQuestionPrompt(it.input).summary;
    return sub.replace(/\s+/g, ' ');
  }
  const input: Record<string, unknown> = it.input && typeof it.input === 'object' ? { ...(it.input as Record<string, unknown>) } : {};
  // File-editing tools check the path first: Kiro's write also puts the operation type (strReplace / create) in command
  const keys = shown === 'Update' || shown === 'Write' ? ['file_path', 'notebook_path', 'path', ...ARG_KEYS] : ARG_KEYS;
  // Antigravity names its arguments in PascalCase (CommandLine, Path, TargetFile, DirectoryPath, SearchPath…)
  const pascal: Record<string, string[]> = {
    command: ['CommandLine', 'toolSummary'],
    path: ['Path', 'TargetFile', 'AbsolutePath', 'FilePath', 'DirectoryPath', 'SearchPath', 'SearchDirectory'],
    pattern: ['Pattern', 'Query'],
    query: ['Query', 'Pattern'],
    url: ['Url'],
    prompt: ['Prompt', 'Message'],
  };
  for (const [k, aliases] of Object.entries(pascal)) {
    if (input[k] === undefined) {
      const hit = aliases.find((a) => typeof input[a] === 'string' && input[a]);
      if (hit) input[k] = input[hit];
    }
  }
  const key = keys.find((k) => typeof input[k] === 'string' && input[k]);
  const opPath = Array.isArray(input.operations) ? (input.operations[0] as { path?: string } | undefined)?.path : undefined;
  let t = key ? String(input[key]) : opPath || it.title || it.diffs[0]?.path || '';
  // Codex commands come wrapped in /bin/bash -lc '…'; the title is the unwrapped version
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

/** Normalize tool output: Cursor wraps results in JSON (sometimes inside another string); split into text, exit code and match count */
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
    // A failed Kiro command only returns one line, "exit code: N"
    const ec = /^exit code: (\d+)\s*/.exec(v);
    if (ec) return { text: v.slice(ec[0].length), exitCode: +ec[1] };
    // Claude's Grep / Glob: no results, or "Found N files" at the start in files_with_matches mode
    if (/^No (files|matches) found/i.test(v)) return { text: v, count: 0 };
    const m = /^Found (\d+) (files?|matches?|lines?)/i.exec(v);
    return { text: v, count: m ? +m[1] : undefined };
  }
  if (!v || typeof v !== 'object') return { text: raw };
  let o = v as Record<string, unknown>;
  // Kiro: {"items":[{"Json":{…}}]} or {"items":[{"Text":"…"}]}
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

/** Small diff for editing tools: only the first hunk, at most 10 lines, with line numbers */
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

/** One tool call: ● ToolName(key argument), with a ⎿ result summary below; click to see the full input and output */
const ToolLine = memo(function ToolLine({ it, active, cwd }: { it: Item & { k: 'tool' }; active: boolean; cwd?: string }) {
  useLang();
  const [open, setOpen] = useState(false);
  const [full, setFull] = useState(false);
  const generic = isGenericTool(it);
  let shown = shownToolName(it);
  // Changing existing content is called Update, same as Claude Code
  if (shown === 'Write' && it.diffs.some((d) => d.oldText)) shown = 'Update';
  const kind = toolKind(shown, it);
  const arg = generic || shown === t('todo') ? '' : toolArg(it, shown, cwd);
  const raw = it.result?.output ?? '';
  const parsed = useMemo(() => parseOutput(raw), [raw]);
  const out = parsed.text;
  const err = !!it.result?.isError || (parsed.exitCode ?? 0) !== 0;
  const lines = useMemo(() => nonEmptyLines(out), [out]);
  const state = !it.result ? (active ? 'run' : 'idle') : err ? 'err' : 'ok';

  let summary: React.ReactNode;
  if (!it.result) summary = <span className="muted">{active ? t('running') : t('noResult')}</span>;
  else if (err)
    summary = (
      <span className="tl-err">
        {parsed.exitCode ? t('exit', { exitCode: parsed.exitCode }) : ''}
        {parsed.exitCode && firstLine(out) ? (isEn() ? ': ' : '：') : ''}
        {firstLine(out) || (parsed.exitCode ? '' : t('failed'))}
      </span>
    );
  else if (kind === 'edit' && it.diffs.length) {
    const all = it.diffs.flatMap((d) => diffLines(d));
    const add = all.filter((l) => l.t === 'add').length;
    const del = all.filter((l) => l.t === 'del').length;
    summary = (
      <>
        <span className="muted">
          {[add && t('lines', { add }), del && t('lines2', { del })].filter(Boolean).join(isEn() ? ', ' : '，') || t('noChanges')}
          {it.diffs.length > 1 && t('files', { length: it.diffs.length })}
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
      <span className="muted">{t('cleared')}</span>
    );
  else if (kind === 'read') summary = <span className="muted">{t('readLines', { length: lines.length })}</span>;
  else if (kind === 'search') {
    const n = parsed.count ?? lines.length;
    summary = <span className="muted">{n ? t('matches', { n }) : t('noMatches')}</span>;
  }
  else if (!lines.length) summary = <span className="muted">{t('noOutput')}</span>;
  else if (open) summary = <span className="muted">{t('lines3', { length: lines.length })}</span>;
  else
    summary = (
      <>
        <div className="tl-out mono">{lines.slice(0, 3).join('\n')}</div>
        {lines.length > 3 && <span className="muted">{t('lines4', { v: lines.length - 3 })}</span>}
      </>
    );

  const hasInput = kind !== 'edit' && it.input !== undefined && it.input !== null && JSON.stringify(it.input) !== '{}';
  const inputObj = it.input && typeof it.input === 'object' ? (it.input as { command?: string; CommandLine?: string }) : undefined;
  const cmd = kind === 'bash' ? inputObj?.command ?? inputObj?.CommandLine : undefined;
  const taskDetail = shown === 'Task' ? formatTaskPrompt(it.input).detail : shown === 'AskUserQuestion' ? formatQuestionPrompt(it.input).detail : '';
  return (
    <div className={`tl tl-tool ${state}`}>
      <span className="tl-dot" />
      <div className="tl-main">
        <button className="tl-head" onClick={() => setOpen(!open)} title={it.title || it.name}>
          {isSubagentTool(it) && <span className="mt-tag">{t('subagentTag')}</span>}
          <span className="tl-name">{shown}</span>
          {arg && <span className="tl-arg">({arg})</span>}
        </button>
        <div className="tl-res">
          <span className="tl-elbow" />
          <div className="tl-res-body">{summary}</div>
        </div>
        {open && (
          <div className="tl-detail">
            {it.desktop && <DesktopShots ev={it.desktop} />}
            {kind === 'edit' && it.diffs.map((d, i) => <DiffView key={i} diff={d} />)}
            {cmd ? (
              <CopyPre text={cmd} label={t('copyCommand')} className="mono">{cmd}</CopyPre>
            ) : taskDetail ? (
              <CopyPre text={taskDetail} label={t('copyInput')} className="mono">{taskDetail}</CopyPre>
            ) : (
              hasInput && (() => {
                const rawIn = typeof it.input === 'string' ? it.input : JSON.stringify(it.input, null, 2);
                return <CopyPre text={rawIn} label={t('copyInput')} className="mono">{rawIn}</CopyPre>;
              })()
            )}
            {it.result && kind !== 'edit' && (
              <>
                <CopyPre text={out} label={t('copyOutput')} className="mono">
                  {(full ? out : out.split('\n').slice(0, 200).join('\n')) || t('noOutput')}
                </CopyPre>
                {!full && out.split('\n').length > 200 && (
                  <button className="link-btn small" onClick={() => setFull(true)}>
                    {t('showAllLines', { length: out.split('\n').length })}
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

/** Zero-width characters: models occasionally emit a long run of them; invisible, so they don't count as "a reply has arrived" */
const INVISIBLE = /[\u200b-\u200d\u2060\ufeff]/g;

/** While a turn is running with no new content, show how long we've been waiting */
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
          {starting ? t('startingTool') : t('waitingForReply')} · {sec >= 60 ? (isEn() ? `${Math.floor(sec / 60)}m ${sec % 60}s` : `${Math.floor(sec / 60)} 分 ${sec % 60} 秒`) : isEn() ? `${sec}s` : `${sec} 秒`}
          {sec >= 60 && t('theModelIsSlowPress')}
        </em>
      )}
    </div>
  );
}

/** Thinking: folded into one "✻ Thinking" line, click to see it all; shows the last few lines while streaming */
const ktok = (n: number) => (n >= 1000 ? `${(n / 1000).toFixed(n >= 10000 ? 0 : 1)}k` : String(n));

/** Usage limit line in the timeline; the countdown and buttons live in `LimitDock` above the input box */
function LimitNote({ it, live, hostId, info }: { it: Extract<Item, { k: 'limit' }>; live: boolean; hostId?: string; info?: SessionInfo }) {
  const resets = it.last.resetsAt ?? it.hit?.resetsAt;
  const tail = limitOutcome(it.last, live);
  const ok = it.last.phase === 'resumed';
  // The latest limit with no automatic continue pending (that one has the button in LimitDock): offer another tool.
  // Not after it was handed off already, or the user carried on in this session
  const settled =
    ok ||
    (it.last.phase === 'cancelled' &&
      (it.last.reason === 'handoff' ||
        it.last.reason === 'manual' ||
        it.last.reason === 'account_switched' ||
        it.last.reason === 'rate_limit_failover'));
  const offer = live && !settled && !!hostId && !!info && !info.autoResume && info.mode === 'structured';
  return (
    <div className={`note limit-note ${ok ? 'note-muted' : 'limit-note-warn'}`}>
      <Icon name="clock" size={13} />
      <span>
        {limitHead(it.hit ?? it.last, resets)}
        {tail && (isEn() ? `; ${tail}` : `，${tail}`)}
      </span>
      {offer && (
        <button className="btn small" onClick={() => handoffSession(hostId!, info!, { limit: true })}>
          {t('continueInAnotherTool')}
        </button>
      )}
    </div>
  );
}

/** Continue with another tool: in the original session where the work went, in the new one where it came from */
function HandoffNote({ ev, hostId }: { ev: HubEvent & { kind: 'handoff' }; hostId?: string }) {
  const exists = useStore((s) => !!hostId && (s.sessions[hostId] ?? []).some((x) => x.id === ev.sessionId));
  const tool = TOOL_LABEL[ev.tool] ?? ev.tool;
  return (
    <div className="note note-muted handoff-note">
      <Icon name="forward" size={13} />
      <span>{ev.to ? t('handedOffTo', { tool }) : t('tookOverFrom', { tool })}</span>
      {exists && (
        <button className="btn small" onClick={() => navigate({ kind: 'session', hostId: hostId!, sessionId: ev.sessionId })}>
          {ev.to ? t('openNewSession') : t('openOriginalSession')}
        </button>
      )}
    </div>
  );
}

/** Account switch within the same session (manual, host-active change, or automatic rate-limit failover) */
function AccountSwitchedNote({ ev }: { ev: HubEvent & { kind: 'account_switched' } }) {
  const msg =
    ev.reason === 'rate_limit'
      ? ev.fromLabel
        ? t('accountSwitchedLimit', { from: ev.fromLabel, to: ev.toLabel })
        : t('accountSwitchedLimitNoFrom', { to: ev.toLabel })
      : ev.reason === 'host_active'
        ? ev.fromLabel
          ? t('accountSwitchedHost', { from: ev.fromLabel, to: ev.toLabel })
          : t('accountSwitchedHostNoFrom', { to: ev.toLabel })
        : ev.fromLabel
          ? t('accountSwitchedManual', { from: ev.fromLabel, to: ev.toLabel })
          : t('accountSwitchedManualNoFrom', { to: ev.toLabel });
  const ctxKey =
    ev.contextCarried === 'native_resume'
      ? 'accountContextNative'
      : ev.contextCarried === 'summary_injected'
        ? 'accountContextSummary'
        : 'accountContextFresh';
  return (
    <div className="note note-muted handoff-note">
      <Icon name="key" size={13} />
      <span>{msg}</span>
      <span className="acc-badge">{t(ctxKey)}</span>
    </div>
  );
}

/** Context compaction: a spinner while running, replaced in place with the result when done (auto/manual, tokens before/after, duration) */
function CompactNote({ ev, live }: { ev: HubEvent & { kind: 'compact' }; live: boolean }) {
  if (ev.phase === 'start') {
    return (
      <div className="note note-muted compact-note">
        {live ? (
          <>
            <span className="spinner" /> {t('compactingContext')}
          </>
        ) : (
          t('compactionDidNotFinish')
        )}
      </div>
    );
  }
  if (ev.phase === 'failed') return <div className="note note-err">{t('contextCompactionFailed')}</div>;
  const how = ev.trigger === 'auto' ? t('auto') : ev.trigger === 'manual' ? t('manual') : '';
  const parts = [isEn() ? `Context compacted${how ? ` (${how})` : ''}` : `上下文已${how}压缩`];
  if (ev.preTokens != null && ev.postTokens != null) parts.push(`${ktok(ev.preTokens)} → ${ktok(ev.postTokens)} tokens`);
  else if (ev.preTokens != null) parts.push(t('tokensBefore', { v: ktok(ev.preTokens) }));
  if (ev.durationMs != null) parts.push(t('s', { v: Math.max(1, Math.round(ev.durationMs / 1000)) }));
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
          <span>{live ? t('thinking') : t('thought')}</span>
          {!open && !live && <span className="tl-arg truncate">{text.replace(/\s+/g, ' ').slice(0, 120)}</span>}
        </button>
        {(open || live) && <div className="thinking-body">{open ? text : tail}</div>}
      </div>
    </div>
  );
}

type RunItem = Item & ({ k: 'tool' } | { k: 'thinking' });
type Block = Item | { k: 'fold'; key: string; items: RunItem[] };

/** More than 8 tool calls in a row: keep the first 2 and the last 4, collapse the middle into one line */
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
      {t('moreToolCallsExpand', { n })}
    </button>
  );
}

/** Fetch images on demand and cache them; each image is requested only once */
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

export interface UserReeditTarget {
  seq: number;
  ts: number;
  rawText: string;
  images: readonly ImageRef[];
}

export async function loadSessionImageData(
  hostId: string,
  sessionId: string,
  img: ImageRef,
): Promise<{ mimeType: string; data: string; url: string }> {
  const url = await loadImage(hostId, sessionId, img);
  const comma = url.indexOf(',');
  const semi = url.indexOf(';');
  const mimeType = semi > 5 ? url.slice(5, semi) : img.mimeType;
  const data = comma >= 0 ? url.slice(comma + 1) : '';
  return { mimeType, data, url };
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
  if (failed) return <div className="msg-img msg-img-missing">{t('imageUnavailable')}</div>;
  if (!src) return <div className="msg-img msg-img-loading" />;
  return (
    <button className="msg-img" title={t('clickToZoomRightClick')} onClick={() => openImage(src)} onContextMenu={(e) => imageMenu(e, src)}>
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

/** Task list card in the timeline: collapses into one line when everything is done; click to see the details */
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
        <button className="tl-head plan-head" onClick={() => setOpen(!shown)} title={shown ? t('collapse') : t('expand')}>
          <span className="tl-name">{t('todo')}</span>
          {plan.title && <span className="plan-title truncate">{plan.title}</span>}
          <span className="tl-arg">{all ? t('done', { done, length: entries.length }) : `(${done}/${entries.length})`}</span>
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

/** A non-blocking question (Codex request_user_input_async); `answer` is the first user message after it */
export type AsyncAsk = { id: string; questions: Question[]; answer?: string };

/** The latest async question with no user message after it */
export function openAsyncAsk(events: HubEvent[]): AsyncAsk | null {
  let open: AsyncAsk | null = null;
  for (const e of events) {
    if (e.kind === 'async_question') open = { id: e.id, questions: e.questions };
    else if (e.kind === 'user_message' && e.origin !== ENGINE_ORIGIN) open = null;
  }
  return open;
}

/** Card for an async question: interactive while waiting (the answer is sent as an ordinary message), otherwise its outcome */
export function AsyncAskCard({
  ask,
  tool,
  skipped,
  onSend,
  onSkip,
  compact,
}: {
  ask: AsyncAsk;
  tool?: SessionInfo['tool'];
  skipped?: boolean;
  onSend?: (text: string) => Promise<void> | void;
  onSkip?: () => void;
  compact?: boolean;
}) {
  const name = tool ? TOOL_LABEL[tool] : 'AI';
  if (ask.answer !== undefined || skipped)
    return (
      <QuestionCard
        title={t('questionFromAi', { name })}
        questions={ask.questions}
        resolved={ask.answer !== undefined ? { optionId: 'answer', answers: asyncAnswers(ask.questions, ask.answer) } : { optionId: 'skip', answers: [] }}
      />
    );
  return (
    <QuestionCard
      title={t('aiIsWaitingForYourAnswer', { name })}
      questions={ask.questions}
      compact={compact}
      submitLabel={t('sendAnswer')}
      onSubmit={onSend ? (a) => onSend(asyncAnswer(ask.questions, a)) : undefined}
      onSkip={onSkip}
    />
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
      ? t('cancelled')
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
            title={folded ? t('expand2') : t('collapse2')}
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
      {!it.resolved && typeof cmd === 'string' && (
        <CopyPre text={cmd} label={t('copyCommand')} className="mono perm-cmd">{cmd}</CopyPre>
      )}
      {!it.resolved && it.input !== undefined && it.input !== null && typeof cmd !== 'string' && (
        <>
          <button className="link-btn small" onClick={() => setShowInput(!showInput)}>
            {showInput ? t('hideDetails') : t('showDetails')}
          </button>
          {showInput && (() => {
            const rawIn = JSON.stringify(it.input, null, 2);
            return <CopyPre text={rawIn} label={t('copyInput')} className="mono perm-cmd">{rawIn}</CopyPre>;
          })()}
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

// ---------- Compact style: consecutive thinking and tool calls fold into a group with a one-line summary title; click to see each step ----------

/** UI names for tools (used by the compact style) */
function toolUiLabel(shown: string): string {
  const en: Record<string, string> = {
    Bash: 'Command',
    PowerShell: 'Command',
    Read: 'Read file',
    Write: 'Write file',
    Update: 'Edit file',
    Grep: 'Search text',
    Glob: 'Find files',
    LS: 'List directory',
    Search: 'Search',
    'Web Search': 'Web search',
    Fetch: 'Open page',
    Task: 'Subagent',
    AskUserQuestion: 'Ask user',
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
    LS: '列出目录',
    Search: '搜索',
    'Web Search': '搜索网页',
    Fetch: '打开网页',
    Task: '子 Agent',
    AskUserQuestion: '向你提问',
    Delete: '删除文件',
    Move: '移动文件',
    Think: '思考',
  };
  return (isEn() ? en : zh)[shown] ?? shown;
}

const KIND_ICON: Record<ToolKind, string> = { bash: 'terminal', read: 'file', edit: 'edit', search: 'search', other: 'settings' };

const baseName = (p: string) => p.replace(/[\\/]+$/, '').split(/[\\/]/).pop() || p;

/** What to show for one tool call; parsing shared by both styles lives here */
function toolView(it: Item & { k: 'tool' }, cwd?: string) {
  const generic = isGenericTool(it);
  let shown = shownToolName(it);
  if (shown === 'Write' && it.diffs.some((d) => d.oldText)) shown = 'Update';
  const kind = toolKind(shown, it);
  const arg = generic || shown === t('todo') ? '' : toolArg(it, shown, cwd);
  const parsed = parseOutput(it.result?.output ?? '');
  const err = !!it.result?.isError || (parsed.exitCode ?? 0) !== 0;
  if (it.desktop) {
    const desktopErr = err || it.desktop.status === 'failed';
    return { shown, kind, arg: it.desktop.message, parsed, err: desktopErr, label: gatewayText(it.desktop).action };
  }
  // A yi_browser call before its gateway event arrives (Codex shows the row first): label it the same way
  const browserTool = browserToolName(it);
  if (browserTool) return { shown, kind, arg, parsed, err, label: getT('browserUse')('action', { tool: browserTool }) };
  const memTool = memoryToolName(it);
  if (memTool) return { shown, kind, arg: memoryToolArg(it), parsed, err, label: memoryToolLabel(memTool) };
  const sessTool = sessionToolName(it);
  if (sessTool) return { shown, kind, arg: sessionToolArg(it), parsed, err, label: sessionToolLabel(sessTool) };
  if (shown === 'ReviewDiff') return { shown, kind, arg, parsed, err, label: t('reviewToolDiff') };
  if (shown === 'ReviewScope') return { shown, kind, arg, parsed, err, label: t('reviewToolScope') };
  return { shown, kind, arg, parsed, err, label: generic ? it.title : toolUiLabel(shown) };
}

/** Shown names of the yi_review tools: reading and searching reuse Read / Grep, the diff and scope get their own */
const REVIEW_SHOWN: Record<string, string> = {
  review_read_file: 'Read',
  review_grep: 'Grep',
  review_diff: 'ReviewDiff',
  review_scope: 'ReviewScope',
};

/** The yi_review read-only tool a row calls across Claude / Codex / ACP providers. Checked before the substring
 *  guesses in `toolName`, which would read every `yi_review` name as Read because it contains "view" */
function reviewToolName(it: Item & { k: 'tool' }): string | null {
  const wrapped = (it.input as { toolName?: unknown } | null)?.toolName;
  for (const s of [it.name, typeof wrapped === 'string' ? wrapped : '', it.title]) {
    const m =
      /^(?:mcp__yi_review__|yi_review\.|mcp_yi_review_|@yi_review\/)(review_(?:scope|diff|read_file|grep))$/.exec(s ?? '') ??
      /^(review_(?:scope|diff|read_file|grep)) \(yi_review\)$/.exec(s ?? '');
    if (m) return m[1];
  }
  return null;
}

/** Key argument of a yi_review call: the path (with line range or revision), the search pattern, or the diff page */
function reviewToolArg(it: Item & { k: 'tool' }, tool: string): string {
  const inp = (it.input as Record<string, unknown> | null) ?? {};
  const inner = (inp.params as Record<string, unknown> | undefined) ?? inp;
  const str = (k: string) => (typeof inner[k] === 'string' ? (inner[k] as string) : '');
  const num = (k: string) => (typeof inner[k] === 'number' ? (inner[k] as number) : undefined);
  switch (tool) {
    case 'review_read_file': {
      const start = num('startLine');
      const end = num('endLine');
      const range = start !== undefined || end !== undefined ? `:${start ?? 1}${end !== undefined ? `-${end}` : ''}` : '';
      const rev = str('rev');
      return `${str('path')}${range}${rev ? ` @${rev}` : ''}`;
    }
    case 'review_grep': {
      const where = str('path') || str('glob');
      return where ? `${str('pattern')} · ${where}` : str('pattern');
    }
    case 'review_diff': {
      const target = str('path') || t('reviewToolAllChanges');
      const offset = num('offset');
      return offset ? `${target} · ${t('reviewToolFromOffset', { offset })}` : target;
    }
    default:
      return '';
  }
}

/** The yi_session read-only tool a row calls across Claude / Codex / ACP providers */
function sessionToolName(it: Item & { k: 'tool' }): string | null {
  const wrapped = (it.input as { toolName?: unknown } | null)?.toolName;
  for (const s of [it.name, typeof wrapped === 'string' ? wrapped : '', it.title]) {
    const m =
      /^(?:mcp__yi_session__|yi_session\.|mcp_yi_session_|@yi_session\/)?(session_(?:read|list_related))$/.exec(s ?? '') ??
      /^(session_(?:read|list_related)) \(yi_session\)$/.exec(s ?? '');
    if (m) return m[1];
  }
  return null;
}

function sessionToolLabel(tool: string): string {
  switch (tool) {
    case 'session_read':
      return t('sessionToolRead');
    case 'session_list_related':
      return t('sessionToolList');
    default:
      return tool;
  }
}

function sessionToolArg(it: Item & { k: 'tool' }): string {
  const inp = (it.input as Record<string, unknown> | null) ?? {};
  const inner = (inp.params as Record<string, unknown> | undefined) ?? inp;
  const parts: string[] = [];
  if (typeof inner.sessionId === 'string' && inner.sessionId) parts.push(inner.sessionId);
  if (typeof inner.seq === 'number') parts.push(`#${inner.seq}`);
  else if (typeof inner.view === 'string' && inner.view !== 'messages') parts.push(inner.view);
  return parts.join(' ');
}

/** The yi_memory tool a row calls across Claude / Codex / ACP providers (including Kiro bare names) */
function memoryToolName(it: Item & { k: 'tool' }): string | null {
  const wrapped = (it.input as { toolName?: unknown } | null)?.toolName;
  for (const s of [it.name, typeof wrapped === 'string' ? wrapped : '', it.title]) {
    const m =
      /^(?:mcp__yi_memory__|yi_memory\.|mcp_yi_memory_|@yi_memory\/)?(memory_(?:list|read|search|save|delete))$/.exec(s ?? '') ??
      /^(memory_(?:list|read|search|save|delete)) \(yi_memory\)$/.exec(s ?? '');
    if (m) return m[1];
  }
  return null;
}

function memoryToolLabel(tool: string): string {
  const m = getT('memory');
  switch (tool) {
    case 'memory_list':
      return m('toolList');
    case 'memory_read':
      return m('toolRead');
    case 'memory_search':
      return m('toolSearch');
    case 'memory_save':
      return m('toolSave');
    case 'memory_delete':
      return m('toolDelete');
    default:
      return tool;
  }
}

function memoryToolArg(it: Item & { k: 'tool' }): string {
  const inp = (it.input as Record<string, unknown> | null) ?? {};
  const inner = (inp.params as Record<string, unknown> | undefined) ?? inp;
  if (typeof inner.id === 'string' && inner.id) return inner.id;
  if (Array.isArray(inner.items)) {
    const ids = inner.items
      .map((x) => (x && typeof (x as { id?: unknown }).id === 'string' ? (x as { id: string }).id : ''))
      .filter(Boolean);
    if (ids.length) return ids.join(', ');
  }
  if (typeof inner.query === 'string' && inner.query) return inner.query;
  if (typeof inner.scope === 'string' && inner.scope) return inner.scope;
  return '';
}

/** The yi_browser tool a row calls, from the forms the AI tools use: mcp__yi_browser__x (Claude), yi_browser.x
 *  (Codex), "x (yi_browser)" (Qoder), DeferExecuteTool with toolName (CodeBuddy) */
function browserToolName(it: Item & { k: 'tool' }): string | null {
  const wrapped = (it.input as { toolName?: unknown } | null)?.toolName;
  for (const s of [it.name, typeof wrapped === 'string' ? wrapped : '', it.title]) {
    const m = /^(?:mcp__yi_browser__|yi_browser\.)(browser_[a-z_]+)$/.exec(s ?? '') ?? /^(browser_[a-z_]+) \(yi_browser\)$/.exec(s ?? '');
    if (m) return m[1];
  }
  return null;
}

function fmtDur(ms: number) {
  const s = Math.max(0, Math.round(ms / 1000));
  if (isEn()) return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`;
  return s < 60 ? `${s}秒` : `${Math.floor(s / 60)}分${String(s % 60).padStart(2, '0')}秒`;
}

/** Current time refreshed every second, only enabled when something needs a timer */
function useNow(on: boolean) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!on) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [on]);
  return now;
}

/** One tool call in a group: icon + localized name + key argument; click to see input and output */
const ModernTool = memo(function ModernTool({ it, active, cwd }: { it: Item & { k: 'tool' }; active: boolean; cwd?: string }) {
  useLang();
  const [open, setOpen] = useState(false);
  const v = useMemo(() => toolView(it, cwd), [it, it.result, it.title, it.input, it.diffs, it.desktop, cwd]);
  const running = !it.result && active;
  const desc = v.kind === 'read' || v.kind === 'edit' ? baseName(v.arg) : v.arg;
  const stat = useMemo(() => {
    if (v.kind !== 'edit' || !it.diffs.length) return null;
    const all = it.diffs.flatMap((d) => diffLines(d));
    return { add: all.filter((l) => l.t === 'add').length, del: all.filter((l) => l.t === 'del').length };
  }, [v.kind, it.diffs]);
  const out = v.parsed.text;
  const inputObj = it.input && typeof it.input === 'object' ? (it.input as { command?: unknown; CommandLine?: unknown }) : undefined;
  const bashCmd = v.kind === 'bash' ? inputObj?.command ?? inputObj?.CommandLine : undefined;
  const taskDetail = v.shown === 'Task' ? formatTaskPrompt(it.input).detail : v.shown === 'AskUserQuestion' ? formatQuestionPrompt(it.input).detail : '';
  const input =
    typeof bashCmd === 'string'
      ? bashCmd
      : taskDetail
        ? taskDetail
        : it.input === undefined || it.input === null || JSON.stringify(it.input) === '{}'
          ? ''
          : typeof it.input === 'string'
            ? it.input
            : JSON.stringify(it.input, null, 2);
  return (
    <div className={`mt-row ${v.err ? 'err' : ''}`}>
      <button className="mt-head" onClick={() => setOpen(!open)} title={v.arg || it.title || it.name} aria-expanded={open}>
        <span className="mt-icon">{running ? <span className="spinner mini" /> : <Icon name={v.err ? 'x' : KIND_ICON[v.kind]} size={13} />}</span>
        {isSubagentTool(it) && <span className="mt-tag">{t('subagentTag')}</span>}
        <span className="mt-name">{v.label}</span>
        {desc && <span className="mt-desc">{desc}</span>}
        {stat && (stat.add > 0 || stat.del > 0) && (
          <span className="mt-stat">
            {stat.add > 0 && <span className="add">+{stat.add}</span>}
            {stat.del > 0 && <span className="del">-{stat.del}</span>}
          </span>
        )}
        {v.err && <span className="mt-fail">{v.parsed.exitCode ? t('exit', { exitCode: v.parsed.exitCode }) : t('failed')}</span>}
        {!v.err && desktopStatus(it.desktop) && <span className="mt-desc">· {desktopStatus(it.desktop)}</span>}
      </button>
      {open && it.desktop && <DesktopShots ev={it.desktop} />}
      {open && (
        <div className="mt-detail">
          {v.kind === 'edit' && it.diffs.length > 0 ? (
            it.diffs.map((d, i) => <DiffView key={i} diff={d} />)
          ) : (
            <div className="mt-box mono">
              {input && (
                <div className="mt-sec">
                  <div className="mt-box-head">
                    <span className="mt-box-label">{t('input')}</span>
                    <InlineCopyBtn text={input} label={v.kind === 'bash' ? t('copyCommand') : t('copyInput')} size={12} />
                  </div>
                  <pre>{input}</pre>
                </div>
              )}
              {it.result ? (() => {
                const outCopy = v.parsed.tasks ? v.parsed.tasks.map((x) => `${x.done ? '☑' : '☐'} ${x.text}`).join('\n') : out;
                const outShown = v.parsed.tasks ? outCopy : out.split('\n').slice(0, 400).join('\n');
                return (
                  <div className="mt-sec">
                    <div className="mt-box-head">
                      <span className="mt-box-label">{t('output')}</span>
                      {outCopy ? <InlineCopyBtn text={outCopy} label={t('copyOutput')} size={12} /> : null}
                    </div>
                    <pre className={v.err ? 'err' : ''}>{outShown || t('noOutput')}</pre>
                  </div>
                );
              })() : (
                <div className="mt-box-label">{running ? t('running') : t('noResult2')}</div>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
});

/** One thinking block in a group: spinner or check + "thought" · first sentence; click to see it all */
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
        <span className="mt-name">{live ? t('thinking2') : t('thought2')}</span>
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

/** A group of steps: one title line (step-count box + summary + duration); expands automatically while running, collapses when done */
function ActGroup({
  items,
  running,
  end,
  liveThought,
  cwd,
}: {
  items: RunItem[];
  running: boolean;
  /** Time of the first item after this group, used to compute the duration */
  end: number | null;
  liveThought?: string;
  cwd?: string;
}) {
  useLang();
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
    } else summary = liveThought != null || items[items.length - 1]?.k === 'thinking' ? t('thinking3') : t('working');
  } else if (!tools.length) summary = t('thought2');
  else
    summary = failed
      ? t('toolCallsFailed', { length: tools.length, failed })
      : t('toolCalls', { length: tools.length });
  return (
    <div className="act-group">
      <button className="ag-head" onClick={() => setToggled(!open)} aria-expanded={open}>
        <span className={`ag-box ${open ? 'open' : ''}`}>
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

/** Compact style: merge adjacent thinking and tool calls into groups; a group ends at the time of the next item that has a timestamp */
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

/** Compact-style waiting indicator: dot animation + rotating short phrases + seconds waited */
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
      <span>{starting ? t('startingTool') : phrases[i % phrases.length]}</span>
      {sec >= 1 && <span className="m-waiting-sec">{sec >= 60 ? (isEn() ? `${Math.floor(sec / 60)}m ${sec % 60}s` : `${Math.floor(sec / 60)}分${sec % 60}秒`) : isEn() ? `${sec}s` : `${sec}秒`}</span>}
      {sec >= 60 && (
        <span className="m-waiting-sec">
          {isMobile()
            ? t('slowResponseTapStopTo')
            : t('slowResponsePressEscTo')}
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

/** Compact-style action row under each reply: left-aligned copy, quiet duration/cost metadata, and hover timestamp; detailed tokens live in the tooltip */
function ModernTurnFooter({
  it,
  latest,
  checkpoint,
  onOpenTurnDiff,
  onBringBack,
  onSelectText,
}: {
  it: Item & { k: 'turn' };
  latest: boolean;
  checkpoint?: TurnCheckpoint;
  onOpenTurnDiff?: (turn: number) => void;
  onBringBack?: (text: string) => void;
  onSelectText?: (text: string) => void;
}) {
  useLang();
  const u = it.usage;
  const parts: string[] = [];
  if (it.ms != null && it.ms >= 0) parts.push(`${t('took')} ${fmtDur(it.ms)}`);
  if (u) {
    if (u.costUsd) parts.push(`$${u.costUsd.toFixed(u.costUsd < 1 ? 3 : 2)}`);
    if (u.credits) parts.push(`${u.credits.toFixed(2)} credits`);
  }
  const tipLines: string[] = [];
  if (u) {
    tipLines.push(
      t('inputCacheReadCacheWrite', {
        inputTokens: u.inputTokens,
        cacheReadTokens: u.cacheReadTokens,
        cacheWriteTokens: u.cacheWriteTokens,
        outputTokens: u.outputTokens,
      }),
    );
  }
  if (it.model) tipLines.push(it.model);
  tipLines.push(fullTime(it.ts));
  const tip = tipLines.join('\n');
  return (
    <div className={`m-actions ${latest ? 'latest' : ''}`}>
      {it.reason && <span className="attn-text">{it.reason}</span>}
      {it.text && (
        <>
          {onBringBack && (
            <button
              type="button"
              className="turn-bring-btn"
              title={t('bringBackThisReply')}
              onClick={() => onBringBack(it.text)}
            >
              <Icon name="back" size={12} />
              <span>{t('bringBackThisReply')}</span>
            </button>
          )}
          <button
            className="m-act-btn"
            title={t('copyThisReply')}
            aria-label={t('copyThisReply')}
            onClick={async () => toast((await writeClipboard(it.text)) ? t('copied') : t('copyFailed'))}
          >
            <Icon name="copy" size={14} />
          </button>
          {onSelectText && (
            <button
              type="button"
              className="m-act-btn mobile-select-btn"
              title={t('selectText')}
              aria-label={t('selectText')}
              onClick={() => onSelectText(it.text)}
            >
              <Icon name="text" size={14} />
            </button>
          )}
        </>
      )}
      {parts.length > 0 && (
        <span className="m-act-meta" title={tip}>
          {parts.join(' · ')}
        </span>
      )}
      {checkpoint && checkpoint.filesChanged > 0 && checkpoint.endRef && onOpenTurnDiff && (
        <button type="button" className="turn-ckpt-btn" title={t('viewTurnDiff')} onClick={() => onOpenTurnDiff(checkpoint.turn)}>
          <span>{t('turnFilesChanged', { count: checkpoint.filesChanged })}</span>
          <span className="ls-a">+{checkpoint.added}</span>
          <span className="ls-d">−{checkpoint.deleted}</span>
        </button>
      )}
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
  jumpReq,
  onJumpDone,
  promptSeqs,
  onPromptScrollState,
  pendingOutside,
  skippedAsks,
  onOpenTurnDiff,
  onBringBack,
  onReedit,
}: {
  events: HubEvent[];
  live: Map<string, string>;
  liveThinking?: Map<string, string>;
  info?: SessionInfo;
  /** Needed to show images in user messages */
  hostId?: string;
  onAnswer: (requestId: string, optionId: string, answers?: QuestionAnswer[]) => void | Promise<void>;
  /** yi-agentd still has older events we haven't fetched */
  hasMore?: boolean;
  loadingOlder?: boolean;
  /** Fetching the session log from the host (first open, reload) */
  loading?: boolean;
  onLoadOlder?: (targetSeq?: number) => void;
  /** Scroll to this message and highlight it (jumped here from global search) */
  focusSeq?: number;
  /** Trigger an explicit jump/flash to any message seq (supports repeated jumps to the same seq) */
  jumpReq?: { seq: number; nonce: number } | null;
  onJumpDone?: () => void;
  /** Ordered seqs of all user prompts in the session (for tracking active prompt when scrolled above loaded bubbles) */
  promptSeqs?: readonly number[];
  /** Called when the visible current prompt or sticky bar visibility changes */
  onPromptScrollState?: (state: { activeSeq: number | null; stickyVisible: boolean }) => void;
  /** Pending permission requests are already shown elsewhere (above the input box) */
  pendingOutside?: boolean;
  /** Async questions (by id) the user skipped */
  skippedAsks?: string[];
  /** Open the file panel scoped to a completed turn's checkpoint diff */
  onOpenTurnDiff?: (turn: number) => void;
  /** Optional "bring back to main chat" action on each completed assistant reply (used inside side chats) */
  onBringBack?: (text: string) => void;
  /** Refill a past user message (text, context refs, and images) into the session composer for re-editing */
  onReedit?: (msg: UserReeditTarget) => void;
}) {
  const hideCompactCmd = info?.tool === 'claude' || info?.tool === 'codex' || info?.tool === 'cursor' || info?.tool === 'antigravity';
  const [, lang] = useLang();
  const [copiedUserKey, setCopiedUserKey] = useState<string | null>(null);
  const copyUserTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(
    () => () => {
      if (copyUserTimer.current) clearTimeout(copyUserTimer.current);
    },
    [],
  );
  const copyUserMsg = async (msgKey: string, text: string) => {
    const ok = await writeClipboard(text);
    toast(ok ? t('copied') : t('copyFailed'));
    if (!ok) return;
    setCopiedUserKey(msgKey);
    if (copyUserTimer.current) clearTimeout(copyUserTimer.current);
    copyUserTimer.current = setTimeout(() => setCopiedUserKey(null), 1500);
  };
  const items = useMemo(() => buildItems(events, hideCompactCmd), [events, events.length, hideCompactCmd, lang]);
  const ckptByEndSeq = useMemo(() => {
    const byEnd = new Map<number, TurnCheckpoint>();
    for (const c of info?.checkpoints ?? []) {
      if (c.turnEndSeq != null) byEnd.set(c.turnEndSeq, c);
    }
    return byEnd;
  }, [info?.checkpoints]);
  const [limit, setLimit] = useState(PAGE);
  const ref = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const liveText = [...live.values()].join('').replace(INVISIBLE, '');
  const liveThought = [...(liveThinking?.values() ?? [])].join('');
  const shown = items.slice(Math.max(0, items.length - limit));
  const modern = useChatStyle() === 'modern';
  const blocks: (Block | MBlock)[] = useMemo(() => {
    // Task-type tool calls are shown by the task list instead; failed ones are shown as usual
    const list = shown.filter((x) => !(x.k === 'tool' && isPlanTool(x.name, x.input) && !x.result?.isError));
    return modern ? groupRuns(list) : foldRuns(list);
  }, [items, limit, modern]);
  // Only tools in the last turn (which has no turn end yet) can still be running
  const lastLimitKey = useMemo(() => {
    for (let i = items.length - 1; i >= 0; i--) if (items[i].k === 'limit') return items[i].key;
    return null;
  }, [items]);
  const lastTurnEnd = useMemo(() => shown.map((x) => x.k).lastIndexOf('turn'), [shown]);
  const liveKeys = useMemo(() => new Set(shown.slice(lastTurnEnd + 1).map((x) => x.key)), [shown, lastTurnEnd]);
  const busy = info && (info.status === 'running' || info.status === 'starting');
  // When new content last arrived (in local time, unaffected by the host's clock skew)
  const lastChange = useRef(Date.now());
  const changeKey = `${events.length}:${liveText.length}:${liveThought.length}:${busy}`;
  const prevKey = useRef(changeKey);
  if (prevKey.current !== changeKey) {
    prevKey.current = changeKey;
    lastChange.current = Date.now();
  }

  // When paging back, remember the content height before, and restore the scroll position after inserting older messages so the view doesn't jump
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
  const [selectingText, setSelectingText] = useState<string | null>(null);
  const stickyShownRef = useRef(false);
  const lastPromptReport = useRef<{ activeSeq: number | null; stickyVisible: boolean }>({
    activeSeq: null,
    stickyVisible: false,
  });
  const promptStateCb = useRef(onPromptScrollState);
  promptStateCb.current = onPromptScrollState;
  const promptSeqsRef = useRef(promptSeqs);
  promptSeqsRef.current = promptSeqs;
  const shownLenRef = useRef(0);
  shownLenRef.current = shown.length;
  const promptRaf = useRef<number | null>(null);

  const computePromptScroll = () => {
    const cb = promptStateCb.current;
    const box = ref.current;
    if (!cb || !box) return;
    const containerTop = box.getBoundingClientRect().top;
    const nodes = box.querySelectorAll<HTMLElement>('[data-prompt-seq]');
    const allSeqs = promptSeqsRef.current ?? [];
    let activeSeq: number | null = null;
    let activeNode: HTMLElement | null = null;

    if (nodes.length > 0) {
      let chosen: HTMLElement | null = null;
      for (const n of nodes) {
        if (n.getBoundingClientRect().top <= containerTop + 80) chosen = n;
        else break;
      }
      if (chosen) {
        activeNode = chosen;
        activeSeq = Number(chosen.dataset.promptSeq);
      } else {
        const firstSeq = Number(nodes[0].dataset.promptSeq);
        let earlierSeq: number | null = null;
        for (const s of allSeqs) {
          if (s < firstSeq) earlierSeq = s;
          else break;
        }
        if (earlierSeq != null) {
          activeSeq = earlierSeq;
        } else {
          activeNode = nodes[0];
          activeSeq = firstSeq;
        }
      }
    } else if (allSeqs.length > 0 && shownLenRef.current > 0) {
      activeSeq = allSeqs[allSeqs.length - 1];
    }

    let stickyVisible = false;
    if (activeSeq != null) {
      if (!activeNode) {
        stickyVisible = shownLenRef.current > 0;
      } else {
        const bottom = activeNode.getBoundingClientRect().bottom;
        stickyVisible = stickyShownRef.current ? bottom < containerTop + 44 : bottom < containerTop + 16;
      }
    }
    stickyShownRef.current = stickyVisible;
    const prev = lastPromptReport.current;
    if (prev.activeSeq !== activeSeq || prev.stickyVisible !== stickyVisible) {
      lastPromptReport.current = { activeSeq, stickyVisible };
      cb({ activeSeq, stickyVisible });
    }
  };

  const schedulePromptScroll = () => {
    if (!promptStateCb.current || promptRaf.current != null) return;
    promptRaf.current = requestAnimationFrame(() => {
      promptRaf.current = null;
      computePromptScroll();
    });
  };

  useEffect(
    () => () => {
      if (promptRaf.current != null) cancelAnimationFrame(promptRaf.current);
    },
    [],
  );

  const toBottom = (smooth = false) => {
    const el = ref.current;
    if (!el) return;
    stick.current = true;
    el.scrollTo({ top: el.scrollHeight, behavior: smooth ? 'smooth' : 'auto' });
    schedulePromptScroll();
  };
  const onScroll = () => {
    const el = ref.current;
    if (!el) return;
    // While older messages are loading, the layout effect restores keepFrom on every render
    // (streaming deltas, prompt-state updates, `away` toggles). Track the user's own scrolling
    // here, otherwise each render snaps the view back to where paging started until the
    // history request returns (slow over SSH / relay), and scrolling down appears stuck.
    if (keepFrom.current != null) keepFrom.current = el.scrollHeight - el.scrollTop;
    const gap = el.scrollHeight - el.scrollTop - el.clientHeight;
    stick.current = gap < 80;
    const far = gap > el.clientHeight * 0.6;
    if (far !== away) setAway(far);
    if (el.scrollTop < 40 && canOlder && !loadingOlder && keepFrom.current == null) older();
    schedulePromptScroll();
  };

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    if (keepFrom.current != null) {
      el.scrollTop = el.scrollHeight - keepFrom.current;
      if (!loadingOlder) keepFrom.current = null;
    } else if (stick.current && !selectingIn(el, false)) el.scrollTop = el.scrollHeight;
  });

  // When the chat area gets shorter (mobile IME opens, input box grows), stay pinned to the bottom if we were there
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const ro = new ResizeObserver(() => {
      if (stick.current && keepFrom.current == null && !selectingIn(el, false)) el.scrollTop = el.scrollHeight;
      schedulePromptScroll();
    });
    ro.observe(el);
    const diagramResize = () => {
      if (stick.current && keepFrom.current == null && !selectingIn(el, false)) el.scrollTop = el.scrollHeight;
      schedulePromptScroll();
    };
    el.addEventListener('yi-diagram-resize', diagramResize);
    return () => { ro.disconnect(); el.removeEventListener('yi-diagram-resize', diagramResize); };
  }, []);

  // A message we just sent: go back to the bottom wherever we are
  const lastUser = useMemo(() => {
    for (let i = items.length - 1; i >= 0; i--) if (items[i].k === 'user') return items[i].key;
    return '';
  }, [items]);
  const seenUser = useRef(lastUser);
  useEffect(() => {
    if (lastUser && seenUser.current && lastUser !== seenUser.current) toBottom();
    seenUser.current = lastUser;
  }, [lastUser]);

  const seenJumpNonce = useRef(0);
  const handledJumpNonce = useRef(0);
  const jumpStartedLoaded = useRef(true);

  useEffect(() => {
    stick.current = true;
    setLimit(PAGE);
    setSelectingText(null);
    seenJumpNonce.current = 0;
    handledJumpNonce.current = 0;
    jumpStartedLoaded.current = true;
    stickyShownRef.current = false;
    lastPromptReport.current = { activeSeq: null, stickyVisible: false };
    onPromptScrollState?.({ activeSeq: null, stickyVisible: false });
  }, [info?.id]);

  useEffect(() => {
    schedulePromptScroll();
  }, [blocks, promptSeqs, info?.id]);

  // Jump to a given message: if it isn't loaded yet, keep fetching older ones until we find it or reach the start
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
      onLoadOlder(focusSeq);
    } else if (items.length > shown.length) setLimit(Infinity);
  });

  // Explicit prompt jump (from outline, tick rail, sticky bar, or Alt+Shift+Up/Down)
  useEffect(() => {
    if (!jumpReq) return;
    if (seenJumpNonce.current !== jumpReq.nonce) {
      seenJumpNonce.current = jumpReq.nonce;
      jumpStartedLoaded.current = !!ref.current?.querySelector(`[data-key="e${jumpReq.seq}"]`);
    }
    if (handledJumpNonce.current === jumpReq.nonce) return;
    stick.current = false;
    keepFrom.current = null;
    const box = ref.current;
    const el = box?.querySelector(`[data-key="e${jumpReq.seq}"]`) as HTMLElement | null;
    if (el && box) {
      handledJumpNonce.current = jumpReq.nonce;
      const top = el.getBoundingClientRect().top - box.getBoundingClientRect().top + box.scrollTop - 18;
      box.scrollTo({ top: Math.max(0, top), behavior: jumpStartedLoaded.current ? 'smooth' : 'auto' });
      el.classList.remove('flash');
      void el.offsetWidth;
      el.classList.add('flash');
      setTimeout(() => el.classList.remove('flash'), 2400);
      jumpStartedLoaded.current = true;
      onJumpDone?.();
      schedulePromptScroll();
      return;
    }
    if (items.some((x) => x.key === `e${jumpReq.seq}`)) {
      jumpStartedLoaded.current = false;
      setLimit(Infinity);
      return;
    }
    const first = events.find((e) => e.seq > 0)?.seq ?? Infinity;
    if (first > jumpReq.seq && hasMore && !loadingOlder && onLoadOlder) {
      jumpStartedLoaded.current = false;
      setLimit(Infinity);
      onLoadOlder(jumpReq.seq);
    } else if (!hasMore && !loadingOlder) {
      handledJumpNonce.current = jumpReq.nonce;
      jumpStartedLoaded.current = true;
      onJumpDone?.();
    }
  });

  // Compact style: the last group counts as running (expanded, timed) while it's in this turn and the reply body hasn't started
  const lastBlock = blocks[blocks.length - 1];
  const tailGroup = modern && lastBlock?.k === 'group' && !!busy && !liveText.trim() && liveKeys.has(lastBlock.items[0].key) ? lastBlock : null;
  const lastTurnKey = useMemo(() => {
    for (let i = blocks.length - 1; i >= 0; i--) if (blocks[i].k === 'turn') return blocks[i].key;
    return '';
  }, [blocks]);


  const host = useStore((st) => (hostId ? st.hosts.find((h) => h.config.id === hostId)?.config : undefined));
  const desktopCtx = useMemo(() => (hostId && info?.id ? { hostId, sessionId: info.id } : null), [hostId, info?.id]);
  const imgCtx = useMemo<ImgCtx | null>(
    () => (hostId ? { hostId, hostName: host?.name ?? hostId, local: host?.kind === 'local', cwd: info?.cwd, sessionId: info?.id } : null),
    [hostId, host?.name, host?.kind, info?.cwd, info?.id],
  );
  const runItem = (x: RunItem) =>
    x.k === 'tool' ? (
      <ToolLine key={x.key} it={x} active={!!busy && liveKeys.has(x.key)} cwd={info?.cwd} />
    ) : (
      <Thinking key={x.key} text={x.text} />
    );

  return (
    <ImgContext.Provider value={imgCtx}>
    <DesktopCtx.Provider value={desktopCtx}>
    <div
      className={`chat ${modern ? 'modern' : ''}`}
      ref={ref}
      onScroll={onScroll}
      onClick={(e) => {
        // Click an image in a reply to enlarge it
        const t = e.target as HTMLElement;
        if (t.tagName === 'IMG' && t.closest('.md') && !t.closest('.mermaid-block')) openImage((t as HTMLImageElement).src);
      }}
      onContextMenu={(e) => {
        const t = e.target as HTMLElement;
        if (t.tagName === 'IMG' && t.closest('.md') && !t.closest('.mermaid-block')) imageMenu(e, (t as HTMLImageElement).src);
      }}
    >
      <div className="chat-inner">
        {loading && (
          <div className={`chat-loading ${events.length ? 'top' : ''}`} role="status">
            <span className="spinner" />
            {t('fetchingMessagesFromHost')}
          </div>
        )}
        {canOlder && (
          <button className="link-btn small center" disabled={loadingOlder} onClick={older}>
            {loadingOlder ? t('loadingEarlierMessages') : t('showEarlierMessages')}
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
            case 'user': {
              if (it.origin === ENGINE_ORIGIN)
                return (
                  <div key={it.key} data-key={it.key} className="msg-engine">
                    <EngineNote text={it.text} />
                  </div>
                );
              const parsed = parseContextMessage(it.text);
              const followUp = parseFindingFollowUp(parsed.text);
              const shownText = followUp.text;
              const canReedit = !!onReedit && !!(shownText.trim() || parsed.refs.length > 0 || it.images.length > 0);
              const isUserCopied = copiedUserKey === it.key;
              const userActionButtons = (extraCls = '') => (
                <>
                  {canReedit && (
                    <button
                      type="button"
                      className={`m-act-btn ${extraCls}`.trim()}
                      title={t('reeditMessage')}
                      aria-label={t('reeditMessage')}
                      onClick={() => onReedit!({ seq: it.seq, ts: it.ts, rawText: it.text, images: it.images })}
                    >
                      <Icon name="reply" size={13} />
                    </button>
                  )}
                  {shownText.trim() && (
                    <>
                      <button
                        type="button"
                        className={`m-act-btn ${isUserCopied ? 'copied' : ''} ${extraCls}`.trim()}
                        title={isUserCopied ? t('copied') : t('copyMessage')}
                        aria-label={isUserCopied ? t('copied') : t('copyMessage')}
                        onClick={() => void copyUserMsg(it.key, shownText)}
                      >
                        <Icon name={isUserCopied ? 'check' : 'copy'} size={13} />
                      </button>
                      <button
                        type="button"
                        className={`m-act-btn mobile-select-btn ${extraCls}`.trim()}
                        title={t('selectText')}
                        aria-label={t('selectText')}
                        onClick={() => setSelectingText(shownText)}
                      >
                        <Icon name="text" size={13} />
                      </button>
                    </>
                  )}
                </>
              );
              return (
                <div key={it.key} data-key={it.key} data-prompt-seq={it.seq} className="msg-user">
                  {it.images.length > 0 && hostId && info && (
                    <div className="msg-imgs">
                      {it.images.map((img) => (
                        <MsgImage key={img.id} hostId={hostId} sessionId={info.id} img={img} />
                      ))}
                    </div>
                  )}
                  {parsed.refs.length > 0 && (
                    <ContextRefBar refs={parsed.refs} hostId={hostId} sessionId={info?.id} cwd={info?.cwd} className="msg-refs" />
                  )}
                  {followUp.ref && (
                    <div className="msg-review-ref">
                      <Icon name="sparkles" size={12} />
                      {followUp.ref.file && (
                        <span className="mono msg-review-ref-loc">
                          {followUp.ref.file}
                          {followUp.ref.line ? `:${followUp.ref.line}` : ''}
                        </span>
                      )}
                      {followUp.ref.title && <span className="truncate">{followUp.ref.title}</span>}
                    </div>
                  )}
                  {shownText.trim() && <div className="bubble">{shownText}</div>}
                  {modern ? (
                    <div className="msg-user-actions">
                      <span className="msg-time">{fullTime(it.ts)}</span>
                      {userActionButtons()}
                    </div>
                  ) : (
                    <div className="msg-user-actions classic">
                      <span className="msg-time">{clock(it.ts)}</span>
                      {userActionButtons('classic-act')}
                    </div>
                  )}
                </div>
              );
            }
            case 'text':
              if (it.ask) {
                // Async question: waiting ones are pinned above the input box; answered or skipped ones show here as a card
                const skipped = skippedAsks?.includes(it.ask.id);
                if (it.ask.answer !== undefined || skipped)
                  return (
                    <div key={it.key} data-key={it.key}>
                      <AsyncAskCard ask={it.ask} tool={info?.tool} skipped={skipped} />
                    </div>
                  );
                if (pendingOutside) return null;
              }
              return (
                <div key={it.key} data-key={it.key} className="tl tl-text msg-assistant">
                  <span className="tl-dot" />
                  <Markdown text={it.text} />
                </div>
              );
            case 'perm':
              // Pending permissions are pinned above the input box by the parent; don't repeat them here
              if (!it.resolved && pendingOutside) return null;
              return <PermissionCard key={it.key} it={it} onAnswer={onAnswer} />;
            case 'plan':
              return <Plan key={it.key} plan={it.plan} busy={!!busy} />;
            case 'turn': {
              const ckpt = ckptByEndSeq.get(it.seq);
              return modern ? (
                <ModernTurnFooter
                  key={it.key}
                  it={it}
                  latest={it.key === lastTurnKey}
                  checkpoint={ckpt}
                  onOpenTurnDiff={onOpenTurnDiff}
                  onBringBack={onBringBack}
                  onSelectText={setSelectingText}
                />
              ) : (
                <TurnFooter
                  key={it.key}
                  it={it}
                  checkpoint={ckpt}
                  onOpenTurnDiff={onOpenTurnDiff}
                  onBringBack={onBringBack}
                  onSelectText={setSelectingText}
                />
              );
            }
            case 'bgdone':
              return <BgTaskNote key={it.key} it={it} />;
            case 'desktop':
              return <DesktopAction key={it.key} ev={it.ev} hostId={hostId} sessionId={info?.id} />;
            case 'note':
              return (
                <div key={it.key} className={`note note-${it.tone}`}>
                  {it.text}
                </div>
              );
            case 'compact':
              // Kiro compacts after its /compact turn has ended, while the session is idle
              return <CompactNote key={it.key} ev={it.ev} live={!!busy || (info?.tool === 'kiro' && info.status === 'idle')} />;
            case 'limit':
              return <LimitNote key={it.key} it={it} live={it.key === lastLimitKey} hostId={hostId} info={info} />;
            case 'handoff':
              return <HandoffNote key={it.key} ev={it.ev} hostId={hostId} />;
            case 'account_switched':
              return <AccountSwitchedNote key={it.key} ev={it.ev} />;
            case 'memory':
              return <MemoryEventCard key={it.key} ev={it.ev} hostId={hostId} cwd={info?.cwd} />;
          }
        })}
        {liveThought && !liveText && !modern && <Thinking text={liveThought} live />}
        {liveThought && !liveText && modern && !tailGroup && <ActGroup key="live-think" items={[]} running end={null} liveThought={liveThought} />}
        {liveText.trim() && <LiveMarkdown text={liveText} />}
        {busy && !liveText.trim() && !liveThought && !modern && <Waiting key={lastChange.current} since={lastChange.current} starting={info?.status === 'starting'} />}
        {busy && !liveText.trim() && !liveThought && modern && !tailGroup?.items.some((x) => x.k === 'tool' && !x.result) && (
          <ModernWaiting key={lastChange.current} since={lastChange.current} starting={info?.status === 'starting'} />
        )}
        {items.length === 0 && !busy && !loading && <div className="empty">{t('sendAMessageToStart')}</div>}
        {away && (
          <button className="jump-bottom" title={t('jumpToBottom')} onClick={() => toBottom(true)}>
            <Icon name="arrow-down" size={16} />
            {busy && <span className="jump-live" />}
          </button>
        )}
      </div>
    </div>
    {selectingText !== null && createPortal(<SelectTextModal text={selectingText} onClose={() => setSelectingText(null)} />, document.body)}
    </DesktopCtx.Provider>
    </ImgContext.Provider>
  );
}

/** Background-task finished notice: icon + status + one-line command (ellipsized if too long, click to see it all) */
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

function DesktopAction({ ev, hostId, sessionId }: { ev: GatewayEvent; hostId?: string; sessionId?: string }) {
  useLang();
  const x = gatewayText(ev);
  const status = x.status[ev.status] ?? x.status.interrupted;
  return <div className="computer-use-action">
    <div className="computer-use-action-head"><Icon name={ev.kind === 'browser_use' ? 'globe' : 'laptop'} size={15} /><strong>{x.action}</strong><span className="muted small">{status}</span></div>
    {ev.message && <div className="muted small truncate">{ev.message}</div>}
    {!!ev.durationMs && <div className="muted small">{x.elapsed}</div>}
    {!!ev.images?.length && hostId && sessionId && <div className="computer-use-images">{ev.images.map((img) => <MsgImage key={img.id} hostId={hostId} sessionId={sessionId} img={img} />)}</div>}
  </div>;
}

function MemoryEventCard({ ev, hostId, cwd }: { ev: MemoryEvent; hostId?: string; cwd?: string }) {
  useLang();
  const m = getT('memory');
  const [state, setState] = useState<'idle' | 'reverted' | 'approved' | 'rejected'>('idle');
  const [busy, setBusy] = useState(false);
  const [editor, setEditor] = useState<{ kind: 'edit' | 'review'; initial: MemoryEditorInit } | null>(null);

  const scopeLabel =
    ev.scope === 'global'
      ? m('scopeGlobalShort')
      : ev.projectName
        ? m('scopeProjectNamed', { name: ev.projectName })
        : m('scopeProjectShort');
  const isPending = ev.action === 'pending_save' || ev.action === 'pending_delete';
  const heading =
    ev.action === 'deleted'
      ? m('timelineDeleted', { scope: scopeLabel, title: ev.title || ev.id })
      : ev.action === 'pending_save'
        ? m('timelinePendingSave', { scope: scopeLabel, title: ev.title || ev.id })
        : ev.action === 'pending_delete'
          ? m('timelinePendingDelete', { scope: scopeLabel, title: ev.title || ev.id })
          : m('timelineSaved', { scope: scopeLabel, title: ev.title || ev.id });

  const undo = async () => {
    if (!hostId || !ev.undoId) return;
    setBusy(true);
    try {
      await rpc(hostId, 'memory.undo', { undoId: ev.undoId });
      setState('reverted');
      toast(m('undone'));
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const resolvePending = async (approve: boolean) => {
    if (!hostId || !ev.pendingId) return;
    setBusy(true);
    try {
      await rpc(hostId, 'memory.resolvePending', { pendingId: ev.pendingId, approve });
      setState(approve ? 'approved' : 'rejected');
      toast(approve ? m('approved') : m('rejected'));
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const openEdit = async () => {
    if (!hostId) return;
    setBusy(true);
    try {
      if (isPending && ev.pendingId) {
        const list = await rpc<MemoryListResponse>(hostId, 'memory.list', { cwd });
        const found = list.pending.find((x) => x.pendingId === ev.pendingId);
        if (!found) {
          toast(m('timelineRejected'));
          return;
        }
        setEditor({
          kind: 'review',
          initial: {
            scope: found.scope,
            projectKey: found.projectKey,
            cwd,
            id: found.id,
            title: found.title,
            description: found.description,
            mode: found.mode,
            tags: found.tags,
            content: found.content,
            pendingId: found.pendingId,
          },
        });
      } else {
        const entry = await rpc<MemoryEntry>(hostId, 'memory.get', {
          scope: ev.scope,
          projectKey: ev.projectKey ?? undefined,
          cwd,
          id: ev.id,
        });
        setEditor({
          kind: 'edit',
          initial: {
            scope: entry.scope,
            projectKey: entry.projectKey,
            cwd,
            id: entry.id,
            prevId: entry.id,
            title: entry.title,
            description: entry.description,
            mode: entry.mode,
            tags: entry.tags ?? [],
            content: entry.content,
          },
        });
      }
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className={`mem-timeline-card ${isPending && state === 'idle' ? 'pending' : ''}`}>
      <div className="mem-timeline-head">
        <Icon name="book" size={14} />
        <strong className="small">{heading}</strong>
        <code className="muted small">{ev.id}</code>
        {state === 'reverted' && <span className="acc-badge acc-badge-muted">{m('timelineReverted')}</span>}
        {state === 'approved' && <span className="acc-badge acc-badge-ok">{m('timelineApproved')}</span>}
        {state === 'rejected' && <span className="acc-badge acc-badge-muted">{m('timelineRejected')}</span>}
      </div>
      {ev.description && <div className="muted small">{ev.description}</div>}
      {state === 'idle' && hostId && (
        <div className="mem-timeline-actions">
          {isPending ? (
            <>
              <button className="btn small primary" disabled={busy} onClick={() => resolvePending(true)}>
                {m('approve')}
              </button>
              {ev.action === 'pending_save' && (
                <button className="btn small" disabled={busy} onClick={openEdit}>
                  {m('editAndApprove')}
                </button>
              )}
              <button className="btn small" disabled={busy} onClick={() => resolvePending(false)}>
                {m('reject')}
              </button>
            </>
          ) : (
            <>
              {ev.action === 'saved' && (
                <button className="btn small" disabled={busy} onClick={openEdit}>
                  {m('edit')}
                </button>
              )}
              {ev.undoId && (
                <button className="btn small" disabled={busy} onClick={undo}>
                  {m('undo')}
                </button>
              )}
            </>
          )}
        </div>
      )}
      {editor && hostId &&
        createPortal(
          <MemoryEditorModal
            hostId={hostId}
            kind={editor.kind}
            initial={editor.initial}
            projects={[]}
            cwd={cwd}
            onSaved={() => {
              if (editor.kind === 'review') setState('approved');
            }}
            onClose={() => setEditor(null)}
          />,
          document.body,
        )}
    </div>
  );
}
