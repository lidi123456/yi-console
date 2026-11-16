import { useCallback, useEffect, useState } from 'react';
import { useBack } from '../back';
import { getT } from '../i18n';
import { errorToast, onAccountsUpdated, toast } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, supportsAccountsTool, type Quota, type SessionInfo, type ToolQuotasGroup, type ToolsQuotasResponse } from '../types';
import { fmtTokens } from '../util';
import { openAccountManager } from './AccountManagerModal';
import { Icon, ToolBadge } from './Icon';
import { errText, QuotaBody } from './StatsView';

const t = getT('usageDialog');

/** "Usage" on the session page: how much this session used, and how much quota is left on this tool account */
export function UsageDialog({ hostId, info, onClose }: { hostId: string; info: SessionInfo; onClose: () => void }) {
  const [quota, setQuota] = useState<Quota | string | undefined>(undefined);
  const [allGroup, setAllGroup] = useState<ToolQuotasGroup | null>(null);
  const [switchingId, setSwitchingId] = useState<string | null>(null);
  useBack(true, onClose);
  const load = useCallback(
    (refresh: boolean, refreshInactive = false) => {
      setQuota(undefined);
      rpc<Quota>(hostId, 'tools.quota', { tool: info.tool, accountId: info.accountId ?? undefined, refresh })
        .then(setQuota)
        .catch((e) => setQuota(errText(e)));
      if (supportsAccountsTool(info.tool)) {
        rpc<ToolsQuotasResponse>(hostId, 'tools.quotas', { tool: info.tool, refresh, refreshInactive })
          .then((r) => setAllGroup(r.tools?.[0] ?? null))
          .catch(() => {});
      }
    },
    [hostId, info.tool, info.accountId],
  );
  useEffect(() => load(false, false), [load]);
  useEffect(() => {
    const off = onAccountsUpdated((h, updatedTool) => {
      if (h === hostId && (!updatedTool || updatedTool === info.tool)) load(false, false);
    });
    return off;
  }, [hostId, info.tool, load]);
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
            <div className="card-title row usage-quota-head" style={{ gap: 6, flexWrap: 'wrap', alignItems: 'center' }}>
              <ToolBadge tool={info.tool} size={16} />
              <span style={{ whiteSpace: 'nowrap' }}>{TOOL_LABEL[info.tool]} {t('quota')}</span>
              {info.accountLabel && <span className="acc-badge acc-badge-accent">{info.accountLabel}</span>}
              {typeof quota === 'object' && (quota.account?.plan || quota.account?.email) && (
                <span className="muted small" style={{ wordBreak: 'break-all' }}>{[quota.account.plan, quota.account.email].filter(Boolean).join(' · ')}</span>
              )}
              <span className="grow" />
              {supportsAccountsTool(info.tool) && (
                <button
                  className="btn small"
                  onClick={() =>
                    openAccountManager({ hostId, tool: info.tool, sessionId: info.id, accountId: info.accountId })
                  }
                >
                  <Icon name="key" size={14} /> {t('manageAccounts')}
                </button>
              )}
              <button className="btn small" disabled={quota === undefined} onClick={() => load(true, false)}>
                {t('refresh')}
              </button>
            </div>
            <div className="quota">
              <QuotaBody q={quota} />
              {typeof quota === 'object' && quota.windows.length === 0 && !quota.note && (
                <div className="muted small">{t('thisToolDidNotReport')}</div>
              )}
            </div>
            {allGroup && allGroup.accounts.length > 0 && (
              <div className="usage-all-accounts">
                <div className="row" style={{ marginTop: 12, marginBottom: 6, gap: 6 }}>
                  <span className="card-title" style={{ margin: 0 }}>{t('allAccountsQuota')}</span>
                  <span className="grow" />
                  {allGroup.oauthIsolation === 'hostActive' && allGroup.accounts.length > 1 && (
                    <button className="btn small" disabled={quota === undefined} onClick={() => load(true, true)}>
                      {t('probeAll')}
                    </button>
                  )}
                </div>
                <div className="quota-accounts">
                  {allGroup.accounts.map((acc) => {
                    const isSession = acc.accountId === info.accountId;
                    return (
                      <div key={acc.accountId} className={`quota-acc ${isSession ? 'active' : ''}`}>
                        <div className="row small" style={{ gap: 6, flexWrap: 'wrap' }}>
                          <strong>{acc.label}</strong>
                          <span className="acc-badge">{acc.authKind === 'apiKey' ? 'API' : 'OAuth'}</span>
                          {isSession && <span className="acc-badge acc-badge-ok">{t('currentSessionAccount')}</span>}
                          <span className="grow" />
                          {!isSession && (
                            <button
                              className="btn small"
                              disabled={switchingId === acc.accountId}
                              onClick={async () => {
                                setSwitchingId(acc.accountId);
                                try {
                                  await rpc(hostId, 'session.switchAccount', {
                                    sessionId: info.id,
                                    accountId: acc.accountId,
                                  });
                                  toast(t('switchedAccount', { label: acc.label }), 'info');
                                } catch (e) {
                                  errorToast(e);
                                } finally {
                                  setSwitchingId(null);
                                }
                              }}
                            >
                              {t('switchToThisAccount')}
                            </button>
                          )}
                        </div>
                        <QuotaBody q={acc.error ? errText(acc.error) : acc.quota ?? undefined} />
                      </div>
                    );
                  })}
                </div>
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
}
