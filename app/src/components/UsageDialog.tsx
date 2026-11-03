import { useEffect, useState } from 'react';
import { useBack } from '../back';
import { getT } from '../i18n';
import { rpc } from '../transport';
import { TOOL_LABEL, type Quota, type SessionInfo } from '../types';
import { fmtTokens } from '../util';
import { Icon, ToolBadge } from './Icon';
import { errText, QuotaBody } from './StatsView';

const t = getT('usageDialog');

/** "Usage" on the session page: how much this session used, and how much quota is left on this tool account */
export function UsageDialog({ hostId, info, onClose }: { hostId: string; info: SessionInfo; onClose: () => void }) {
  const [quota, setQuota] = useState<Quota | string | undefined>(undefined);
  useBack(true, onClose);
  const load = (refresh: boolean) => {
    setQuota(undefined);
    rpc<Quota>(hostId, 'tools.quota', { tool: info.tool, refresh })
      .then(setQuota)
      .catch((e) => setQuota(errText(e)));
  };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(() => load(false), [hostId, info.tool]);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  const u = info.usage;
  const total = u.inputTokens + u.outputTokens + u.cacheReadTokens + u.cacheWriteTokens;
  const rows: [string, string][] = [
    [t('input'), fmtTokens(u.inputTokens)],
    [t('output'), fmtTokens(u.outputTokens)],
    [t('cacheRead'), fmtTokens(u.cacheReadTokens)],
    [t('cacheWrite'), fmtTokens(u.cacheWriteTokens)],
    [t('total'), fmtTokens(total)],
  ];
  if (u.costUsd) rows.push([t('cost'), `$${u.costUsd.toFixed(u.costUsd < 1 ? 3 : 2)}`]);
  if (u.credits) rows.push(['Credits', u.credits.toFixed(2)]);
  if (info.contextPercent != null) rows.push([t('context'), `${Math.round(info.contextPercent)}%`]);
  if (info.model) rows.push([t('model'), info.model]);
  const noTokens = total === 0 && (info.tool === 'cursor' || info.mode === 'terminal');

  return (
    <div className="modal-bg" onClick={onClose}>
      <div className="modal usage-dlg" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <h3>{t('usage')}</h3>
          <button className="icon-btn" onClick={onClose}>
            <Icon name="x" />
          </button>
        </div>
        <div className="card-title">{t('thisSession')}</div>
        {noTokens ? (
          <div className="muted small">
            {info.mode === 'terminal'
              ? t('terminalModeDoesNotReport')
              : t('cursorDoesNotReportPer')}
          </div>
        ) : (
          <div className="usage-grid">
            {rows.map(([k, v]) => (
              <div key={k} className="usage-cell">
                <span className="muted small">{k}</span>
                <b className="truncate" title={v}>
                  {v}
                </b>
              </div>
            ))}
          </div>
        )}
        {info.tool !== 'shell' && (
          <>
            <div className="card-title row usage-quota-head">
              <ToolBadge tool={info.tool} size={16} />
              <span>{TOOL_LABEL[info.tool]} {t('quota')}</span>
              {typeof quota === 'object' && (quota.account?.plan || quota.account?.email) && (
                <span className="muted small">{[quota.account.plan, quota.account.email].filter(Boolean).join(' · ')}</span>
              )}
              <span className="grow" />
              <button className="btn small" disabled={quota === undefined} onClick={() => load(true)}>
                {t('refresh')}
              </button>
            </div>
            <div className="quota">
              <QuotaBody q={quota} />
              {typeof quota === 'object' && quota.windows.length === 0 && !quota.note && (
                <div className="muted small">{t('thisToolDidNotReport')}</div>
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
