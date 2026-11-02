import { tr } from '../i18n';
import { hubCall } from '../transport';
import { errorToast, setState, useStore } from '../store';

/** yi-agentd 升级：上传 / 重启期间整屏挡住（这段时间连不上主机）；等回合结束时只在角落提示 */
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
              <h3>{tr('正在升级 yi-agentd', 'Upgrading yi-agentd')}</h3>
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
                      {tr('关闭', 'Close')}
                    </button>
                    <button className="btn small primary" onClick={() => now(id)}>
                      {tr('重试', 'Retry')}
                    </button>
                  </div>
                )}
              </div>
            ))}
            <div className="muted small upgrade-note">
              {tr('升级期间暂时无法使用，完成后会自动重新连接，会话都会保留。', 'Temporarily unavailable during upgrade. It will reconnect automatically and all sessions will be preserved.')}
            </div>
          </div>
        </div>
      )}
      {waiting.map(([id, u]) => (
        <div key={id} className="upgrade-toast">
          <span>
            {name(id)}{tr('：', ': ')}{u.message}
          </span>
          <button className="link-btn small" onClick={() => now(id)}>
            {tr('立即升级', 'Upgrade Now')}
          </button>
        </div>
      ))}
    </>
  );
}
