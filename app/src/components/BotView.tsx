// Bot details page (desktop app only, or a remote control of one): four-section overview
// (Needs You -> SOPs & Prompt -> Memories & Notes -> Connectors, Workspaces, Routines & Outbox/Inbox).
// Configuration, SOPs and routines are changed by talking to the Bot; memories and scratchpad notes can also be
// added, edited or deleted directly here.

import { useEffect, useState } from 'react';
import { getT, useLang } from '../i18n';
import { errorToast, navigate, toast } from '../store';
import { TOOL_LABEL } from '../types';
import { permLabel } from '../perm';
import {
  useBotCall,
  useBotList,
  useBotSource,
  type BotConfig,
  type BotEntry,
  type EventRow,
  type MemoryItem,
  type OutboxRow,
  type ConnectorInstance,
  type RoutineRun,
  type Sop,
} from '../bots';
import {
  ApprovalCards,
  PendingCard,
  SecretCards,
  TaskAnsweredBanner,
  TaskQuestionCard,
  answeredWaitingTasks,
  botMenu,
  botStateLabel,
  deleteBotWithConfirm,
  localHost,
  openOn,
  questionTasks,
  useSourceHosts,
  visibleAttention,
} from './BotsSection';
import { EventLine, fmtTime, PromptDiff, StatsCard } from './BotActivity';
import { askConfirm, openMenu } from './Overlay';
import { showChanges } from './SessionActions';
import { Icon } from './Icon';

const t = getT('bots');

interface BotRowFull {
  config: BotConfig;
  notes: string;
  session: { host: string; sessionId: string } | null;
}

interface PromptRevision {
  revision: number;
  baseRevision: number;
  sections: BotConfig['prompt'];
  sops?: Sop[];
  origin: string;
  at: number;
}

function originLabel(o: string) {
  return o === 'create' ? t('originCreate') : o === 'ai' || o === 'bot' ? t('originBot') : o === 'restore' ? t('originRestore') : t('originForm');
}

function memorySourceLabel(s: string) {
  switch (s) {
    case 'owner':
      return t('memSourceOwner');
    case 'review':
      return t('memSourceReview');
    case 'task':
      return t('memSourceTask');
    default:
      return t('memSourceBot');
  }
}

function outboxStateLabel(s: string) {
  switch (s) {
    case 'sent':
      return t('outboxSent');
    case 'gone':
      return t('outboxGone');
    case 'failed':
      return t('outboxFailed');
    default:
      return t('outboxPending');
  }
}

