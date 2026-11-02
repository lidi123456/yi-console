import { useEffect, useState } from 'react';
import { isEn, tr } from '../i18n';
import { errorToast } from '../store';
import { rpc } from '../transport';
import type { BackgroundTask, SessionInfo } from '../types';
import { Icon } from './Icon';

function kindLabel(k: string) {
  switch (k) {
    case 'local_bash':
      return tr('终端命令', 'Bash Command');
    case 'local_agent':
      return tr('子代理', 'Subagent');
    case 'remote_agent':
      return tr('远程代理', 'Remote Agent');
    default:
      return k;
  }
}

/** Collapse multi-line commands (heredocs etc.) into one line for display; the full text goes in the title */
function oneLine(s: string) {
  return s.replace(/\s+/g, ' ').trim();
}

function fmtElapsed(ms: number) {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

/** Background task output: read the tail of the output file, refreshing every 2 s while open */
function TaskOutput({ hostId, path }: { hostId: string; path: string }) {
  const [text, setText] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    const load = () =>
      rpc<{ content: string; truncated: boolean }>(hostId, 'fs.read', { path, tail: true })
        .then((r) => alive && setText(r.content.split('\n').slice(-200).join('\n')))
        .catch(() => alive && setText(''));
    load();
    const t = setInterval(load, 2000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [hostId, path]);
  return <pre className="mono bg-output">{text === null ? tr('读取中…', 'Loading…') : text || tr('（还没有输出）', '(no output yet)')}</pre>;
}

/** Background task bar above the input box: one line normally, expands to list each task, with output and stop */
export function BgTasks({ hostId, info }: { hostId: string; info: SessionInfo }) {
  const tasks = info.backgroundTasks ?? [];
  const [open, setOpen] = useState(false);
  const [viewing, setViewing] = useState<string | null>(null);
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!tasks.length) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [tasks.length]);
  if (!tasks.length) return null;

  const stop = async (t: BackgroundTask) => {
    try {
      await rpc(hostId, 'session.stopTask', { sessionId: info.id, taskId: t.id });
    } catch (e) {
      errorToast(e);
    }
  };

  return (
    <div className={`bg-tasks ${open ? 'open' : ''}`}>
      <button className="bg-tasks-head" onClick={() => setOpen(!open)}>
        <span className="spinner" />
        <span className="bg-tasks-count">{tr(`${tasks.length} 个后台任务`, `${tasks.length} background task(s)`)}</span>
        {!open && (
          <span className="bg-tasks-sum truncate" title={tasks.map((t) => t.description || t.id).join('\n')}>
            {tasks.map((t) => oneLine(t.description || t.id)).join(isEn() ? ', ' : '、')}
          </span>
        )}
        {!open && tasks.length === 1 && <span className="bg-tasks-time">{fmtElapsed(now - tasks[0].startedAt)}</span>}
        <span className={`chev ${open ? 'open' : ''}`}>
          <Icon name="chevron" size={12} />
        </span>
      </button>
      {open &&
        tasks.map((t) => (
          <div key={t.id} className="bg-task">
            <div className="bg-task-row">
              <Icon name={t.kind === 'local_bash' ? 'terminal' : 'chat'} size={14} />
              <span className="bg-task-kind">{kindLabel(t.kind)}</span>
              <span className="bg-task-desc truncate" title={`${t.description}\n${t.id}`}>
                {oneLine(t.description || t.id)}
              </span>
              <span className="muted small">{fmtElapsed(now - t.startedAt)}</span>
              {t.outputFile && (
                <button className="btn small" onClick={() => setViewing(viewing === t.id ? null : t.id)}>
                  {viewing === t.id ? tr('收起', 'Hide') : tr('输出', 'Output')}
                </button>
              )}
              <button className="btn small danger-ghost" onClick={() => stop(t)}>
                {tr('停止', 'Stop')}
              </button>
            </div>
            {viewing === t.id && t.outputFile && <TaskOutput hostId={hostId} path={t.outputFile} />}
          </div>
        ))}
    </div>
  );
}
