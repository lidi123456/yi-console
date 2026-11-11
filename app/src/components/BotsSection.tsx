// Sidebar "Bots" sections: this app's Bots (desktop app) and those of each desktop app reachable through a host
// (remote control from a phone or yi-web), each Bot with its tasks. Their Bot and task sessions are listed here
// instead of under their host (`isListedBotSession`). Host ids in a remote desktop's data are its own; `openOn`
// maps them to this client's hosts.

import { createContext, useEffect, useState } from 'react';
import { getT, isEn, useLang } from '../i18n';
import { errorToast, navigate, openNewSession, toast, useStore, getState, type HostEntry } from '../store';
import { lsGet, lsSet } from '../util';
import { ACTIVE_TASK, botCall, mapHost, unmapHost, useBotCall, refreshRemote, sourceOf, useBotList, useBots, type Approval, type BotEntry, type SecretCard, type EventRow, type TaskRow } from '../bots';
import { openUrl } from './PortsView';
import { fmtTime } from './BotActivity';
import { askConfirm, openMenu, type MenuItem } from './Overlay';
import { Icon, ToolBadge } from './Icon';
import type { SessionInfo } from '../types';

const t = getT('bots');

/** Things waiting for the owner: calls for attention, drafts to approve and secrets to enter */
export function needsYouCount(b: BotEntry) {
  return b.attention.length + (b.approvals?.length ?? 0) + (b.secrets?.length ?? 0);
}

/** Sidebar dot class for a Bot state */
export function botDot(b: BotEntry) {
  if (needsYouCount(b) > 0) return 'attn';
  switch (b.state) {
    case 'running':
    case 'starting':
    case 'compacting':
      return 'busy';
    case 'error':
      return 'err';
    default:
      return null;
  }
}

export function taskDot(state: string) {
  switch (state) {
    case 'needsYou':
      return 'attn';
    case 'working':
      return 'busy';
    case 'failed':
      return 'err';
    case 'ready':
      return 'ok';
    default:
      return null;
  }
}

export function taskStateLabel(state: string) {
  switch (state) {
    case 'queued':
      return t('stateQueued');
    case 'working':
      return t('stateWorking');
    case 'needsYou':
      return t('stateNeedsYou');
    case 'ready':
      return t('stateReady');
    case 'failed':
      return t('stateFailed');
    case 'done':
      return t('stateDone');
    default:
      return t('stateStopped');
  }
}

export function botStateLabel(state: string) {
  switch (state) {
    case 'disabled':
      return t('botDisabled');
    case 'starting':
      return t('botStarting');
    case 'running':
      return t('botRunning');
    case 'compacting':
      return t('botCompacting');
    case 'error':
      return t('botError');
    default:
      return t('botIdle');
  }
}

const run = (p: Promise<unknown>) => p.catch(errorToast);

/** Open a session of a Bot source; a remote desktop's host may not be reachable from here */
// Hosts as the desktop that runs the Bot sees them: this app's own list, or the remote desktop's via bots.hosts
export const SourceHosts = createContext<HostEntry[]>([]);