function EventsCard({ botId }: { botId: string }) {
  const botCall = useBotCall();
  const [list, setList] = useState<EventRow[] | null>(null);
  const src = useBotSource();
  const load = () => botCall<EventRow[]>('bots.events', { id: botId, limit: 20 }).then(setList).catch(errorToast);
  useEffect(() => {
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [botId]);
  return (
    <div className="card">
      <div className="card-title">
        {t('recentEvents')}
        <span className="row">
          <button className="link-btn small" onClick={() => navigate({ kind: 'botEvents', botId, src: src || undefined })}>
            {t('allEvents')}
          </button>
          <button className="icon-btn" title={t('refresh')} onClick={load}>
            <Icon name="refresh" size={13} />
          </button>
        </span>
      </div>
      {!list ? (
        <p className="muted small">{t('loading')}</p>
      ) : list.length === 0 ? (
        <p className="muted small">{t('noEvents')}</p>
      ) : (
        <div className="bot-events">
          {list.map((e) => (
            <EventLine key={e.id} e={e} src={src} />
          ))}
        </div>
      )}
    </div>
  );
}

/** Outbox rows filtered to this Bot (`connectors.outbox { botId }`), loaded lazily when opened */
function BotOutboxPanel({ botId, instName }: { botId: string; instName: (id: string) => string }) {
  const botCall = useBotCall();
  const [open, setOpen] = useState(false);
  const [rows, setRows] = useState<OutboxRow[] | null>(null);
  const load = () => botCall<OutboxRow[]>('connectors.outbox', { botId, limit: 30 }).then(setRows).catch(errorToast);
  useEffect(() => {
    if (open) load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, botId]);
  return (
    <div className="bot-subpanel">
      <div className="row">
        <button className="link-btn small" onClick={() => setOpen(!open)}>
          <Icon name={open ? 'down' : 'chevron'} size={12} /> {open ? t('hideOutbox') : t('viewOutbox')}
        </button>
        {open && (
          <button className="icon-btn" title={t('refresh')} onClick={load}>
            <Icon name="refresh" size={13} />
          </button>
        )}
      </div>
      {open && (
        <div className="bot-outbox-list">
          {!rows ? (
            <p className="muted small">{t('loading')}</p>
          ) : rows.length === 0 ? (
            <p className="muted small">{t('outboxEmpty')}</p>
          ) : (
            rows.map((o) => (
              <div key={o.key} className="bot-outbox-row">
                <span className="muted small">{fmtTime(o.createdAt)}</span>
                <span className="pill">{instName(o.instanceId)}</span>
                <span className={`pill bot-outbox-${o.state}`}>{outboxStateLabel(o.state)}</span>
                <span className="bot-outbox-text truncate" title={o.text}>
                  {o.text}
                </span>
                {o.attempts > 1 && <span className="muted small">{t('outboxRetry', { n: o.attempts })}</span>}
                {o.error && (
                  <span className="muted small truncate" title={o.error}>
                    {o.error}
                  </span>
                )}
              </div>
            ))
          )}
        </div>
      )}
    </div>
  );
}

export function runStateLabel(s: string) {
  switch (s) {
    case 'queued':
      return t('runQueued');
    case 'running':
      return t('runRunning');
    case 'nothing':
      return t('runNothing');
    case 'failed':
      return t('runFailed');
    case 'skipped':
      return t('runSkipped');
    default:
      return t('runDone');
  }
}

/** Routines: schedule, linked SOP, last run, run now and run history. Changing them is done by talking to the Bot */
function RoutinesCard({ entry }: { entry: BotEntry }) {
  const botCall = useBotCall();
  const [open, setOpen] = useState<string | null>(null);
  const [runs, setRuns] = useState<RoutineRun[]>([]);
  const routines = entry.config.routines ?? [];
  const sops = entry.config.sops ?? [];
  const botId = entry.config.id;
  const last = entry.lastRuns ?? {};
  const stamp = JSON.stringify(last);
  useEffect(() => {
    if (open) botCall<RoutineRun[]>('bots.routineRuns', { botId, routineId: open, limit: 20 }).then(setRuns).catch(errorToast);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, botId, stamp]);
  const runNow = async (id: string, name: string) => {
    try {
      await botCall('bots.runRoutine', { botId, routineId: id });
      toast(t('runStarted', { name }), 'info');
    } catch (e) {
      errorToast(e);
    }
  };
  return (
    <div className="card">
      <div className="card-title">{t('routines')}</div>
      {routines.length === 0 ? (
        <p className="muted small">{t('routinesEmpty')}</p>
      ) : (
        routines.map((r) => {
          const l = last[r.id];
          const busy = l && (l.state === 'queued' || l.state === 'running');
          const sop = r.sopId ? sops.find((x) => x.id === r.sopId) : undefined;
          return (
            <div key={r.id} className={`bot-routine ${r.enabled ? '' : 'off'}`}>
              <div className="bot-routine-head">
                <span className="bot-routine-name">{r.name}</span>
                <span className="mono small muted">{r.schedule}</span>
                {sop && (
                  <span className="pill bot-sop-pill">
                    <Icon name="outline" size={11} /> {sop.id} · {sop.name}
                  </span>
                )}
                {!r.enabled && <span className="pill">{t('routinePaused')}</span>}
                <span className="bot-bar-fill" />
                <button className="link-btn small" onClick={() => setOpen(open === r.id ? null : r.id)}>
                  {open === r.id ? t('hideHistory') : t('runHistory')}
                </button>
                <button className="btn small" disabled={!entry.config.enabled || !!busy} onClick={() => runNow(r.id, r.name)}>
                  <Icon name="play" size={12} /> {t('runNow')}
                </button>
              </div>
              <div className="bot-routine-prompt small">{r.prompt}</div>
              <div className="muted small">
                {l ? t('lastRun', { time: fmtTime(l.startedAt), state: runStateLabel(l.state) }) : t('noRuns')}
                {l?.summary && <span> · {l.summary}</span>}
              </div>
              {open === r.id && (
                <div className="bot-runs">
                  {runs.length === 0 ? (
                    <p className="muted small">{t('noRuns')}</p>
                  ) : (
                    runs.map((x) => (
                      <div key={x.id} className="bot-run">
                        <span className="muted small">{fmtTime(x.startedAt)}</span>
                        <span className={`pill bot-run-${x.state}`}>{runStateLabel(x.state)}</span>
                        {x.manual && <span className="muted small">{t('runManual')}</span>}
                        <span className="small">{x.summary}</span>
                      </div>
                    ))
                  )}
                </div>
              )}
            </div>
          );
        })
      )}
    </div>
  );
}

/** Section 3: Durable tiered memories (global + per-workspace) and editable scratchpad notes */
function MemoriesCard({
  botId,
  config,
  memories,
  notes,
  hostName,
  onMemoriesChange,
  onNotesSaved,
}: {
  botId: string;
  config: BotConfig;
  memories: MemoryItem[];
  notes: string;
  hostName: (id: string) => string;
  onMemoriesChange: () => void;
  onNotesSaved: (next: string) => void;
}) {
  const botCall = useBotCall();
  const [editingMem, setEditingMem] = useState<{ id?: string; scope: string; content: string } | null>(null);
  const [savingMem, setSavingMem] = useState(false);
  const [editingNotes, setEditingNotes] = useState<string | null>(null);
  const [savingNotes, setSavingNotes] = useState(false);

  const wsOptions = config.workspaces.map((w) => ({
    key: `${w.host}:${w.cwd}`,
    label: w.name ? `${w.name} (${hostName(w.host)}: ${w.cwd})` : `${hostName(w.host)}: ${w.cwd}`,
  }));
  const scopeLabel = (scope: string) => wsOptions.find((x) => x.key === scope)?.label ?? scope;

  const globals = memories.filter((m) => !m.scope);
  const byScope = new Map<string, MemoryItem[]>();
  for (const m of memories) {
    if (!m.scope) continue;
    const arr = byScope.get(m.scope) ?? [];
    arr.push(m);
    byScope.set(m.scope, arr);
  }

  const saveMemory = async () => {
    if (!editingMem || !editingMem.content.trim()) return;
    setSavingMem(true);
    try {
      await botCall('bots.saveMemory', {
        botId,
        id: editingMem.id ?? null,
        scope: editingMem.scope,
        content: editingMem.content.trim(),
      });
      setEditingMem(null);
      onMemoriesChange();
      toast(t('saved'), 'info');
    } catch (e) {
      errorToast(e);
    } finally {
      setSavingMem(false);
    }
  };

  const deleteMemory = async (m: MemoryItem) => {
    try {
      await botCall('bots.deleteMemory', { botId, id: m.id });
      onMemoriesChange();
    } catch (e) {
      errorToast(e);
    }
  };

  const saveNotes = async () => {
    if (editingNotes === null) return;
    setSavingNotes(true);
    try {
      await botCall('bots.saveNotes', { id: botId, notes: editingNotes });
      onNotesSaved(editingNotes);
      setEditingNotes(null);
      toast(t('saved'), 'info');
    } catch (e) {
      errorToast(e);
    } finally {
      setSavingNotes(false);
    }
  };

  const renderRow = (m: MemoryItem) => (
    <div key={m.id} className="bot-mem-row">
      <span className="bot-mem-text">{m.content}</span>
      <span className={`pill bot-mem-src-${m.source}`}>{memorySourceLabel(m.source)}</span>
      <button
        className="icon-btn"
        title={t('edit')}
        onClick={() => setEditingMem({ id: m.id, scope: m.scope, content: m.content })}
      >
        <Icon name="edit" size={12} />
      </button>
      <button className="icon-btn" title={t('delete')} onClick={() => deleteMemory(m)}>
        <Icon name="x" size={12} />
      </button>
    </div>
  );

  return (
    <div className="card">
      <div className="card-title">
        <span>
          {t('memoriesTitle')} <span className="muted small">({memories.length} / 80)</span>
        </span>
        <button className="btn small" onClick={() => setEditingMem({ scope: '', content: '' })}>
          <Icon name="plus" size={12} /> {t('addMemory')}
        </button>
      </div>
      <p className="muted small">{t('memoriesHint')}</p>

      {editingMem && (
        <div className="bot-mem-editor">
          <div className="row wrap">
            <select
              value={editingMem.scope}
              onChange={(e) => setEditingMem({ ...editingMem, scope: e.target.value })}
              disabled={savingMem}
            >
              <option value="">{t('memScopeGlobal')}</option>
              {wsOptions.map((w) => (
                <option key={w.key} value={w.key}>
                  {w.label}
                </option>
              ))}
            </select>
          </div>
          <textarea
            rows={2}
            maxLength={300}
            value={editingMem.content}
            placeholder={t('memContentPlaceholder')}
            onChange={(e) => setEditingMem({ ...editingMem, content: e.target.value })}
            disabled={savingMem}
          />
          <div className="row">
            <span className="muted small">{editingMem.content.length} / 300</span>
            <span className="bot-bar-fill" />
            <button className="btn small" disabled={savingMem} onClick={() => setEditingMem(null)}>
              {t('cancel')}
            </button>
            <button className="btn small primary" disabled={savingMem || !editingMem.content.trim()} onClick={saveMemory}>
              {t('save')}
            </button>
          </div>
        </div>
      )}

      {memories.length === 0 ? (
        <p className="muted small">{t('memoriesEmpty')}</p>
      ) : (
        <div className="bot-mem-groups">
          {globals.length > 0 && (
            <div className="bot-mem-group">
              <div className="bot-mem-group-title">
                <Icon name="globe" size={13} /> {t('memGlobalGroup')} ({globals.length})
              </div>
              {globals.map(renderRow)}
            </div>
          )}
          {[...byScope.entries()].map(([scope, items]) => (
            <div key={scope} className="bot-mem-group">
              <div className="bot-mem-group-title">
                <Icon name="folder" size={13} /> {t('memWorkspaceGroup', { name: scopeLabel(scope) })} ({items.length})
              </div>
              {items.map(renderRow)}
            </div>
          ))}
        </div>
      )}

      <div className="bot-notes-box">
        <div className="bot-notes-head">
          <span className="bot-section-label">{t('notes')}</span>
          {editingNotes === null && (
            <button className="link-btn small" onClick={() => setEditingNotes(notes)}>
              <Icon name="edit" size={12} /> {t('edit')}
            </button>
          )}
        </div>
        <p className="muted small">{t('notesHint')}</p>
        {editingNotes !== null ? (
          <div className="bot-mem-editor">
            <textarea
              rows={4}
              value={editingNotes}
              onChange={(e) => setEditingNotes(e.target.value)}
              disabled={savingNotes}
            />
            <div className="row">
              <span className="bot-bar-fill" />
              <button className="btn small" disabled={savingNotes} onClick={() => setEditingNotes(null)}>
                {t('cancel')}
              </button>
              <button className="btn small primary" disabled={savingNotes} onClick={saveNotes}>
                {t('save')}
              </button>
            </div>
          </div>
        ) : notes.trim() ? (
          <div className="bot-section-text">{notes}</div>
        ) : (
          <div className="muted small">{t('notesEmpty')}</div>
        )}
      </div>
    </div>
  );
}

/** One line of the details: a label and its value */
function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <>
      <dt>{label}</dt>
      <dd>{children}</dd>
    </>
  );
}

