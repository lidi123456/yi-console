// Overview board grouped by Bot: every Bot of this app and of the remote desktops, the ones that need you first.

import { getT, useLang } from '../i18n';
import { errorToast, useStore } from '../store';
import { ACTIVE_TASK, botCall, useBots, type BotEntry, type TaskRow } from '../bots';
import {
  TaskAnsweredBanner,
  TaskQuestionCard,
  answeredWaitingTasks,
  botDot,
  botMenu,
  botStateLabel,
  localHost,
  needsYouCount,
  openOn,
  questionTasks,
  taskDot,
  taskMenu,
  taskStateLabel,
  visibleAttention,
} from './BotsSection';
import { openMenu } from './Overlay';
import { Icon, ToolBadge } from './Icon';
import { timeAgo } from '../util';

const t = getT('bots');

interface Row {
  b: BotEntry;
  src: string;
  /** Device name for remote desktops */
  device?: string;
}

/** Needs you (including pending changes, drafts, secrets, decision cards), then error, busy, idle, disabled */
function rank(b: BotEntry) {
  if (needsYouCount(b) > 0 || b.tasks.some((x) => x.state === 'needsYou')) return 0;
  if (b.state === 'error') return 1;
  if (b.tasks.some((x) => ACTIVE_TASK.includes(x.state)) || b.state === 'running') return 2;
  return b.config.enabled ? 3 : 4;
}

export function useBotRows(): Row[] {
  const st = useBots((s) => s);
  const rows: Row[] = [];
  for (const b of st.list ?? []) rows.push({ b, src: '' });
  for (const r of st.remotes) for (const b of r.list ?? []) rows.push({ b, src: r.key, device: r.name });
  return rows.sort((x, y) => rank(x.b) - rank(y.b) || x.b.config.name.localeCompare(y.b.config.name));
}

function BoardTask({ task, src }: { task: TaskRow; src: string }) {
  const dot = taskDot(task.state);
  const hostName = useStore((s) => s.hosts.find((h) => h.config.id === localHost(src, task.host))?.config.name ?? task.host);
  return (
    <button
      className="bot-card-task"
      onClick={() => task.sessionId && openOn(src, task.host, task.sessionId)}
      onContextMenu={(e) => openMenu(e, taskMenu(task, src))}
    >
      <ToolBadge tool={String(task.spec.tool ?? 'claude')} size={14} />
      <span className="bot-card-task-text">
        <span className="truncate">{task.title}</span>
        {task.lastReport?.question ? (
          <span className="muted small truncate">{task.lastReport.question}</span>
        ) : task.lastReport?.summary ? (
          <span className="muted small truncate">{task.lastReport.summary}</span>
        ) : null}
      </span>
      <span className="muted small">{hostName}</span>
      {dot && <span className={`sdot sdot-${dot}`} />}
      <span className="small">{taskStateLabel(task.state)}</span>
    </button>
  );
}

function BotCard({ b, src, device }: Row) {
  const dot = botDot(b);
  const questions = questionTasks(b);
  const answered = answeredWaitingTasks(b);
  const attn = visibleAttention(b);
  const active = b.tasks.filter((x) => ACTIVE_TASK.includes(x.state));
  const ready = b.tasks.filter((x) => x.state === 'ready');
  const today = new Date().setHours(0, 0, 0, 0);
  const endedToday = b.tasks.filter((x) => !ACTIVE_TASK.includes(x.state) && (x.endedAt ?? 0) >= today).length;
  return (
    <div className={`card bot-card ${rank(b) === 0 ? 'attn' : ''} ${b.config.enabled ? '' : 's-off'}`}>
      <div className="bot-card-head">
        <Icon name="bot" size={16} />
        <button className="bot-card-name" onClick={() => b.session && openOn(src, b.session.host, b.session.sessionId)} title={t('openBotSession')}>
          {b.config.name}
        </button>
        {device && <span className="pill">{device}</span>}
        <span className="grow" />
        {dot && <span className={`sdot sdot-${dot}`} />}
        <span className="muted small">{botStateLabel(b.config.enabled ? b.state : 'disabled')}</span>
        <button
          className="icon-btn"
          title={t('more')}
          onClick={(e) => {
            const r = e.currentTarget.getBoundingClientRect();
            openMenu({ clientX: r.left, clientY: r.bottom + 2 }, botMenu(b, src));
          }}
        >
          <Icon name="more" size={14} />
        </button>
      </div>
      {b.state === 'error' && b.error && <div className="bot-card-err small">{b.error}</div>}
      {questions.map((qt) => (
        <TaskQuestionCard key={qt.id} task={qt} src={src} showTaskLink compact />
      ))}
      {answered.map((at) => (
        <TaskAnsweredBanner key={at.id} task={at} />
      ))}
      {attn.map((a) => (
        <div key={a.id} className="bot-card-attn">
          <span className="sdot sdot-attn" />
          <span className="grow">{a.text}</span>
          <span className="muted small">{timeAgo(a.at)}</span>
          <button className="link-btn small" onClick={() => botCall('bots.resolve', { id: a.id }, src).catch(errorToast)}>
            {t('resolve')}
          </button>
        </div>
      ))}
      {[...active, ...ready].map((x) => (
        <BoardTask key={x.id} task={x} src={src} />
      ))}
      {active.length === 0 && ready.length === 0 && attn.length === 0 && questions.length === 0 && <div className="muted small">{t('boardIdle')}</div>}
      <div className="bot-card-foot muted small">
        {b.waiting > 0 && <span>{t('boardWaiting', { n: b.waiting })}</span>}
        {endedToday > 0 && <span>{t('boardEndedToday', { n: endedToday })}</span>}
      </div>
    </div>
  );
}

export function BotBoard() {
  useLang();
  const rows = useBotRows();
  if (rows.length === 0) return <div className="muted">{t('noBots')}</div>;
  return (
    <div className="bot-board">
      {rows.map((r) => (
        <BotCard key={`${r.src}/${r.b.config.id}`} {...r} />
      ))}
    </div>
  );
}
