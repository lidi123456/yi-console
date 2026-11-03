import { getT } from '../i18n';
import { hubCall } from '../transport';
import { errorToast, setState, useStore } from '../store';

const t = getT('upgrade');

/** yi-agentd upgrade: block the whole screen while uploading / restarting (the host is unreachable then); while waiting for a turn to end, only a corner notice */
export function UpgradeOverlay() {
  const upgrades = useStore((s) => s.upgrades);
  const hosts = useStore((s) => s.hosts);
  const name = (id: string) => hosts.find((h) => h.config.id === id)?.config.name ?? id;
  const entries = Object.entries(upgrades);
  const blocking = entries.filter(([, u]) => u.phase !== 'waiting');
  const waiting = entries.filter(([, u]) => u.phase === 'waiting');
  const dismiss = (id: string) =>
    setState((s) => {
      const upgrades = { ...s.upgrades };
      delete upgrades[id];
      return { upgrades };
    });
  const now = (id: string) => hubCall('hosts.upgrade', { id }).catch(errorToast);

  return (
    <>
      {blocking.length > 0 && (
        <div className="modal-bg upgrade-bg">
          <div className="modal upgrade-modal">
            <div className="modal-head">
              <h3>{t('upgradingYiAgentd')}</h3>
            </div>
            {blocking.map(([id, u]) => (
              <div key={id} className="upgrade-item">
                <div className="upgrade-host">{name(id)}</div>
                <div className={u.phase === 'error' ? 'error-text small' : 'muted small'}>{u.message}</div>
                {u.phase !== 'error' && (
                  <div className="upgrade-bar">
                    <div
                      className={`upgrade-fill ${u.progress == null ? 'indeterminate' : ''}`}
                      style={u.progress == null ? undefined : { width: `${Math.round(u.progress * 100)}%` }}
                    />
                  </div>
                )}
                {u.phase === 'error' && (
                  <div className="upgrade-actions">
                    <button className="btn small" onClick={() => dismiss(id)}>
                      {t('close')}
                    </button>
                    <button className="btn small primary" onClick={() => now(id)}>
                      {t('retry')}
                    </button>
                  </div>
                )}
              </div>
            ))}
            <div className="muted small upgrade-note">
              {t('temporarilyUnavailableDuringUpgradeIt')}
            </div>
          </div>
        </div>
      )}
      {waiting.map(([id, u]) => (
        <div key={id} className="upgrade-toast">
          <span>
            {name(id)}{t('text')}{u.message}
          </span>
          <button className="link-btn small" onClick={() => now(id)}>
            {t('upgradeNow')}
          </button>
        </div>
      ))}
    </>
  );
}
