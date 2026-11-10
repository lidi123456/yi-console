// What a Bot has been doing: the full event log page, the activity / budget card and prompt revision diffs.

import { useEffect, useState } from 'react';
import { diffLines } from 'diff';
import { getT, isEn, useLang } from '../i18n';
import { errorToast, navigate } from '../store';
import { findTask, useBotCall, useBotList, useBotSource, type BotConfig, type EventRow } from '../bots';
import { openOn, taskStateLabel } from './BotsSection';
import { openUrl } from './PortsView';
import { Icon } from './Icon';

const t = getT('bots');

export const DISPOSITIONS = ['queued', 'held', 'attached', 'spawned', 'observed', 'ignored', 'filtered'];

export function fmtTime(ms: number) {
  return new Date(ms).toLocaleString(isEn() ? 'en' : 'zh-CN', { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' });
}

export function dispositionLabel(d: string) {
  switch (d) {
    case 'queued':
      return t('dispQueued');
    case 'held':
      return t('dispHeld');
    case 'attached':
      return t('dispAttached');
    case 'spawned':
      return t('dispSpawned');
    case 'observed':
      return t('dispObserved');
    case 'ignored':
      return t('dispIgnored');
    case 'filtered':
      return t('dispFiltered');
    default:
      return d;
  }
}

/** One event line; the task it went to opens on click */
export function EventLine({ e, src, full }: { e: EventRow; src: string; full?: boolean }) {
  const task = e.taskId ? findTask(e.taskId, src) : undefined;
  return (
    <div className={`bot-event ${full ? 'full' : ''}`} title={full ? undefined : e.note || undefined}>
      <span className="muted small">{fmtTime(e.at)}</span>
      <span className="bot-event-text">
        <b>{e.senderName || e.sender}</b> {e.text}
        {full && e.note && <span className="bot-event-note muted small">{e.note}</span>}
      </span>
      {full && e.url && (
        <button className="icon-btn" title={t('openSource')} onClick={() => openUrl(e.url!)}>
          <Icon name="external" size={13} />
        </button>
      )}
      {full && task?.sessionId && (
        <button className="icon-btn" title={t('openTask')} onClick={() => openOn(src, task.host, task.sessionId!)}>
          <Icon name="chat" size={13} />
        </button>
      )}
      <span className={`pill bot-disp-${e.disposition}`}>{dispositionLabel(e.disposition)}</span>
    </div>
  );
}

const PAGE = 100;

/** Every event a Bot received, newest first, filtered by what happened to it */
export function BotEventsView({ botId }: { botId: string }) {
  useLang();
  const src = useBotSource();
  const botCall = useBotCall();
  const entry = useBotList(src)?.find((b) => b.config.id === botId);
  const [filter, setFilter] = useState('');
  const [rows, setRows] = useState<EventRow[] | null>(null);
  const [more, setMore] = useState(false);

  const load = (before?: number) =>
    botCall<EventRow[]>('bots.events', { id: botId, limit: PAGE, before, disposition: filter || undefined })
      .then((r) => {
        setRows((old) => (before && old ? [...old, ...r] : r));
        setMore(r.length === PAGE);
      })
      .catch(errorToast);
  useEffect(() => {
    setRows(null);
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [botId, filter]);

  return (
    <div className="page bot-page">
      <div className="page-head">
        <h2>
          <button className="icon-btn" title={t('backToBot')} onClick={() => navigate({ kind: 'bot', botId, src: src || undefined })}>
            <Icon name="back" size={16} />
          </button>
          {t('eventsOf', { name: entry?.config.name ?? botId })}
        </h2>
        <div className="row">
          <select value={filter} onChange={(e) => setFilter(e.target.value)}>
            <option value="">{t('allEvents')}</option>
            {DISPOSITIONS.map((d) => (
              <option key={d} value={d}>
                {dispositionLabel(d)}
              </option>
            ))}
          </select>
          <button className="icon-btn" title={t('refresh')} onClick={() => load()}>
            <Icon name="refresh" size={14} />
          </button>
        </div>
      </div>
      <div className="card">
        {!rows ? (
          <p className="muted small">{t('loading')}</p>
        ) : rows.length === 0 ? (
          <p className="muted small">{t('noEvents')}</p>
        ) : (
          <div className="bot-events">
            {rows.map((e) => (
              <EventLine key={e.id} e={e} src={src} full />
            ))}
          </div>
        )}
        {rows && more && (
          <button className="link-btn small" onClick={() => load(rows[rows.length - 1]?.at)}>
            {t('loadMore')}
          </button>
        )}
      </div>
    </div>
  );
}

interface Stats {
  tasksPerDay: number;
  tasksToday: number;
  tokensPerDay?: number;
  tokensToday?: number;
  events: Record<string, number>;
  tasks: Record<string, number>;
  days: { day: string; events: Record<string, number>; tasks: Record<string, number> }[];
}

const sum = (m: Record<string, number>) => Object.values(m).reduce((a, b) => a + b, 0);

/** Today's budget use and the last two weeks of events and tasks */
export function StatsCard({ botId }: { botId: string }) {
  const botCall = useBotCall();
  const [s, setS] = useState<Stats | null>(null);
  useEffect(() => {
    botCall<Stats>('bots.stats', { id: botId, days: 14 }).then(setS).catch(errorToast);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [botId]);
  if (!s) return null;
  const peak = Math.max(1, ...s.days.map((d) => Math.max(sum(d.events), sum(d.tasks))));
  const used = s.tasksPerDay > 0 ? Math.min(1, s.tasksToday / s.tasksPerDay) : 0;
  const tokMax = s.tokensPerDay ?? 0;
  const tok = s.tokensToday ?? 0;
  const tokUsed = tokMax > 0 ? Math.min(1, tok / tokMax) : 0;
  return (
    <div className="card">
      <div className="card-title">{t('activity')}</div>
      <div className="bot-budget">
        <span>{s.tasksPerDay > 0 ? t('budgetUsed', { n: s.tasksToday, max: s.tasksPerDay }) : t('budgetUnlimited', { n: s.tasksToday })}</span>
        {s.tasksPerDay > 0 && (
          <span className={`bot-budget-bar ${used >= 1 ? 'full' : used >= 0.8 ? 'high' : ''}`}>
            <span style={{ width: `${used * 100}%` }} />
          </span>
        )}
      </div>
      <div className="bot-budget" title={t('tokensNote')}>
        <span>{tokMax > 0 ? t('tokensTodayOf', { n: tok.toLocaleString(), max: tokMax.toLocaleString() }) : t('tokensToday', { n: tok.toLocaleString() })}</span>
        {tokMax > 0 && (
          <span className={`bot-budget-bar ${tokUsed >= 1 ? 'full' : tokUsed >= 0.8 ? 'high' : ''}`}>
            <span style={{ width: `${tokUsed * 100}%` }} />
          </span>
        )}
      </div>
      <div className="bot-chart" title={t('chartHint')}>
        {s.days.map((d) => (
          <div key={d.day} className="bot-chart-day" title={t('dayStats', { day: d.day, events: sum(d.events), tasks: sum(d.tasks) })}>
            <span className="bar events" style={{ height: `${(sum(d.events) / peak) * 100}%` }} />
            <span className="bar tasks" style={{ height: `${(sum(d.tasks) / peak) * 100}%` }} />
          </div>
        ))}
      </div>
      <div className="bot-chart-legend muted small">
        <span>
          <i className="dot events" /> {t('legendEvents')}
        </span>
        <span>
          <i className="dot tasks" /> {t('legendTasks')}
        </span>
      </div>
      <div className="bot-stat-pills">
        {DISPOSITIONS.filter((d) => s.events[d]).map((d) => (
          <span key={d} className={`pill bot-disp-${d}`}>
            {dispositionLabel(d)} {s.events[d]}
          </span>
        ))}
        {Object.entries(s.tasks).map(([k, n]) => (
          <span key={k} className="pill">
            {taskStateLabel(k)} {n}
          </span>
        ))}
      </div>
    </div>
  );
}

const SECTIONS: (keyof BotConfig['prompt'])[] = ['role', 'communication', 'delivery'];

/** Line diff of the three prompt sections between two revisions */
export function PromptDiff({ from, to }: { from: BotConfig['prompt'] | null; to: BotConfig['prompt'] }) {
  const label = (k: string) => (k === 'role' ? t('promptRole') : k === 'communication' ? t('promptCommunication') : t('promptDelivery'));
  const changed = SECTIONS.filter((k) => (from?.[k] ?? '') !== to[k]);
  if (changed.length === 0) return <p className="muted small">{t('noPromptChange')}</p>;
  return (
    <div className="bot-diff">
      {changed.map((k) => (
        <div key={k}>
          <div className="muted small">{label(k)}</div>
          <pre className="bot-diff-body">
            {diffLines(from?.[k] ?? '', to[k]).map((p, i) => (
              <span key={i} className={p.added ? 'dl-add' : p.removed ? 'dl-del' : 'bot-diff-ctx'}>
                {p.value
                  .replace(/\n$/, '')
                  .split('\n')
                  .map((l) => `${p.added ? '+ ' : p.removed ? '- ' : '  '}${l}`)
                  .join('\n') + '\n'}
              </span>
            ))}
          </pre>
        </div>
      ))}
    </div>
  );
}
