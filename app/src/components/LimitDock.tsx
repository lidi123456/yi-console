import { useEffect, useState } from 'react';
import { isEn, tr } from '../i18n';
import { fmtAt, fmtLeft, limitName } from '../limit';
import { errorToast } from '../store';
import { rpc } from '../transport';
import type { SessionInfo } from '../types';
import { Icon } from './Icon';

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
              <span className="hide-mobile">{tr(`${fmtAt(ar.at)} 自动继续 · 还有 ${fmtLeft(left)}`, `continues at ${fmtAt(ar.at)} · in ${fmtLeft(left)}`)}</span>
              <span className="show-mobile">{tr(`${fmtAt(ar.at)} 自动继续 · 还有 ${fmtLeft(left)}`, `Auto at ${fmtAt(ar.at)} · ${fmtLeft(left)}`)}</span>
            </>
          ) : (
            tr('正在自动继续…', 'Continuing…')
          )}
        </span>
      </span>
      <button className="btn small" disabled={!connected || busy} onClick={() => act('now')} title={tr('现在就发送“继续”', 'Send "continue" right away')}>
        <span className="hide-mobile">{tr('立即继续', 'Continue now')}</span>
        <span className="show-mobile">{tr('立即继续', 'Continue')}</span>
      </button>
      <button className="btn small" disabled={!connected || busy} onClick={() => act('cancel')}>
        {tr('取消', 'Cancel')}
      </button>
    </div>
  );
}
