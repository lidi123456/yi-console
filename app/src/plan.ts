// Task lists: normalize every tool's task list into { title, entries }.
// - Claude TodoWrite, Codex turn/plan/updated, ACP plan: yi-agentd already turns these into plan events, use them directly
// - Kiro's todo_list tool: each call's result is the full list (only done / not done), and it's cleared once everything is done
// - Newer Claude TaskCreate / TaskUpdate: creation returns a number in the result ("Task #N created successfully: …"), later updates change status by number
import type { HubEvent, PlanEntry } from './types';

export interface PlanState {
  title?: string;
  entries: PlanEntry[];
  /** The tool itself has no "in progress" (Kiro): while the turn runs, treat the first unfinished item as in progress */
  infer?: boolean;
}

/** These tool calls are shown by the task list instead (failed ones are shown as usual) */
export const PLAN_TOOLS = new Set(['TodoWrite', 'todo_list', 'TaskCreate', 'TaskUpdate']);

type Rec = Record<string, unknown>;
const str = (v: unknown) => (typeof v === 'string' ? v : v == null ? '' : String(v));

/** Kiro todo_list result: {"items":[{"Json":{description, tasks:[{id, task_description, completed}]}}]}, possibly wrapped in another string */
function kiroList(raw: string): { title: string; tasks: { done: boolean; text: string }[] } | null {
  let v: unknown = raw;
  for (let i = 0; i < 2 && typeof v === 'string'; i++) {
    try {
      v = JSON.parse(v);
    } catch {
      return null;
    }
  }
  if (!v || typeof v !== 'object') return null;
  let o = v as Rec;
  const first = Array.isArray(o.items) ? (o.items[0] as Rec | undefined) : undefined;
  if (first?.Json && typeof first.Json === 'object') o = first.Json as Rec;
  if (!Array.isArray(o.tasks)) return null;
  return {
    title: str(o.description),
    tasks: (o.tasks as Rec[]).map((t) => ({ done: !!t.completed, text: str(t.task_description ?? t.content) })),
  };
}

export class PlanTracker {
  plan: PlanState | null = null;
  private calls = new Map<string, { name: string; input: Rec }>();
  /** Tasks created by Claude TaskCreate, keyed by number, in creation order */
  private tasks = new Map<string, PlanEntry>();

  /** Process one event; returns true if the task list changed */
  feed(e: HubEvent): boolean {
    switch (e.kind) {
      case 'cleared':
        this.plan = null;
        this.calls.clear();
        this.tasks.clear();
        return false;
      case 'plan':
        this.plan = { entries: e.entries };
        return true;
      case 'tool_call': {
        if (!PLAN_TOOLS.has(e.name)) return false;
        const input = (e.input && typeof e.input === 'object' ? e.input : {}) as Rec;
        this.calls.set(e.id, { name: e.name, input });
        if (e.name !== 'TaskUpdate') return false;
        // Status updates don't wait for the result: for a missing number the result is "Task #N not found", and it isn't found here either, so it's naturally ignored
        const t = this.tasks.get(str(input.taskId));
        if (!t) return false;
        const status = str(input.status);
        if (status === 'deleted') this.tasks.delete(str(input.taskId));
        else {
          if (status) t.status = status;
          if (typeof input.subject === 'string' && input.subject) t.content = input.subject;
        }
        return this.syncTasks();
      }
      case 'tool_result': {
        const c = this.calls.get(e.id);
        if (!c || e.isError) return false;
        if (c.name === 'TaskCreate') {
          const m = /^Task #(\S+) created successfully/.exec(e.output.trim());
          if (!m) return false;
          this.tasks.set(m[1], { content: str(c.input.subject), status: 'pending' });
          return this.syncTasks();
        }
        if (c.name === 'todo_list') {
          const l = kiroList(e.output);
          if (!l) return false;
          if (!l.tasks.length) {
            // Kiro clears the list once everything is done: reuse the previous one with every item checked
            if (str(c.input.command) === 'complete' && this.plan?.entries.length) {
              this.plan = { ...this.plan, entries: this.plan.entries.map((x) => ({ ...x, status: 'completed' })) };
              return true;
            }
            this.plan = null;
            return true;
          }
          this.plan = {
            title: l.title || undefined,
            entries: l.tasks.map((t) => ({ content: t.text, status: t.done ? 'completed' : 'pending' })),
            infer: true,
          };
          return true;
        }
        return false;
      }
    }
    return false;
  }

  private syncTasks() {
    this.plan = this.tasks.size ? { entries: [...this.tasks.values()].map((x) => ({ ...x })) } : null;
    return true;
  }
}

/** The session's current task list (for the bar above the input box) */
export function latestPlan(events: HubEvent[]): PlanState | null {
  const t = new PlanTracker();
  for (const e of events) t.feed(e);
  return t.plan;
}

/** Status for display: with infer, while the turn runs, the first unfinished item counts as in progress */
export function viewEntries(p: PlanState, busy: boolean): PlanEntry[] {
  if (!p.infer || !busy) return p.entries;
  const i = p.entries.findIndex((x) => x.status !== 'completed');
  return i < 0 ? p.entries : p.entries.map((x, j) => (j === i ? { ...x, status: 'in_progress' } : x));
}

/** The task in progress (or the next one) */
export function currentEntry(entries: PlanEntry[]): { entry: PlanEntry; active: boolean } | null {
  const a = entries.find((x) => x.status === 'in_progress');
  if (a) return { entry: a, active: true };
  const n = entries.find((x) => x.status !== 'completed');
  return n ? { entry: n, active: false } : null;
}
