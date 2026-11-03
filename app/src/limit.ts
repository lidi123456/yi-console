// Wording for usage limits and the automatic continue (`rate_limit` events, `SessionInfo.autoResume`)

import { getT, isEn } from './i18n';
import type { HubEvent } from './types';
import { clock } from './util';

const t = getT('limit');

export type LimitEvent = HubEvent & { kind: 'rate_limit' };

/** five_hour / seven_day / … as reported by the tool */
export function limitName(limit?: string | null): string {
  switch (limit) {
    case 'five_hour':
      return t('n5HourLimit');
    case 'seven_day':
      return t('weeklyLimit');
    case 'seven_day_opus':
      return t('weeklyOpusLimit');
    case 'seven_day_sonnet':
      return t('weeklySonnetLimit');
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
  const extra = [name, at ? t('resets', { v: fmtAt(at) }) : ''].filter(Boolean).join(isEn() ? ', ' : '，');
  return isEn() ? `Usage limit reached${extra ? ` (${extra})` : ''}` : `额度已用完${extra ? `（${extra}）` : ''}`;
}

/** Second half: what happens / happened with the automatic continue. `live`: hit without a decision yet (probing) */
export function limitOutcome(e: LimitEvent, live: boolean): string {
  switch (e.phase) {
    case 'hit':
      return live ? t('lookingUpWhenItResets') : '';
    case 'scheduled':
      return e.resumeAt ? t('willContinueAutomaticallyAt', { v: fmtAt(e.resumeAt) }) : '';
    case 'skipped':
      switch (e.reason) {
        case 'off':
          return t('automaticContinueIsTurnedOff');
        case 'unknown':
          return t('couldnTFindWhenIt');
        case 'far':
          return t('resetsMoreThanADay');
        case 'repeated':
          return t('hitTheLimitAgainSeveral');
        default:
          return t('wonTContinueAutomatically');
      }
    case 'resumed':
      return e.reason === 'now' ? t('continuedNowByHand') : t('limitResetSentContinueAutomatically');
    case 'cancelled':
      switch (e.reason) {
        case 'user':
          return t('automaticContinueCancelled');
        case 'manual':
          return t('youSentAMessageAutomatic');
        case 'mode':
          return t('switchedToTheTerminalUi');
        case 'off':
          return t('automaticContinueIsTurnedOff');
        case 'busy':
          return t('theSessionWasBusyAutomatic');
        default:
          return t('automaticContinueCancelled2');
      }
  }
}
