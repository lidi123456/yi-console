import { getT } from '../i18n';
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

const t = getT('appUpdate');

// App self-update UI: a corner notice when a new version is found (or while it downloads / installs),
// and the "Updates" part of the About card in system settings. Logic is in app/src/update.ts.

function percent(u: UpdateState): number | null {
  return u.total ? Math.min(100, Math.round(((u.got ?? 0) / u.total) * 100)) : null;
}

function progressText(u: UpdateState): string {
  const p = percent(u);
  return p != null
    ? t('downloadingUpdate', { p })
    : t('downloadingUpdate2', { v: fmtSize(u.got ?? 0) });
}

function installingText(): string {
  return updatePlatform === 'android'
    ? t('confirmTheInstallInThe')
    : t('installingTheAppWillRestart');
}

const PERMISSION_TEXT = () =>
  t('allowYiConsoleToInstall');

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
          <span>{t('yiConsoleIsAvailable', { version: u.version ?? '' })}</span>
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
            {u.installable === false ? t('download') : t('updateNow')}
          </button>
          <button className="link-btn small muted" onClick={dismissUpdate}>
            {t('later')}
          </button>
        </>
      )}
      {u.phase === 'installing' && updatePlatform === 'android' && (
        <button className="link-btn small" onClick={() => installUpdate()}>
          {t('installAgain')}
        </button>
      )}
      {(u.phase === 'installing' || u.phase === 'permission') && updatePlatform === 'android' && (
        <button className="link-btn small muted" onClick={() => setClosedIn(u.phase)}>
          {t('close')}
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
  if (u.phase === 'checking') status = t('checkingForUpdates');
  else if (u.phase === 'latest') status = t('youAreUpToDate', { current: u.current });
  else if (u.phase === 'available') status = t('newVersionAvailable', { version: u.version ?? '' });
  else if (u.phase === 'downloading') status = progressText(u);
  else if (u.phase === 'installing') status = installingText();
  else if (u.phase === 'permission') status = PERMISSION_TEXT();
  else if (u.current) status = t('currentVersion', { current: u.current });
  return (
    <div className="app-update-section">
      <div className="pref-row">
        <div className="grow min0">
          <div>{t('checkForUpdatesAutomatically')}</div>
          <div className="muted small">
            {t('atStartupAndEvery6')}
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
              ? t('installAgain')
              : u.installable === false
                ? t('download')
                : t('updateNow')}
          </button>
        ) : (
          <button className="btn small" disabled={busy} onClick={() => checkUpdate()}>
            {t('checkForUpdates')}
          </button>
        )}
      </div>
      {u.phase === 'downloading' && <Bar u={u} />}
      {u.notes && ['available', 'downloading', 'installing'].includes(u.phase) && <div className="app-update-notes small">{u.notes}</div>}
    </div>
  );
}
