// 任务清单：把各家工具的任务列表统一成 { title, entries }。
// - Claude TodoWrite、Codex turn/plan/updated、ACP plan：yi-agentd 已转成 plan 事件，直接用
// - Kiro 的 todo_list 工具：每次调用的结果是完整列表（只有完成 / 未完成），全部完成后会清空
// - 新版 Claude 的 TaskCreate / TaskUpdate：创建时结果给编号（"Task #N created successfully: …"），之后按编号改状态
import type { HubEvent, PlanEntry } from './types';

export interface PlanState {
  title?: string;
  entries: PlanEntry[];
  /** 工具本身没有「进行中」（Kiro）：回合运行时把第一个没完成的当作进行中 */
  infer?: boolean;
}

/** 这些工具调用由任务清单代替显示（出错的照常显示） */
export const PLAN_TOOLS = new Set(['TodoWrite', 'todo_list', 'TaskCreate', 'TaskUpdate']);

type Rec = Record<string, unknown>;
const str = (v: unknown) => (typeof v === 'string' ? v : v == null ? '' : String(v));

/** Kiro todo_list 的结果：{"items":[{"Json":{description, tasks:[{id, task_description, completed}]}}]}，可能多包一层字符串 */
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
  /** Claude TaskCreate 建的任务，按编号，保持创建顺序 */
  private tasks = new Map<string, PlanEntry>();

  /** 处理一条事件，任务清单有变化时返回 true */
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
        // 改状态不用等结果：编号不存在时结果是 "Task #N not found"，这里也找不到，自然忽略
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
            // 全部完成后 Kiro 会清空列表：沿用上一份，全部打勾
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

/** 会话当前的任务清单（输入框上方那条用） */
export function latestPlan(events: HubEvent[]): PlanState | null {
  const t = new PlanTracker();
  for (const e of events) t.feed(e);
  return t.plan;
}

/** 显示用的状态：infer 时回合运行中把第一个没完成的当作进行中 */
export function viewEntries(p: PlanState, busy: boolean): PlanEntry[] {
  if (!p.infer || !busy) return p.entries;
  const i = p.entries.findIndex((x) => x.status !== 'completed');
  return i < 0 ? p.entries : p.entries.map((x, j) => (j === i ? { ...x, status: 'in_progress' } : x));
}

/** 正在做的（或下一项）任务 */
export function currentEntry(entries: PlanEntry[]): { entry: PlanEntry; active: boolean } | null {
  const a = entries.find((x) => x.status === 'in_progress');
  if (a) return { entry: a, active: true };
  const n = entries.find((x) => x.status !== 'completed');
  return n ? { entry: n, active: false } : null;
}