/** Tool, model and thinking level as one line */
function toolLine(tool: string, model?: string | null, effort?: string | null) {
  return [TOOL_LABEL[tool as keyof typeof TOOL_LABEL] ?? tool, model || t('toolDefault'), effort].filter(Boolean).join(' · ');
}

export function BotView({ botId }: { botId: string }) {
  useLang();
  const src = useBotSource();
  const botCall = useBotCall();
  const hosts = useSourceHosts(src);
  const list = useBotList(src);
  const entry = list?.find((b) => b.config.id === botId);
  const available = list !== null;
  const [row, setRow] = useState<BotRowFull | null>(null);
  const [revisions, setRevisions] = useState<PromptRevision[]>([]);
  const [memories, setMemories] = useState<MemoryItem[]>([]);
  const [showRevs, setShowRevs] = useState(false);
  const [showBasePrompt, setShowBasePrompt] = useState(false);
  /** revision whose diff against the one it was based on is open */
  const [diffOf, setDiffOf] = useState<number | null>(null);
  const [instances, setInstances] = useState<ConnectorInstance[]>([]);
  const [preview, setPreview] = useState<{ bot: string; task: string } | null>(null);
  const [defaults, setDefaults] = useState<Record<string, string>>({});
  // The list changes whenever the Bot saves; reload the full row then
  const stamp = entry ? JSON.stringify(entry.config) : '';

  const loadMemories = () => {
    botCall<MemoryItem[]>('bots.memories', { id: botId }).then(setMemories).catch(errorToast);
  };

  useEffect(() => {
    if (!available) return;
    botCall<{ bot: BotRowFull; prompts: PromptRevision[]; memories?: MemoryItem[] }>('bots.get', { id: botId })
      .then((r) => {
        setRow(r.bot);
        setRevisions(r.prompts);
        if (r.memories) setMemories(r.memories);
        else loadMemories();
      })
      .catch(errorToast);
    botCall<ConnectorInstance[]>('connectors.instances').then(setInstances).catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [botId, available, src, stamp]);
  useEffect(() => {
    if (available) botCall<Record<string, string>>('bots.defaults').then(setDefaults).catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [available, src]);

  if (!available) return <div className="page muted">{t('desktopOnly')}</div>;
  if (!row) return <div className="page muted">{t('loading')}</div>;
  const c = row.config;
  const sops = c.sops ?? [];
  const routines = c.routines ?? [];
  const hostName = (id: string) => hosts.find((h) => h.config.id === id)?.config.name ?? id;
  const instName = (id: string) => (id === 'app' ? t('notifyApp') : instances.find((i) => i.id === id)?.name ?? id);
  const disabledInst = (id: string) => id !== 'app' && instances.some((i) => i.id === id && !i.enabled);
  const names = (ids: string[]) =>
    ids.length === 0 ? (
      <span className="muted">{t('none')}</span>
    ) : (
      ids.map((id) => (
        <span key={id} className={`pill bot-pick ${disabledInst(id) ? 'off' : ''}`} title={disabledInst(id) ? t('instanceDisabled') : id}>
          {instName(id)}
        </span>
      ))
    );
  const approval = c.approval ?? { reply: false, write: false };
  const approvalLabel =
    approval.reply && approval.write
      ? t('approvalBoth')
      : approval.reply
        ? t('approvalReplyOnly')
        : approval.write
          ? t('approvalWriteOnly')
          : t('approvalNone');
  const weekdays = t('weekdays').split(',');
  const hours = c.activeHours;
  const hoursLine = hours
    ? t('activeHoursLine', {
        days: hours.days.length === 0 ? t('everyDay') : hours.days.map((d) => weekdays[d - 1] ?? d).join(t('listSep')),
        start: hours.start,
        end: hours.end,
      })
    : t('activeHoursAlways');
  const acceptLabel = c.accept === 'off' ? t('acceptOff') : c.accept === 'auto' ? t('acceptAuto') : t('acceptObserve');
  const acceptHint = c.accept === 'off' ? t('acceptOffHint') : c.accept === 'auto' ? t('acceptAutoHint') : t('acceptObserveHint');
  const openSession = () => entry?.session && openOn(src, entry.session.host, entry.session.sessionId);

  const setEnabled = async (enabled: boolean) => {
    try {
      await botCall('bots.setEnabled', { id: botId, enabled });
    } catch (e) {
      errorToast(e);
    }
  };

  const showPreview = () =>
    preview
      ? setPreview(null)
      : botCall<{ bot: string; task: string }>('bots.preview', { bot: c })
          .then(setPreview)
          .catch(errorToast);

  const sections: [string, string, string][] = [
    [t('promptRole'), c.prompt.role, ''],
    [t('promptCommunication'), c.prompt.communication, (defaults.communication ?? '').replace(/^#.*\n+/, '').trim()],
    [t('promptDelivery'), c.prompt.delivery, ''],
  ];

  // Section 1: Needs You aggregation (deduplicated against interactive decision cards)
  const questions = entry ? questionTasks(entry) : [];
  const answered = entry ? answeredWaitingTasks(entry) : [];
  const attn = entry ? visibleAttention(entry) : [];
  const readyTasks = entry ? entry.tasks.filter((x) => x.state === 'ready') : [];
  const needsTotal =
    (entry?.pendingChange ? 1 : 0) +
    (entry?.secrets?.length ?? 0) +
    questions.length +
    (entry?.approvals?.length ?? 0) +
    readyTasks.length +
    attn.length;

  const openTaskDiff = (task: { host: string; sessionId?: string | null }) => {
    if (!task.sessionId) return;
    const hostId = localHost(src, task.host);
    openOn(src, task.host, task.sessionId);
    if (hostId) {
      window.setTimeout(() => showChanges(hostId, task.sessionId!), 120);
    }
  };

  const baseVisible = showBasePrompt || sops.length === 0;

  return (
    <div className="page bot-page">
      <div className="page-head">
        <h2>
          {c.name || t('untitledBot')}
          {entry && <span className="pill bot-state">{botStateLabel(entry.state)}</span>}
        </h2>
        <div className="row">
          {entry?.session && (
            <button className="btn primary" onClick={openSession}>
              <Icon name="chat" size={14} /> {t('openBotSession')}
            </button>
          )}
          <button className="btn" onClick={() => setEnabled(!c.enabled)}>
            <Icon name={c.enabled ? 'pause' : 'play'} size={14} /> {c.enabled ? t('disable') : t('enable')}
          </button>
          {entry && (
            <button
              className="icon-btn"
              title={t('more')}
              onClick={(e) => {
                const r = e.currentTarget.getBoundingClientRect();
                openMenu({ clientX: r.left, clientY: r.bottom + 2 }, botMenu(entry, src));
              }}
            >
              <Icon name="more" size={16} />
            </button>
          )}
        </div>
      </div>
      <p className="muted small bot-talk-hint">
        <Icon name="chat" size={13} /> {t('changeByTalking')}
      </p>
      {entry?.error && <div className="card bot-error">{entry.error}</div>}

      {/* Section 1: Needs You — single human-attention inbox */}
      {entry && (needsTotal > 0 || answered.length > 0) && (
        <div className="card bot-needs-card">
          <div className="card-title">
            <span>
              {t('needsYou')} <span className="pill pill-attn">{needsTotal}</span>
            </span>
          </div>
          {entry.pendingChange && <PendingCard bot={entry} src={src} bare />}
          <SecretCards bot={entry} src={src} inline />
          {questions.map((qt) => (
            <TaskQuestionCard key={qt.id} task={qt} src={src} showTaskLink />
          ))}
          {answered.map((at) => (
            <TaskAnsweredBanner key={at.id} task={at} />
          ))}
          {(entry.approvals?.length ?? 0) > 0 && <ApprovalCards bot={entry} src={src} inline />}
          {readyTasks.map((rt) => (
            <div key={rt.id} className="bot-ready-row">
              <div className="bot-ready-main">
                <div className="row wrap">
                  <span className="pill bot-task-ready">{t('stateReady')}</span>
                  {rt.sessionId ? (
                    <button className="link-btn" onClick={() => openOn(src, rt.host, rt.sessionId!)}>
                      {rt.title}
                    </button>
                  ) : (
                    <strong>{rt.title}</strong>
                  )}
                  {typeof rt.spec.sopName === 'string' && (
                    <span className="pill bot-sop-pill">
                      <Icon name="outline" size={11} /> {rt.spec.sopName}
                    </span>
                  )}
                  {rt.worktree?.branch && <span className="muted small mono">{rt.worktree.branch}</span>}
                </div>
                {rt.lastReport?.summary && <div className="muted small">{rt.lastReport.summary}</div>}
              </div>
              <div className="row">
                {rt.sessionId && (
                  <button className="btn small" onClick={() => openTaskDiff(rt)}>
                    <Icon name="diff" size={12} /> {t('viewDiff')}
                  </button>
                )}
                <button className="btn small primary" onClick={() => botCall('tasks.markDone', { id: rt.id }).catch(errorToast)}>
                  <Icon name="check" size={12} /> {t('markDone')}
                </button>
              </div>
            </div>
          ))}
          {attn.map((a) => (
            <div key={a.id} className="bot-attn">
              <span>{a.text}</span>
              <button className="link-btn small" onClick={() => botCall('bots.resolve', { id: a.id }).catch(errorToast)}>
                {t('resolve')}
              </button>
            </div>
          ))}
        </div>
      )}

      {/* Section 2: SOPs & Baseline Prompt */}
      <div className="card">
        <div className="card-title">
          <span>
            {t('sopsTitle')} {sops.length > 0 && <span className="muted small">({sops.length})</span>}
          </span>
          <span className="row">
            <button className="link-btn small" onClick={showPreview}>
              {preview ? t('hidePreview') : t('previewPrompt')}
            </button>
            {revisions.length > 1 && (
              <button className="link-btn small" onClick={() => setShowRevs(!showRevs)}>
                {t('revision', { n: c.promptRevision, total: revisions.length })}
              </button>
            )}
          </span>
        </div>

        {sops.length === 0 ? (
          <p className="muted small">{t('sopsEmpty')}</p>
        ) : (
          <div className="bot-sop-grid">
            {sops.map((s) => {
              const linked = routines.filter((r) => r.sopId === s.id);
              return (
                <div key={s.id} className={`bot-sop-card ${s.enabled ? '' : 'off'}`}>
                  <div className="bot-sop-head">
                    <span className="pill mono">{s.id}</span>
                    <strong className="truncate">{s.name}</strong>
                    <span className="bot-bar-fill" />
                    <span className={`pill ${s.enabled ? 'bot-sop-on' : ''}`}>
                      {s.enabled ? t('sopEnabled') : t('sopDisabled')}
                    </span>
                  </div>
                  {linked.length > 0 && (
                    <div className="muted small">
                      <Icon name="clock" size={11} /> {t('sopLinkedRoutines', { names: linked.map((r) => r.name).join(t('listSep')) })}
                    </div>
                  )}
                  <dl className="bot-sop-dl">
                    <dt>{t('sopWhen')}</dt>
                    <dd>{s.when}</dd>
                    <dt>{t('sopHow')}</dt>
                    <dd>{s.how}</dd>
                    {s.askHuman && (
                      <>
                        <dt>{t('sopAskHuman')}</dt>
                        <dd>{s.askHuman}</dd>
                      </>
                    )}
                    {s.delivery && (
                      <>
                        <dt>{t('sopDelivery')}</dt>
                        <dd>{s.delivery}</dd>
                      </>
                    )}
                  </dl>
                </div>
              );
            })}
          </div>
        )}

        {sops.length > 0 && (
          <button className="link-btn small bot-subtoggle" onClick={() => setShowBasePrompt(!showBasePrompt)}>
            <Icon name={baseVisible ? 'down' : 'chevron'} size={12} /> {baseVisible ? t('hideBasePrompt') : t('showBasePrompt')}
          </button>
        )}

        {baseVisible &&
          sections.map(([label, text, fallback]) => (
            <div key={label} className="bot-section">
              <div className="bot-section-label">{label}</div>
              {text.trim() ? (
                <div className="bot-section-text">{text}</div>
              ) : (
                <div className="bot-section-text muted">{fallback || t('notSet')}</div>
              )}
            </div>
          ))}

        {showRevs && (
          <div className="bot-revs">
            {revisions.map((r) => {
              const prev = revisions.find((x) => x.revision === r.baseRevision) ?? revisions.find((x) => x.revision < r.revision);
              return (
                <div key={r.revision} className="bot-rev">
                  <span>{t('revisionN', { n: r.revision })}</span>
                  <span className="muted small">
                    {originLabel(r.origin)} · {fmtTime(r.at)}
                  </span>
                  <button className="link-btn small" onClick={() => setDiffOf(diffOf === r.revision ? null : r.revision)}>
                    {t('showChanges')}
                  </button>
                  {r.revision === c.promptRevision ? (
                    <span className="pill">{t('current')}</span>
                  ) : (
                    <button
                      className="link-btn small"
                      onClick={async () => {
                        if (!(await askConfirm({ title: t('restoreTitle', { n: r.revision }), message: t('restoreMessage') }))) return;
                        try {
                          const saved = await botCall<BotConfig>('bots.promptRestore', { id: botId, revision: r.revision });
                          setRow({ ...row, config: saved });
                          setRevisions(await botCall<PromptRevision[]>('bots.prompts', { id: botId }));
                          toast(t('restored', { n: saved.promptRevision }));
                        } catch (e) {
                          errorToast(e);
                        }
                      }}
                    >
                      {t('restoreRevision')}
                    </button>
                  )}
                  {diffOf === r.revision && (
                    <div className="bot-rev-diff">
                      <PromptDiff
                        from={prev?.sections ?? null}
                        to={r.sections}
                        fromSops={prev?.sops ?? null}
                        toSops={r.sops ?? []}
                      />
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}
        {preview && (
          <>
            <div className="field">
              {t('previewBot')}
              <pre className="bot-preview">{preview.bot}</pre>
            </div>
            <div className="field">
              {t('previewTask')}
              <pre className="bot-preview">{preview.task}</pre>
            </div>
          </>
        )}
      </div>

      {/* Section 3: Tiered Memories & Scratchpad Notes */}
      <MemoriesCard
        botId={botId}
        config={c}
        memories={memories}
        notes={row.notes}
        hostName={hostName}
        onMemoriesChange={loadMemories}
        onNotesSaved={(next) => setRow({ ...row, notes: next })}
      />

      {/* Section 4: Connectors, Workspaces, Routines & Inbox/Outbox */}
      <div className="card">
        <div className="card-title">
          {t('connectorsAndSetup')}
          <span className="row">
            <button className="link-btn small" onClick={() => navigate({ kind: 'botEvents', botId, src: src || undefined })}>
              {t('viewInboxLog')}
            </button>
            <button className="link-btn small" onClick={() => navigate({ kind: 'connectors', src: src || undefined })}>
              {t('managePlugins')}
            </button>
          </span>
        </div>
        <dl className="bot-kv">
          <Row label={t('triggers')}>{c.triggers.length ? names(c.triggers) : <span className="muted">{t('onlyThisSession')}</span>}</Row>
          <Row label={t('notify')}>{names(c.notify)}</Row>
          <Row label={t('acceptLabel')}>
            {acceptLabel} <span className="muted small">{acceptHint}</span>
          </Row>
          <Row label={t('ambientLabel')}>
            {c.ambient === 'deliver' ? t('ambientDeliver') : t('ambientRecord')}{' '}
            <span className="muted small">{c.ambient === 'deliver' ? t('ambientDeliverHint') : t('ambientRecordHint')}</span>
          </Row>
          <Row label={t('approvalLabel')}>
            {approvalLabel} <span className="muted small">{t('approvalAutoOnly')}</span>
          </Row>
          <Row label={t('activeHoursLabel')}>
            {hoursLine} {hours && <span className="muted small">{t('activeHoursNote')}</span>}
          </Row>
          <Row label={t('tokenReminderLabel')}>
            {c.budget.tokensPerDay ? t('tokenReminderLine', { n: c.budget.tokensPerDay.toLocaleString() }) : t('tokenReminderOff')}
          </Row>
          {c.allow.length > 0 && (
            <Row label={t('allowList')}>
              {c.allow.map((a) => (
                <div key={a.instance} className="small">
                  {instName(a.instance)}: <span className="mono">{a.ids.join(', ')}</span>
                </div>
              ))}
            </Row>
          )}
          <Row label={t('botSession')}>
            {toolLine(c.bot.tool, c.bot.model, c.bot.effort)} · {hostName(c.bot.host)}
            {c.bot.permissionMode && <span className="muted small"> · {permLabel(c.bot.permissionMode)}</span>}
          </Row>
          <Row label={t('workspaces')}>
            {c.workspaces.length === 0 ? (
              <span className="muted">{t('none')}</span>
            ) : (
              c.workspaces.map((w, i) => (
                <div key={i} className="small">
                  {w.name && <strong>{w.name} · </strong>}
                  <span className="mono">
                    {hostName(w.host)}: {w.cwd}
                  </span>
                  {w.about && <span className="muted"> — {w.about}</span>}
                </div>
              ))
            )}
          </Row>
          <Row label={t('tasks')}>
            {toolLine(c.task.tool, c.task.model, c.task.effort)}
            {(c.task.tools ?? []).length > 0 && (
              <span className="muted small"> + {(c.task.tools ?? []).map((x) => TOOL_LABEL[x] ?? x).join(', ')}</span>
            )}
            <div className="muted small">
              {[
                permLabel(c.task.permissionMode),
                c.task.computerUse ? t('computerUseOnDemand') : null,
                t('limitsLine', {
                  n: c.task.maxConcurrent,
                  timeout: c.task.timeoutMinutes ? t('minutes', { n: c.task.timeoutMinutes }) : t('never'),
                  perDay: c.budget.tasksPerDay || t('unlimited'),
                }),
                (c.task.keepDays ?? 7) > 0 ? t('keepDays', { n: c.task.keepDays ?? 7 }) : t('keepForever'),
              ]
                .filter(Boolean)
                .join(' · ')}
            </div>
          </Row>
        </dl>
        <BotOutboxPanel botId={botId} instName={instName} />
      </div>

      {entry && <RoutinesCard entry={entry} />}

      <StatsCard botId={botId} />

      <EventsCard botId={botId} />

      <div className="card bot-danger">
        <div className="grow">
          <div className="card-title">{t('deleteBot')}</div>
          <p className="muted small">{t('deleteBotMessage')}</p>
        </div>
        <button className="btn" onClick={() => deleteBotWithConfirm(entry ?? { config: c, session: row.session }, src)}>
          <Icon name="trash" size={14} /> {t('deleteBot')}
        </button>
      </div>
    </div>
  );
}