export function useSourceHosts(src: string): HostEntry[] {
  const own = useStore((s) => s.hosts);
  const call = useBotCall();
  const [remote, setRemote] = useState<HostEntry[]>([]);
  useEffect(() => {
    if (!src) return;
    call<{ hosts: HostEntry[] }>('bots.hosts')
      .then((r) => setRemote(r.hosts))
      .catch(errorToast);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [src]);
  return src ? remote : own;
}

export function openOn(src: string, host: string, sessionId: string) {
  const hostId = mapHost(src, host, (id) => getState().hosts.some((h) => h.config.id === id));
  if (!hostId) {
    toast(t('hostNotShared'), 'error');
    return;
  }
  navigate({ kind: 'session', hostId, sessionId });
}

/** This client's id for a host in a source's data (for the "active" highlight); null when unreachable */
export function localHost(src: string, host: string) {
  return mapHost(src, host, (id) => getState().hosts.some((h) => h.config.id === id));
}

/** After a Bot is deleted: go home if the page shows it, its session or one of its tasks' sessions */
export function leaveBotPages(b: BotEntry) {
  const v = getState().view;
  const sessions = new Set([b.session?.sessionId, ...b.tasks.map((x) => x.sessionId)].filter(Boolean));
  if (('botId' in v && v.botId === b.config.id) || ('sessionId' in v && sessions.has(v.sessionId))) navigate({ kind: 'home' });
}

export function botMenu(b: BotEntry, src = ''): (MenuItem | '-')[] {
  const id = b.config.id;
  return [
    {
      label: t('openBotSession'),
      icon: 'chat',
      disabled: !b.session,
      run: () => b.session && openOn(src, b.session.host, b.session.sessionId),
    },
    { label: t('botDetails'), icon: 'eye', run: () => navigate({ kind: 'bot', botId: id, src }) },
    { label: t('eventLog'), icon: 'history', run: () => navigate({ kind: 'botEvents', botId: id, src }) },
    '-',
    b.config.enabled
      ? { label: t('disable'), icon: 'pause', run: () => run(botCall('bots.setEnabled', { id, enabled: false }, src)) }
      : { label: t('enable'), icon: 'play', run: () => run(botCall('bots.setEnabled', { id, enabled: true }, src)) },
    { label: t('restartSession'), icon: 'refresh', disabled: !b.config.enabled, run: () => run(botCall('bots.restart', { id }, src)) },
    ...(b.attention.length > 0
      ? [{ label: t('clearAttention'), icon: 'check', run: () => run(botCall('bots.resolve', { id: null, taskId: null, botId: id }, src)) }]
      : []),
    '-',
    {
      label: t('deleteBot'),
      icon: 'trash',
      danger: true,
      run: async () => {
        if (!(await askConfirm({ title: t('deleteBotTitle', { name: b.config.name }), message: t('deleteBotMessage'), danger: true })))
          return;
        try {
          await botCall('bots.delete', { id, sessions: false }, src);
          leaveBotPages(b);
        } catch (e) {
          errorToast(e);
        }
      },
    },
  ];
}

export function taskMenu(task: TaskRow, src = ''): (MenuItem | '-')[] {
  const id = task.id;
  const active = ACTIVE_TASK.includes(task.state);
  return [
    ...(active
      ? [
          ...(task.state === 'needsYou' && !task.manual && task.lastReport?.state === 'blocked'
            ? [{ label: t('continueTask'), icon: 'play', run: () => run(botCall('tasks.continue', { id }, src)) }]
            : []),
          task.manual
            ? { label: t('handBack'), icon: 'bot', run: () => run(botCall('tasks.handBack', { id }, src)) }
            : { label: t('takeOver'), icon: 'edit', run: () => run(botCall('tasks.manual', { id }, src)) },
          { label: t('markDone'), icon: 'check', run: () => run(botCall('tasks.markDone', { id }, src)) },
          { label: t('stopTask'), icon: 'stop', run: () => run(botCall('tasks.stop', { id }, src)) },
        ]
      : [
          { label: t('markDone'), icon: 'check', disabled: task.state === 'done', run: () => run(botCall('tasks.markDone', { id }, src)) },
          { label: t('retryTask'), icon: 'refresh', run: () => run(botCall('tasks.retry', { id }, src)) },
          ...(task.state !== 'ready' && (task.sessionId || task.worktree)
            ? [
                {
                  label: t('cleanTask'),
                  icon: 'trash',
                  run: async () => {
                    if (!(await askConfirm({ title: t('cleanTaskTitle', { title: task.title }), message: t('cleanTaskMessage') }))) return;
                    try {
                      const r = await botCall<{ worktreeKept: boolean }>('tasks.cleanup', { id }, src);
                      if (r.worktreeKept && task.worktree) toast(t('cleanTaskKept', { path: task.worktree.path }));
                    } catch (e) {
                      errorToast(e);
                    }
                  },
                },
              ]
            : []),
        ]),
    ...(task.worktree && !active
      ? [
          '-' as const,
          {
            label: t('removeWorktree'),
            icon: 'trash',
            danger: true,
            run: async () => {
              const w = task.worktree!;
              if (!(await askConfirm({ title: t('removeWorktreeTitle'), message: t('removeWorktreeMessage', { path: w.path, branch: w.branch }), danger: true })))
                return;
              run(botCall('tasks.removeWorktree', { id }, src));
            },
          },
        ]
      : []),
  ];
}

function TaskLine({ task, active, showHost, src }: { task: TaskRow; active: boolean; showHost: boolean; src: string }) {
  const dot = taskDot(task.state);
  const hostName = useStore((s) => s.hosts.find((h) => h.config.id === localHost(src, task.host))?.config.name ?? task.host);
  const open = () => task.sessionId && openOn(src, task.host, task.sessionId);
  return (
    <div className={`session-row-wrap ${active ? 'active' : ''}`}>
      <button
        className={`session-row bot-task-row ${active ? 'active' : ''} ${ACTIVE_TASK.includes(task.state) ? '' : 's-off'}`}
        onClick={open}
        onContextMenu={(e) => openMenu(e, taskMenu(task, src))}
        title={`${task.title}\n${hostName} · ${task.cwd}\n${taskStateLabel(task.state)}${task.lastReport?.summary ? `\n${task.lastReport.summary}` : ''}`}
      >
        <span className="row-badge">
          <ToolBadge tool={String(task.spec.tool ?? 'claude')} size={16} />
        </span>
        <span className="session-title">
          {task.title}
          {showHost && <span className="row-dir"> · {hostName}</span>}
        </span>
        <span className="row-end">
          {dot ? <span className={`sdot sdot-${dot}`} /> : <span className="row-time">{taskStateLabel(task.state)}</span>}
        </span>
      </button>
      <div className="row-actions">
        <button
          className="row-act"
          title={t('more')}
          onClick={(e) => {
            const r = e.currentTarget.getBoundingClientRect();
            openMenu({ clientX: r.left, clientY: r.bottom + 2 }, taskMenu(task, src));
          }}
        >
          <Icon name="more" size={14} />
        </button>
      </div>
    </div>
  );
}

function BotBlock({ b, src }: { b: BotEntry; src: string }) {
  const view = useStore((s) => s.view);
  const id = b.config.id;
  const [showEnded, setShowEnded] = useState(false);
  const [collapsed, setCollapsed] = useState<boolean>(() => lsGet(`collapse-bot-${id}`, false));
  const toggle = () => {
    lsSet(`collapse-bot-${id}`, !collapsed);
    setCollapsed(!collapsed);
  };
  const dot = botDot(b);
  const isActive = (host: string, sid?: string | null) => view.kind === 'session' && view.hostId === localHost(src, host) && view.sessionId === sid;
  const botActive =
    (b.session && isActive(b.session.host, b.session.sessionId)) || (view.kind === 'bot' && view.botId === id && (view.src ?? '') === src);
  const live = b.tasks.filter((x) => ACTIVE_TASK.includes(x.state) || x.state === 'ready' || x.state === 'failed');
  const ended = b.tasks.filter((x) => !live.includes(x));
  // Name the host on task rows once tasks of this Bot can land on more than one host
  const showHost = new Set([b.config.bot.host, ...b.config.workspaces.map((w) => w.host), ...b.tasks.map((x) => x.host)]).size > 1;
  const open = () =>
    b.session ? openOn(src, b.session.host, b.session.sessionId) : navigate({ kind: 'bot', botId: id, src });
  return (
    <div className="bot-block">
      <div className={`session-row-wrap ${botActive ? 'active' : ''}`}>
        <button
          className={`session-row bot-row ${botActive ? 'active' : ''} ${b.config.enabled ? '' : 's-off'}`}
          onClick={open}
          onContextMenu={(e) => openMenu(e, botMenu(b, src))}
          title={`${b.config.name}\n${botStateLabel(b.state)}${b.error ? `\n${b.error}` : ''}`}
        >
          <span
            className={`chev bot-chev ${collapsed ? '' : 'open'}`}
            onClick={(e) => {
              e.stopPropagation();
              toggle();
            }}
          >
            <Icon name="chevron" size={11} />
          </span>
          <span className="row-badge">
            <Icon name="bot" size={15} />
          </span>
          <span className="session-title">{b.config.name}</span>
          {needsYouCount(b) > 0 && <span className="pill pill-attn">{needsYouCount(b)}</span>}
          {collapsed && live.length > 0 && <span className="project-count">{live.length}</span>}
          <span className="row-end">{dot && <span className={`sdot sdot-${dot}`} />}</span>
        </button>
        <div className="row-actions">
          <button className="row-act" title={t('editBot')} onClick={() => navigate({ kind: 'bot', botId: id, src })}>
            <Icon name="settings" size={13} />
          </button>
          <button
            className="row-act"
            title={t('more')}
            onClick={(e) => {
              const r = e.currentTarget.getBoundingClientRect();
              openMenu({ clientX: r.left, clientY: r.bottom + 2 }, botMenu(b, src));
            }}
          >
            <Icon name="more" size={14} />
          </button>
        </div>
      </div>
      {!collapsed && (
        <div className="bot-tasks">
          {live.map((x) => (
            <TaskLine key={x.id} task={x} active={isActive(x.host, x.sessionId)} showHost={showHost} src={src} />
          ))}
          {ended.length > 0 && (
            <button className="old-toggle" onClick={() => setShowEnded(!showEnded)}>
              <span className={`chev ${showEnded ? 'open' : ''}`}>
                <Icon name="chevron" size={12} />
              </span>
              {showEnded ? t('hideEnded') : t('showEnded', { n: ended.length })}
            </button>
          )}
          {showEnded && ended.map((x) => <TaskLine key={x.id} task={x} active={isActive(x.host, x.sessionId)} showHost={showHost} src={src} />)}
        </div>
      )}
    </div>
  );
}

/** Menu of the events a task came from; ones with a link open it */
async function sourceMenu(el: HTMLElement, task: TaskRow, src: string) {
  const r = el.getBoundingClientRect();
  try {
    const list = await botCall<EventRow[]>('tasks.events', { id: task.id }, src);
    const cut = (x: string) => (x.length > 80 ? x.slice(0, 80) + '…' : x);
    openMenu(
      { clientX: r.left, clientY: r.bottom + 2 },
      list.map((e) => ({
        label: cut(`${e.senderName || e.sender}: ${e.text.replace(/\s+/g, ' ')}`),
        icon: e.url ? 'external' : 'chat',
        disabled: !e.url,
        run: () => e.url && openUrl(e.url),
      })),
    );
  } catch (e) {
    errorToast(e);
  }
}

/** Open the new-session page in Bot mode, on a connected host the source's desktop can reach */
export function newBot(src: string) {
  const hosts = getState().hosts.filter((h) => h.status.status === 'connected' && unmapHost(src, h.config.id) !== null);
  const pick = hosts.find((h) => unmapHost(src, h.config.id) === 'local') ?? hosts[0];
  if (!pick) {
    toast(t('noHostForBot'), 'error');
    return;
  }
  openNewSession({ hostId: pick.config.id, bot: { src } });
}

/** A configuration change the Bot asked for, waiting for the owner: what changes and why, confirm or decline */
function PendingCard({ bot, src }: { bot: BotEntry; src: string }) {
  const p = bot.pendingChange;
  const [busy, setBusy] = useState(false);
  if (!p) return null;
  const answer = async (approve: boolean) => {
    setBusy(true);
    try {
      await botCall('bots.confirmChange', { botId: bot.config.id, id: p.id, approve }, src);
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };
  const items = p.items.map((i) =>
    i === 'accept'
      ? t('changeAccept')
      : i === 'allow'
        ? t('changeAllow')
        : i === 'approval'
          ? t('changeApproval')
          : i === 'computerUse'
            ? t('changeComputerUse')
            : t('changePermission'),
  );
  return (
    <div className="bot-approvals">
      <div className="bot-approval bot-confirm">
        <div className="bot-approval-head">
          <span className="bot-approval-icon">
            <Icon name="settings" size={13} />
          </span>
          <span className="bot-approval-title">{t('confirmChangeTitle', { items: items.join(t('listSep')) })}</span>
        </div>
        {p.summary && <div className="bot-approval-text">{p.summary}</div>}
        <div className="bot-approval-actions">
          <span className="bot-bar-fill" />
          <button className="btn small" disabled={busy} onClick={() => answer(false)}>
            {t('decline')}
          </button>
          <button className="btn small primary" disabled={busy} onClick={() => answer(true)}>
            {t('confirmChange')}
          </button>
        </div>
      </div>
    </div>
  );
}

/** Minutes left before an undecided draft is dropped (the engine drops them after 2 hours) */
function expiresIn(createdAt: number) {
  const min = Math.max(0, Math.round((createdAt + 2 * 3600_000 - Date.now()) / 60_000));
  return min >= 60 ? t('expiresInHours', { n: Math.floor(min / 60), m: min % 60 }) : t('expiresInMinutes', { n: min });
}

/** One draft: the message it answers, what would go out (a reply can be edited first), approve or reject */
function ApprovalCard({ a, src }: { a: Approval; src: string }) {
  const [busy, setBusy] = useState(false);
  const [text, setText] = useState<string | null>(null);
  const [showInput, setShowInput] = useState(false);
  const answer = async (approve: boolean) => {
    setBusy(true);
    try {
      await botCall('bots.approve', { id: a.id, approve, text: approve && text !== null ? text : null }, src);
    } catch (e) {
      errorToast(e);
      setBusy(false);
    }
  };
  const reply = a.kind === 'reply';
  const edited = text !== null && text.trim() !== a.text.trim();
  const hasInput = !reply && a.input != null && !(typeof a.input === 'object' && Object.keys(a.input as object).length === 0);
  return (
    <div className="bot-approval">
      <div className="bot-approval-head">
        <span className="bot-approval-icon">
          <Icon name="shield" size={13} />
        </span>
        <span className="bot-approval-title">{reply ? t('approvalReplyTitle') : t('approvalToolTitle')}</span>
        {(a.source || a.instanceId) && <span className="bot-approval-src truncate">{a.source || a.instanceId}</span>}
        <span className="bot-bar-fill" />
        <span className="bot-approval-time" title={fmtTime(a.createdAt)}>
          {expiresIn(a.createdAt)}
        </span>
      </div>
      {reply && a.quote && (
        <div className="bot-approval-quote">
          {a.quoteFrom && <span className="bot-approval-who">{a.quoteFrom}</span>}
          <span className="bot-approval-quote-text">{a.quote}</span>
        </div>
      )}
      {text !== null ? (
        <textarea
          className="bot-approval-edit"
          autoFocus
          value={text}
          rows={Math.min(10, Math.max(3, text.split('\n').length + 1))}
          onChange={(e) => setText(e.target.value)}
        />
      ) : (
        <div className="bot-approval-text">{a.text}</div>
      )}
      {!reply && a.tool && (
        <div className="bot-approval-tool">
          <span className="mono">{a.tool}</span>
          {hasInput && (
            <button className="link-btn small" onClick={() => setShowInput(!showInput)}>
              {showInput ? t('hideInput') : t('showInput')}
            </button>
          )}
        </div>
      )}
      {showInput && <pre className="bot-approval-input">{JSON.stringify(a.input, null, 2)}</pre>}
      <div className="bot-approval-actions">
        {reply && text === null && (
          <button className="btn small ghost" disabled={busy} onClick={() => setText(a.text)}>
            <Icon name="edit" size={13} /> {t('edit')}
          </button>
        )}
        {text !== null && (
          <button className="btn small ghost" disabled={busy} onClick={() => setText(null)}>
            {t('cancelEdit')}
          </button>
        )}
        <span className="bot-bar-fill" />
        <button className="btn small" disabled={busy} onClick={() => answer(false)}>
          {t('reject')}
        </button>
        <button className="btn small primary" disabled={busy || (text !== null && !text.trim())} onClick={() => answer(true)}>
          {reply ? (edited ? t('approveEdited') : t('approveSend')) : t('approveRun')}
        </button>
      </div>
    </div>
  );
}

/** Drafts of a Bot waiting for the owner: the oldest first, the rest folded */
export function ApprovalCards({ bot, src, inline }: { bot: BotEntry; src: string; inline?: boolean }) {
  const [all, setAll] = useState(false);
  const list = [...(bot.approvals ?? [])].sort((x, y) => x.createdAt - y.createdAt);
  if (list.length === 0) return null;
  const shown = all ? list : list.slice(0, 1);
  return (
    <div className={`bot-approvals ${inline ? 'inline' : ''}`}>
      {shown.map((a) => (
        <ApprovalCard key={a.id} a={a} src={src} />
      ))}
      {list.length > 1 && (
        <button className="link-btn small bot-approvals-more" onClick={() => setAll(!all)}>
          {all ? t('approvalsFold') : t('approvalsMore', { n: list.length - 1 })}
        </button>
      )}
    </div>
  );
}

/** Secrets of one plugin instance: password fields that go straight to the desktop app's plugin configuration */
function SecretForm({ bot, card, src }: { bot: BotEntry; card: SecretCard; src: string }) {
  const [values, setValues] = useState<Record<string, string>>({});
  const [shown, setShown] = useState(false);
  const [busy, setBusy] = useState(false);
  const en = isEn();
  const filled = card.fields.filter((f) => values[f.key]?.trim());
  // Every missing required one must be entered; for a requested replacement, at least one
  const ready = filled.length > 0 && card.fields.every((f) => !f.missing || values[f.key]?.trim());
  const save = async () => {
    setBusy(true);
    try {
      const r: any = await botCall('bots.fillSecrets', { botId: bot.config.id, instanceId: card.instanceId, values }, src);
      setValues({});
      toast(r?.enabled ? t('secretSavedOn', { name: card.name }) : t('secretSaved', { name: card.name }), 'info');
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };
  const dismiss = async () => {
    setBusy(true);
    try {
      await botCall('bots.dismissSecret', { botId: bot.config.id, id: card.requestId }, src);
    } catch (e) {
      errorToast(e);
      setBusy(false);
    }
  };
  return (
    <div className="bot-approval bot-secret">
      <div className="bot-approval-head">
        <span className="bot-approval-icon">
          <Icon name="key" size={13} />
        </span>
        <span className="bot-approval-title">{card.requestId ? t('secretReplaceTitle') : t('secretNeededTitle')}</span>
        <span className="bot-approval-src truncate">{card.name}</span>
      </div>
      {card.reason && <div className="bot-approval-text">{card.reason}</div>}
      <form
        className="bot-secret-fields"
        autoComplete="off"
        onSubmit={(e) => {
          e.preventDefault();
          if (ready && !busy) void save();
        }}
      >
        {card.fields.map((f) => {
          const title = (en && f.titleEn) || f.title || f.key;
          const hint = (en && f.descriptionEn) || f.description;
          return (
            <label key={f.key} className="bot-secret-field">
              <span className="bot-secret-label">{title}</span>
              <input
                type={shown ? 'text' : 'password'}
                autoComplete="new-password"
                spellCheck={false}
                className="mono"
                value={values[f.key] ?? ''}
                placeholder={f.missing ? '' : t('secretKeepPlaceholder')}
                onChange={(e) => setValues({ ...values, [f.key]: e.target.value })}
              />
              {hint && <span className="muted small">{hint}</span>}
            </label>
          );
        })}
        <div className="bot-secret-note muted small">
          <Icon name="shield" size={12} /> {t('secretNote')}
        </div>
        <div className="bot-approval-actions">
          <button type="button" className="btn small ghost" onClick={() => setShown(!shown)}>
            <Icon name="eye" size={13} /> {shown ? t('secretHide') : t('secretShow')}
          </button>
          <span className="bot-bar-fill" />
          {card.requestId && (
            <button type="button" className="btn small" disabled={busy} onClick={dismiss}>
              {t('secretDismiss')}
            </button>
          )}
          <button type="submit" className="btn small primary" disabled={busy || !ready}>
            {t('secretSave')}
          </button>
        </div>
      </form>
    </div>
  );
}

/** Plugin instances of a Bot waiting for the owner's secrets */
export function SecretCards({ bot, src, inline }: { bot: BotEntry; src: string; inline?: boolean }) {
  const list = bot.secrets ?? [];
  if (list.length === 0) return null;
  return (
    <div className={`bot-approvals ${inline ? 'inline' : ''}`}>
      {list.map((c) => (
        <SecretForm key={c.instanceId} bot={bot} card={c} src={src} />
      ))}
    </div>
  );
}

/** Bar under the session header for a Bot's own session or one of its tasks */
export function SessionBotBar({ info }: { info: SessionInfo }) {
  useLang();
  const link = info.bot;
  // Subscribe to list changes; the source of the session's desktop app may appear later
  const src = useBots(() => sourceOf(link?.clientId));
  const list = useBotList(src ?? '');
  const bot = src === null || !link ? undefined : list?.find((b) => b.config.id === link.botId);
  if (!link) return null;
  if (src === null || !bot) {
    return (
      <div className="bot-bar">
        <Icon name="bot" size={14} />
        <span className="truncate">{link.role === 'task' ? t('taskOfOther', { name: link.botName }) : t('botOfOther', { name: link.botName })}</span>
      </div>
    );
  }
  if (link.role === 'task') {
    const task = bot.tasks.find((x) => x.id === link.taskId);
    const active = task ? ACTIVE_TASK.includes(task.state) : false;
    const waitingExternal = !!task && task.state === 'needsYou' && !task.manual && !info.pending;
    return (
      <div className="bot-bar">
        <Icon name="bot" size={14} />
        <button className="link-btn" onClick={() => navigate({ kind: 'bot', botId: bot.config.id, src })}>
          {bot.config.name}
        </button>
        {task && <span className={`pill bot-task-${task.state}`}>{taskStateLabel(task.state)}</span>}
        {task?.spec.computerUse === true && (
          <span className="muted small" title={t('computerUseOnDemand')}>
            <Icon name="laptop" size={13} />
          </span>
        )}
        {task?.manual && <span className="muted small">{t('manualHint')}</span>}
        {waitingExternal && task?.lastReport?.question && (
          <span className="muted small truncate" title={task.lastReport.question}>
            {task.lastReport.question}
          </span>
        )}
        {task && task.eventIds.length > 0 && (
          <button className="btn small" onClick={(e) => sourceMenu(e.currentTarget, task, src)}>
            {t('sources', { n: task.eventIds.length })}
          </button>
        )}
        {task?.worktree && (
          <span className="muted small mono truncate" title={task.worktree.path}>
            {task.worktree.branch}
          </span>
        )}
        <span className="bot-bar-fill" />
        {waitingExternal && (
          <button className="btn small primary" onClick={() => run(botCall('tasks.continue', { id: task.id }, src))}>
            <Icon name="play" size={12} /> {t('continueTask')}
          </button>
        )}
        {task && active && task.manual && (
          <button className="btn small" onClick={() => run(botCall('tasks.handBack', { id: task.id }, src))}>
            {t('handBack')}
          </button>
        )}
        {task && task.state !== 'done' && (
          <button className="btn small" onClick={() => run(botCall('tasks.markDone', { id: task.id }, src))}>
            {t('markDone')}
          </button>
        )}
        {task && (
          <button
            className="icon-btn"
            title={t('more')}
            onClick={(e) => {
              const r = e.currentTarget.getBoundingClientRect();
              openMenu({ clientX: r.left, clientY: r.bottom + 2 }, taskMenu(task, src));
            }}
          >
            <Icon name="more" size={15} />
          </button>
        )}
      </div>
    );
  }
  return (
    <>
    <div className="bot-bar">
      <Icon name="bot" size={14} />
      <span className="truncate">{t('botSessionOf', { name: bot.config.name })}</span>
      <span className="pill">{botStateLabel(bot.state)}</span>
      {bot.waiting > 0 && <span className="muted small">{t('waitingItems', { n: bot.waiting })}</span>}
      <span className="bot-bar-fill" />
      <button className="btn small" onClick={() => navigate({ kind: 'bot', botId: bot.config.id, src })}>
        <Icon name="eye" size={13} /> {t('botDetails')}
      </button>
      <button
        className="icon-btn"
        title={t('more')}
        onClick={(e) => {
          const r = e.currentTarget.getBoundingClientRect();
          openMenu({ clientX: r.left, clientY: r.bottom + 2 }, botMenu(bot, src));
        }}
      >
        <Icon name="more" size={15} />
      </button>
    </div>
    <PendingCard bot={bot} src={src} />
    <SecretCards bot={bot} src={src} />
    <ApprovalCards bot={bot} src={src} />
    </>
  );
}

/** This app's Bots section, then one per remote desktop app */
export function BotsSections() {
  const remotes = useBots((s) => s.remotes);
  return (
    <>
      <BotsSection src="" />
      {remotes.map((r) => (
        <BotsSection key={r.key} src={r.key} />
      ))}
    </>
  );
}

function BotsSection({ src }: { src: string }) {
  useLang();
  const list = useBotList(src);
  const remote = useBots((s) => s.remotes.find((r) => r.key === src));
  const view = useStore((s) => s.view);
  const key = src ? `collapse-bots-${remote?.clientId}` : 'collapse-bots';
  const [collapsed, setCollapsed] = useState<boolean>(() => lsGet(key, false));
  if (!list && !remote) return null;
  const toggle = () => {
    lsSet(key, !collapsed);
    setCollapsed(!collapsed);
  };
  const attn = (list ?? []).reduce((n, b) => n + needsYouCount(b), 0);
  const here = (view.kind === 'plugins' || view.kind === 'bot' || view.kind === 'botEvents') && (view.src ?? '') === src;
  return (
    <div className="host-block bots-block">
      <div className="host-head">
        <button className="host-name" onClick={toggle} title={remote ? t('remoteBotsHint', { name: remote.name }) : t('botsSectionHint')}>
          <span className="host-icon">
            <Icon name="bot" size={13} />
          </span>
          <span className="truncate">{remote ? t('botsOf', { name: remote.name }) : t('bots')}</span>
          {collapsed && attn > 0 && <span className="pill pill-attn">{attn}</span>}
          <span className={`chev ${collapsed ? '' : 'open'}`}>
            <Icon name="chevron" size={12} />
          </span>
        </button>
        {list && (
          <>
            <button className={`host-act ${here && view.kind === 'plugins' ? 'active' : ''}`} title={t('plugins')} onClick={() => navigate({ kind: 'plugins', src })}>
              <Icon name="plug" size={14} />
            </button>
            <button className="host-act" title={t('newBot')} onClick={() => newBot(src)}>
              <Icon name="plus" size={15} />
            </button>
          </>
        )}
      </div>
      {!collapsed && (
        <div className="host-body">
          {list?.map((b) => (
            <BotBlock key={b.config.id} b={b} src={src} />
          ))}
          {list && list.length === 0 && <div className="muted small pad-x">{t('noBotsYet')}</div>}
          {!list && remote && (
            <div className="muted small pad-x">
              {remote.error ? t('desktopUnreachable', { error: remote.error }) : t('loading')}{' '}
              {remote.error && (
                <button className="link-btn small" onClick={() => refreshRemote(src)}>
                  {t('refresh')}
                </button>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
