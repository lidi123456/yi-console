import { useEffect, useState } from 'react';
import { getT, isEn } from '../i18n';
import { fmtAt, fmtLeft, limitName } from '../limit';
import { errorToast, toast } from '../store';
import { rpc } from '../transport';
import { supportsAccountsTool, type SessionInfo, type ToolAccountsSummary } from '../types';
import { openAccountManager } from './AccountManagerModal';
import { Icon } from './Icon';
import { openMenu } from './Overlay';
import { handoffSession } from './SessionActions';

const t = getT('limitDock');

/** Above the input box while an automatic continue is pending: countdown, "Continue now" and "Cancel" */
export function LimitDock({ hostId, info, connected }: { hostId: string; info: SessionInfo; connected: boolean }) {
  const ar = info.autoResume;
  const [now, setNow] = useState(Date.now());
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (!ar) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [ar?.at]);
  if (!ar) return null;
  const act = async (action: 'cancel' | 'now') => {
    setBusy(true);
    try {
      await rpc(hostId, 'session.autoResume', { sessionId: info.id, action });
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };
  const name = limitName(ar.limit);
  const left = ar.at - now;
  return (
    <div className="limit-dock" role="status">
      <Icon name="clock" size={14} />
      <span className="limit-dock-text">
        <span>{isEn() ? `Usage limit reached${name ? ` (${name})` : ''}` : `额度已用完${name ? `（${name}）` : ''}`}</span>
        <span className="limit-dock-when">
          {left > 0 ? (
            <>
              <span className="lbl-long">{t('continuesAtIn', { v: fmtAt(ar.at), v2: fmtLeft(left) })}</span>
              <span className="lbl-short">{t('autoAt', { v: fmtAt(ar.at), v2: fmtLeft(left) })}</span>
            </>
          ) : (
            t('continuing')
          )}
        </span>
      </span>
      {/* Buttons move to their own row when the chat area is narrow (container queries in styles.css) */}
      <div className="limit-dock-actions">
        <button className="btn small" disabled={!connected || busy} onClick={() => act('now')} title={t('sendContinueRightAway')}>
          <span className="lbl-long">{t('continueNow')}</span>
          <span className="lbl-short">{t('continue')}</span>
        </button>
        {supportsAccountsTool(info.tool) && (
          <button
            className="btn small"
            disabled={!connected || busy}
            onClick={async (e) => {
              const coords = { clientX: e.clientX, clientY: e.clientY };
              try {
                const res = await rpc<{ tools: ToolAccountsSummary[] }>(hostId, 'tools.accounts.list', {
                  tool: info.tool,
                  checkNative: false,
                });
                const accounts = res.tools?.[0]?.accounts ?? [];
                openMenu(coords, [
                  ...accounts.map((acc) => ({
                    label: acc.label,
                    hint: [acc.authKind === 'apiKey' ? 'API' : 'OAuth', acc.identity.email].filter(Boolean).join(' · '),
                    active: acc.id === info.accountId,
                    run: async () => {
                      setBusy(true);
                      try {
                        const switching = acc.id !== info.accountId;
                        const updated = await rpc<SessionInfo>(hostId, 'session.switchAccount', {
                          sessionId: info.id,
                          accountId: acc.id,
                          ...(switching ? { autoContinue: true } : {}),
                        });
                        toast(
                          switching && (updated.status === 'running' || updated.status === 'starting')
                            ? t('switchedBackupAccountContinuing', { label: acc.label })
                            : t('switchedBackupAccount', { label: acc.label }),
                          'info',
                        );
                      } catch (err) {
                        errorToast(err);
                      } finally {
                        setBusy(false);
                      }
                    },
                  })),
                  ...(accounts.length ? (['-'] as const) : []),
                  {
                    label: t('manageAccounts'),
                    icon: 'key',
                    run: () => openAccountManager({ hostId, tool: info.tool, sessionId: info.id, accountId: info.accountId }),
                  },
                ]);
              } catch (err) {
                errorToast(err);
              }
            }}
            title={t('switchBackupAccountTitle')}
          >
            <span className="lbl-long">{t('switchBackupAccount')}</span>
            <span className="lbl-short">{t('switchAccountShort')}</span>
          </button>
        )}
        <button className="btn small" disabled={!connected || busy} onClick={() => handoffSession(hostId, info, { limit: true })} title={t('handOffToAnotherToolTitle')}>
          <span className="lbl-long">{t('handOffToAnotherTool')}</span>
          <span className="lbl-short">{t('switchTool')}</span>
        </button>
        <button className="btn small" disabled={!connected || busy} onClick={() => act('cancel')}>
          {t('cancel')}
        </button>
      </div>
    </div>
  );
}
