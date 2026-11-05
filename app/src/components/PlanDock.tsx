import { useMemo, useState } from 'react';
import { getT } from '../i18n';
import { currentEntry, latestPlan, viewEntries } from '../plan';
import type { HubEvent } from '../types';
import { PlanList } from './ChatView';
import { Icon } from './Icon';

const t = getT('planDock');

/** Task progress above the input box: while tasks remain, one line "Tasks 3/7 · Now: …", click for the full list;
 *  disappears when all are done or the list's turn ends (`latestPlan`) */
export function PlanDock({ events, busy }: { events: HubEvent[]; busy: boolean }) {
  const plan = useMemo(() => latestPlan(events), [events, events.length]);
  const [open, setOpen] = useState(false);
  if (!plan) return null;
  const entries = viewEntries(plan, busy);
  const done = entries.filter((e) => e.status === 'completed').length;
  const cur = currentEntry(entries);
  if (!cur) return null;
  return (
    <div className={`bg-tasks plan-dock ${open ? 'open' : ''}`}>
      <button className="bg-tasks-head" onClick={() => setOpen(!open)} title={plan.title}>
        {cur.active && busy ? <span className="spinner" /> : <span className="plan-dock-icon">☐</span>}
        <span className="bg-tasks-count">{t('tasks', { done, length: entries.length })}</span>
        <span className="bg-tasks-sum plan-dock-cur truncate" title={cur.entry.content}>
          {(cur.active ? t('now') : t('next')) + cur.entry.content}
        </span>
        <span className={`chev ${open ? 'open' : ''}`}>
          <Icon name="chevron" size={12} />
        </span>
      </button>
      {open && (
        <div className="plan-dock-list">
          {plan.title && <div className="plan-dock-title">{plan.title}</div>}
          <PlanList entries={entries} />
        </div>
      )}
    </div>
  );
}
