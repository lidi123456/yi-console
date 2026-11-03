import { useEffect, useState } from 'react';
import { getT, isEn } from '../i18n';
import { fmtAt, fmtLeft, limitName } from '../limit';
import { errorToast } from '../store';
import { rpc } from '../transport';
import type { SessionInfo } from '../types';
import { Icon } from './Icon';

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
        {/* On phones only the time is shown, so the bar stays one line */}
        <span className="hide-mobile">{isEn() ? `Usage limit reached${name ? ` (${name})` : ''}` : `额度已用完${name ? `（${name}）` : ''}`}</span>
        <span className="limit-dock-when">
          {left > 0 ? (
            <>
              <span className="hide-mobile">{t('continuesAtIn', { v: fmtAt(ar.at), v2: fmtLeft(left) })}</span>
              <span className="show-mobile">{t('autoAt', { v: fmtAt(ar.at), v2: fmtLeft(left) })}</span>
            </>
          ) : (
            t('continuing')
          )}
        </span>
      </span>
      <button className="btn small" disabled={!connected || busy} onClick={() => act('now')} title={t('sendContinueRightAway')}>
        <span className="hide-mobile">{t('continueNow')}</span>
        <span className="show-mobile">{t('continue')}</span>
      </button>
      <button className="btn small" disabled={!connected || busy} onClick={() => act('cancel')}>
        {t('cancel')}
      </button>
    </div>
  );
}
