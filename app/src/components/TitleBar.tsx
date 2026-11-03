import { useEffect, useState } from 'react';
import { getT } from '../i18n';
import { goBack, goForward, toggleSidebar, useStore } from '../store';
import { isTauri } from '../transport';
import { Icon } from './Icon';
import { MOD } from './Palette';

const t = getT('titleBar');

const ua = navigator.userAgent;
const isMobileOS = /Android|iPhone|iPad/.test(ua);
const isMac = /Mac/.test(navigator.platform);
/** The desktop app drops the system title bar (macOS keeps the traffic lights) and draws its own window buttons */
export const customFrame = isTauri && !isMobileOS;

async function win() {
  return (await import('@tauri-apps/api/window')).getCurrentWindow();
}

function WindowButtons() {
  const [max, setMax] = useState(false);
  useEffect(() => {
    let off: (() => void) | undefined;
    win().then(async (w) => {
      setMax(await w.isMaximized());
      off = await w.onResized(async () => setMax(await w.isMaximized()));
    });
    return () => off?.();
  }, []);
  return (
    <div className="win-btns">
      <button title={t('minimize')} onClick={() => win().then((w) => w.minimize())}>
        <Icon name="win-min" size={14} />
      </button>
      <button title={max ? t('restore') : t('maximize')} onClick={() => win().then((w) => w.toggleMaximize())}>
        <Icon name={max ? 'win-restore' : 'win-max'} size={12} />
      </button>
      <button className="win-close" title={t('close')} onClick={() => win().then((w) => w.close())}>
        <Icon name="x" size={14} />
      </button>
    </div>
  );
}

/** Top row: collapse sidebar, back, forward; empty space drags the window, double-click maximizes */
export function TitleBar() {
  const canBack = useStore((s) => s.canBack);
  const canForward = useStore((s) => s.canForward);
  const hidden = useStore((s) => s.sidebarHidden);
  const drag = customFrame ? { 'data-tauri-drag-region': true } : {};
  return (
    <div className={`titlebar ${customFrame && isMac ? 'mac' : ''} ${customFrame ? 'framed' : ''}`} {...drag}>
      <div className="tb-nav">
        <button
          title={hidden ? t('showSidebar', { mod: MOD }) : t('hideSidebar', { mod: MOD })}
          onClick={toggleSidebar}
        >
          <Icon name="sidebar" size={16} />
        </button>
        <button title={t('backAlt')} disabled={!canBack} onClick={goBack}>
          <Icon name="back" size={16} />
        </button>
        <button title={t('forwardAlt')} disabled={!canForward} onClick={goForward}>
          <Icon name="forward" size={16} />
        </button>
      </div>
      <div className="grow" {...drag} />
      {customFrame && !isMac && <WindowButtons />}
    </div>
  );
}
