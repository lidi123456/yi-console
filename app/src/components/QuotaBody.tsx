import { getT, isEn, trErr } from '../i18n';
import type { Quota } from '../types';

const tx = getT('statsView');

export const quotaErrText = (e: unknown) => {
  const m = String((e as Error)?.message ?? e);
  return m.includes('未知方法') ? tx('thisHostSDaemonIs') : trErr(m);
};

export const fmtQuotaReset = (v: number | string | null | undefined) => {
  if (v == null) return '';
  const d = new Date(v);
  if (isNaN(d.getTime())) return '';
  const ms = d.getTime() - Date.now();
  if (ms <= 0) return tx('resetsSoon');
  const h = Math.floor(ms / 3600000);
  const mins = Math.ceil((ms % 3600000) / 60000);
  if (h < 24) return isEn() ? `resets in ${h > 0 ? `${h}h ` : ''}${mins}m` : `${h > 0 ? `${h} 小时` : ''}${mins} 分钟后重置`;
  const clock = `${d.getMonth() + 1}/${d.getDate()} ${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
  return isEn() ? `resets ${clock}` : `${clock} 重置`;
};

/** Quota windows for one tool or account profile: undefined while loading, a string is an error message */
export function QuotaBody({ q }: { q: Quota | string | undefined }) {
  return (
    <>
      {q === undefined && <div className="muted small">{tx('loading')}</div>}
      {typeof q === 'string' && <div className="muted small">{tx('failed')}{q}</div>}
      {typeof q === 'object' && (
        <>
          {q.windows.map((w) => (
            <div key={w.label} className="quota-win">
              <div className="row small">
                <span>{w.label}</span>
                <span className="grow" />
                <span className="muted">
                  {w.detail ?? (w.usedPercent != null ? tx('used', { v: Math.round(w.usedPercent) }) : '')}
                  {fmtQuotaReset(w.resetsAt) && ` · ${fmtQuotaReset(w.resetsAt)}`}
                </span>
              </div>
              {w.usedPercent != null && (
                <div className="bar">
                  <i
                    style={{ width: `${Math.min(100, Math.max(0, w.usedPercent))}%` }}
                    className={w.usedPercent >= 90 ? 'hot' : ''}
                  />
                </div>
              )}
            </div>
          ))}
          {q.note && <div className="muted small">{trErr(q.note)}</div>}
        </>
      )}
    </>
  );
}
