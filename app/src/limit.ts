// Wording for usage limits and the automatic continue (`rate_limit` events, `SessionInfo.autoResume`)

import { isEn, tr } from './i18n';
import type { HubEvent } from './types';
import { clock } from './util';

export type LimitEvent = HubEvent & { kind: 'rate_limit' };

/** five_hour / seven_day / … as reported by the tool */
export function limitName(limit?: string | null): string {
  switch (limit) {
    case 'five_hour':
      return tr('5 小时额度', '5-hour limit');
    case 'seven_day':
      return tr('每周额度', 'weekly limit');
    case 'seven_day_opus':
      return tr('每周 Opus 额度', 'weekly Opus limit');
    case 'seven_day_sonnet':
      return tr('每周 Sonnet 额度', 'weekly Sonnet limit');
    case null:
    case undefined:
    case '':
      return '';
    default:
      return limit;
  }
}

/** A time that is usually within a day: "15:05", or "11/4 15:05" on another day */
export function fmtAt(ms: number): string {
  const t = new Date(ms);
  const now = new Date();
  const sameDay = t.toDateString() === now.toDateString();
  return sameDay ? clock(ms) : `${t.getMonth() + 1}/${t.getDate()} ${clock(ms)}`;
}

/** Time left: "1h 05m" / "12m" / "40s" (Chinese: 1 小时 5 分 / 12 分 / 40 秒) */
export function fmtLeft(ms: number): string {
  const s = Math.max(0, Math.ceil(ms / 1000));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (isEn()) return h ? `${h}h ${String(m).padStart(2, '0')}m` : m ? `${m}m` : `${s}s`;
  return h ? `${h} 小时 ${m} 分` : m ? `${m} 分` : `${s} 秒`;
}

/** First half of the timeline line: "Usage limit reached (5-hour limit, resets 15:04)" */
export function limitHead(hit: LimitEvent | undefined, resetsAt?: number): string {
  const name = limitName(hit?.limit);
  const at = resetsAt ?? hit?.resetsAt;
  const extra = [name, at ? tr(`${fmtAt(at)} 恢复`, `resets ${fmtAt(at)}`) : ''].filter(Boolean).join(isEn() ? ', ' : '，');
  return isEn() ? `Usage limit reached${extra ? ` (${extra})` : ''}` : `额度已用完${extra ? `（${extra}）` : ''}`;
}

/** Second half: what happens / happened with the automatic continue. `live`: hit without a decision yet (probing) */
export function limitOutcome(e: LimitEvent, live: boolean): string {
  switch (e.phase) {
    case 'hit':
      return live ? tr('正在查询恢复时间…', 'Looking up when it resets…') : '';
    case 'scheduled':
      return e.resumeAt ? tr(`将在 ${fmtAt(e.resumeAt)} 自动继续`, `will continue automatically at ${fmtAt(e.resumeAt)}`) : '';
    case 'skipped':
      switch (e.reason) {
        case 'off':
          return tr('自动继续已在设置中关闭', 'automatic continue is turned off in settings');
        case 'unknown':
          return tr('查不到恢复时间，不会自动继续', "couldn't find when it resets, so it won't continue automatically");
        case 'far':
          return tr('恢复时间超过一天，不会自动继续', "resets more than a day from now, so it won't continue automatically");
        case 'repeated':
          return tr('连续多次碰到额度上限，已停止自动继续', 'hit the limit again several times in a row, automatic continue stopped');
        default:
          return tr('不会自动继续', "won't continue automatically");
      }
    case 'resumed':
      return e.reason === 'now' ? tr('已手动立即继续', 'continued now by hand') : tr('额度已恢复，已自动发送继续', 'limit reset, sent "continue" automatically');
    case 'cancelled':
      switch (e.reason) {
        case 'user':
          return tr('已取消自动继续', 'automatic continue cancelled');
        case 'manual':
          return tr('你发送了消息，不再自动继续', 'you sent a message, automatic continue dropped');
        case 'mode':
          return tr('切换到了终端界面，不再自动继续', 'switched to the terminal UI, automatic continue dropped');
        case 'off':
          return tr('自动继续已在设置中关闭', 'automatic continue is turned off in settings');
        case 'busy':
          return tr('会话正在运行，跳过了自动继续', 'the session was busy, automatic continue skipped');
        default:
          return tr('自动继续已取消', 'automatic continue cancelled');
      }
  }
}
