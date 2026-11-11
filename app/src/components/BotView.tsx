// Bot details page (desktop app only, or a remote control of one): what the Bot is set up to do, its prompt
// revisions, notes, stats and recent events. Read-only: the owner changes a Bot by talking to it in its session
// (it saves with update_config); here are only pause / resume, restoring a prompt revision and delete.

import { useEffect, useState } from 'react';
import { getT, useLang } from '../i18n';
import { errorToast, navigate, toast } from '../store';
import { TOOL_LABEL } from '../types';
import { permLabel } from '../perm';
import { useBotCall, useBotList, useBotSource, type BotConfig, type BotEntry, type EventRow, type PluginInstance, type RoutineRun } from '../bots';
import { ApprovalCards, SecretCards, botMenu, botStateLabel, openOn, useSourceHosts } from './BotsSection';
import { EventLine, fmtTime, PromptDiff, StatsCard } from './BotActivity';
import { askConfirm, openMenu } from './Overlay';
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
  origin: string;
  at: number;
}

function originLabel(o: string) {
  return o === 'create' ? t('originCreate') : o === 'ai' || o === 'bot' ? t('originBot') : o === 'restore' ? t('originRestore') : t('originForm');
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

/** Routines: schedule, last run, run now and the run history. Changing them is done by talking to the Bot */
function RoutinesCard({ entry }: { entry: BotEntry }) {
  const botCall = useBotCall();
  const [open, setOpen] = useState<string | null>(null);
  const [runs, setRuns] = useState<RoutineRun[]>([]);
  const routines = entry.config.routines ?? [];
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
          return (
            <div key={r.id} className={`bot-routine ${r.enabled ? '' : 'off'}`}>
              <div className="bot-routine-head">
                <span className="bot-routine-name">{r.name}</span>
                <span className="mono small muted">{r.schedule}</span>
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
  const [showRevs, setShowRevs] = useState(false);
  /** revision whose diff against the one it was based on is open */
  const [diffOf, setDiffOf] = useState<number | null>(null);
  const [instances, setInstances] = useState<PluginInstance[]>([]);
  const [preview, setPreview] = useState<{ bot: string; task: string } | null>(null);
  const [defaults, setDefaults] = useState<Record<string, string>>({});
  // The list changes whenever the Bot saves; reload the full row then
  const stamp = entry ? JSON.stringify(entry.config) : '';

  useEffect(() => {
    if (!available) return;
    botCall<{ bot: BotRowFull; prompts: PromptRevision[] }>('bots.get', { id: botId })
      .then((r) => {
        setRow(r.bot);
        setRevisions(r.prompts);
      })
      .catch(errorToast);
    botCall<PluginInstance[]>('plugins.instances').then(setInstances).catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [botId, available, src, stamp]);
  useEffect(() => {
    if (available) botCall<Record<string, string>>('bots.defaults').then(setDefaults).catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [available, src]);

  if (!available) return <div className="page muted">{t('desktopOnly')}</div>;
  if (!row) return <div className="page muted">{t('loading')}</div>;
  const c = row.config;
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
    // The built-in default, without its "# Communication defaults" heading
    [t('promptCommunication'), c.prompt.communication, (defaults.communication ?? '').replace(/^#.*\n+/, '').trim()],
    [t('promptDelivery'), c.prompt.delivery, ''],
  ];

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
      {entry && <SecretCards bot={entry} src={src} inline />}
      {entry && (entry.approvals?.length ?? 0) > 0 && <ApprovalCards bot={entry} src={src} inline />}
      {entry && entry.attention.length > 0 && (
        <div className="card">
          <div className="card-title">{t('needsYou')}</div>
          {entry.attention.map((a) => (
            <div key={a.id} className="bot-attn">
              <span>{a.text}</span>
              <button className="link-btn small" onClick={() => botCall('bots.resolve', { id: a.id }).catch(errorToast)}>
                {t('resolve')}
              </button>
            </div>
          ))}
        </div>
      )}

      <div className="card">
        <div className="card-title">{t('setup')}</div>
        <dl className="bot-kv">
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
          <Row label={t('triggers')}>{c.triggers.length ? names(c.triggers) : <span className="muted">{t('onlyThisSession')}</span>}</Row>
          <Row label={t('notify')}>{names(c.notify)}</Row>
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
                <div key={i} className="mono small">
                  {hostName(w.host)}: {w.cwd}
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
                c.task.worktree ? t('worktreeOn') : t('noWorktree'),
                t('limitsLine', {
                  n: c.task.maxConcurrent,
                  timeout: c.task.timeoutMinutes ? t('minutes', { n: c.task.timeoutMinutes }) : t('never'),
                  perDay: c.budget.tasksPerDay || t('unlimited'),
                }),
                (c.task.keepDays ?? 7) > 0 ? t('keepDays', { n: c.task.keepDays ?? 7 }) : t('keepForever'),
              ].join(' · ')}
            </div>
          </Row>
        </dl>
      </div>

      {entry && <RoutinesCard entry={entry} />}

      <div className="card">
        <div className="card-title">
          {t('prompt')}
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
        {sections.map(([label, text, fallback]) => (
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
            {revisions.map((r) => (
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
                      from={(revisions.find((x) => x.revision === r.baseRevision) ?? revisions.find((x) => x.revision < r.revision))?.sections ?? null}
                      to={r.sections}
                    />
                  </div>
                )}
              </div>
            ))}
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

      {row.notes.trim() && (
        <div className="card">
          <div className="card-title">{t('notes')}</div>
          <p className="muted small">{t('notesHint')}</p>
          <div className="bot-section-text">{row.notes}</div>
        </div>
      )}

      <StatsCard botId={botId} />

      <EventsCard botId={botId} />

      <div className="card bot-danger">
        <div className="grow">
          <div className="card-title">{t('deleteBot')}</div>
          <p className="muted small">{t('deleteBotMessage')}</p>
        </div>
        <button
          className="btn"
          onClick={async () => {
            if (!(await askConfirm({ title: t('deleteBotTitle', { name: c.name }), message: t('deleteBotMessage'), danger: true }))) return;
            try {
              await botCall('bots.delete', { id: botId, sessions: false });
              navigate({ kind: 'home' });
            } catch (e) {
              errorToast(e);
            }
          }}
        >
          <Icon name="trash" size={14} /> {t('deleteBot')}
        </button>
      </div>
    </div>
  );
}
