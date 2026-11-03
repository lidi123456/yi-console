import { tr } from '../i18n';
import {
  autoUpdateEnabled,
  checkUpdate,
  dismissUpdate,
  installUpdate,
  isDismissed,
  setAutoUpdate,
  updatePlatform,
  updateSupported,
  useUpdate,
  type UpdateState,
} from '../update';
import { fmtSize } from '../util';
import { useState } from 'react';

// App self-update UI: a corner notice when a new version is found (or while it downloads / installs),
// and the "Updates" part of the About card in system settings. Logic is in app/src/update.ts.

function percent(u: UpdateState): number | null {
  return u.total ? Math.min(100, Math.round(((u.got ?? 0) / u.total) * 100)) : null;
}

function progressText(u: UpdateState): string {
  const p = percent(u);
  return p != null
    ? tr(`正在下载更新 ${p}%`, `Downloading update ${p}%`)
    : tr(`正在下载更新 ${fmtSize(u.got ?? 0)}`, `Downloading update ${fmtSize(u.got ?? 0)}`);
}

function installingText(): string {
  return updatePlatform === 'android'
    ? tr('请在系统安装界面确认安装', 'Confirm the install in the system installer')
    : tr('正在安装，完成后会自动重启', 'Installing, the app will restart when done');
}

const PERMISSION_TEXT = () =>
  tr('请允许 Yi Console 安装应用，返回后会继续安装', 'Allow Yi Console to install apps; installing continues when you come back');

function Bar({ u }: { u: UpdateState }) {
  const p = percent(u);
  return (
    <div className="upgrade-bar">
      <div className={`upgrade-fill ${p == null ? 'indeterminate' : ''}`} style={p == null ? undefined : { width: `${p}%` }} />
    </div>
  );
}

/** Corner notice: new version found (unless "Later" was clicked for it), download progress, install state, install errors */
export function AppUpdateNotice() {
  const u = useUpdate();
  // "Close" hides the notice for the phase it was closed in (Android: waiting for the installer / the permission)
  const [closedIn, setClosedIn] = useState<string | null>(null);
  if (!updateSupported || closedIn === u.phase) return null;
  // Failed checks count as "up to date" and never show an error; install errors show in the settings card
  const show =
    (u.phase === 'available' && !isDismissed(u.version)) || ['downloading', 'installing', 'permission'].includes(u.phase);
  if (!show) return null;
  return (
    <div className="upgrade-toast app-update" role="status">
      <div className="grow min0">
        {u.phase === 'available' && (
          <span>{tr(`Yi Console ${u.version} 可以更新了`, `Yi Console ${u.version} is available`)}</span>
        )}
        {u.phase === 'downloading' && (
          <>
            <span>{progressText(u)}</span>
            <Bar u={u} />
          </>
        )}
        {u.phase === 'installing' && <span>{installingText()}</span>}
        {u.phase === 'permission' && <span>{PERMISSION_TEXT()}</span>}
      </div>
      {u.phase === 'available' && (
        <>
          <button className="link-btn small" onClick={() => installUpdate()}>
            {u.installable === false ? tr('去下载', 'Download') : tr('立即更新', 'Update Now')}
          </button>
          <button className="link-btn small muted" onClick={dismissUpdate}>
            {tr('稍后', 'Later')}
          </button>
        </>
      )}
      {u.phase === 'installing' && updatePlatform === 'android' && (
        <button className="link-btn small" onClick={() => installUpdate()}>
          {tr('重新安装', 'Install Again')}
        </button>
      )}
      {(u.phase === 'installing' || u.phase === 'permission') && updatePlatform === 'android' && (
        <button className="link-btn small muted" onClick={() => setClosedIn(u.phase)}>
          {tr('关闭', 'Close')}
        </button>
      )}
    </div>
  );
}

/** "Updates" section of the About card */
export function UpdateSection({ Switch }: { Switch: (p: { checked: boolean; onChange: (v: boolean) => void }) => React.ReactElement }) {
  const u = useUpdate();
  const [auto, setAuto] = useState(autoUpdateEnabled);
  if (!updateSupported) return null;
  const busy = ['checking', 'downloading', 'installing'].includes(u.phase);
  let status = '';
  if (u.phase === 'checking') status = tr('正在检查更新…', 'Checking for updates…');
  else if (u.phase === 'latest') status = tr(`已经是最新版本（${u.current}）`, `You are up to date (${u.current})`);
  else if (u.phase === 'available') status = tr(`发现新版本 ${u.version}`, `New version ${u.version} available`);
  else if (u.phase === 'downloading') status = progressText(u);
  else if (u.phase === 'installing') status = installingText();
  else if (u.phase === 'permission') status = PERMISSION_TEXT();
  else if (u.current) status = tr(`当前版本 ${u.current}`, `Current version ${u.current}`);
  return (
    <div className="app-update-section">
      <div className="pref-row">
        <div className="grow min0">
          <div>{tr('自动检查更新', 'Check for updates automatically')}</div>
          <div className="muted small">
            {tr('启动后和之后每 6 小时检查一次，有新版本时在右下角提示。', 'At startup and every 6 hours; a new version is announced in the corner.')}
          </div>
        </div>
        <Switch
          checked={auto}
          onChange={(v) => {
            setAuto(v);
            setAutoUpdate(v);
          }}
        />
      </div>
      <div className="row app-update-row">
        <div className="grow min0 small">
          {status && <div className={u.phase === 'available' ? '' : 'muted'}>{status}</div>}
          {u.phase === 'error' && <div className="error-text">{u.error}</div>}
        </div>
        {u.phase === 'available' || (u.phase === 'installing' && updatePlatform === 'android') ? (
          <button className="btn primary small" onClick={() => installUpdate()}>
            {u.phase !== 'available'
              ? tr('重新安装', 'Install Again')
              : u.installable === false
                ? tr('去下载', 'Download')
                : tr('立即更新', 'Update Now')}
          </button>
        ) : (
          <button className="btn small" disabled={busy} onClick={() => checkUpdate()}>
            {tr('检查更新', 'Check for Updates')}
          </button>
        )}
      </div>
      {u.phase === 'downloading' && <Bar u={u} />}
      {u.notes && ['available', 'downloading', 'installing'].includes(u.phase) && <div className="app-update-notes small">{u.notes}</div>}
    </div>
  );
}
