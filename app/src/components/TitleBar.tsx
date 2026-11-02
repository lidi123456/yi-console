import { useEffect, useState } from 'react';
import { tr } from '../i18n';
import { goBack, goForward, toggleSidebar, useStore } from '../store';
import { isTauri } from '../transport';
import { Icon } from './Icon';
import { MOD } from './Palette';

const ua = navigator.userAgent;
const isMobileOS = /Android|iPhone|iPad/.test(ua);
const isMac = /Mac/.test(navigator.platform);
/** 桌面 App 去掉了系统标题栏（macOS 保留红绿灯），窗口按钮自己画 */
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
      <button title={tr('最小化', 'Minimize')} onClick={() => win().then((w) => w.minimize())}>
        <Icon name="win-min" size={14} />
      </button>
      <button title={max ? tr('还原', 'Restore') : tr('最大化', 'Maximize')} onClick={() => win().then((w) => w.toggleMaximize())}>
        <Icon name={max ? 'win-restore' : 'win-max'} size={12} />
      </button>
      <button className="win-close" title={tr('关闭', 'Close')} onClick={() => win().then((w) => w.close())}>
        <Icon name="x" size={14} />
      </button>
    </div>
  );
}

/** 顶部一行：收起侧边栏、后退、前进；空白处可以拖动窗口、双击最大化 */
export function TitleBar() {
  const canBack = useStore((s) => s.canBack);
  const canForward = useStore((s) => s.canForward);
  const hidden = useStore((s) => s.sidebarHidden);
  const drag = customFrame ? { 'data-tauri-drag-region': true } : {};
  return (
    <div className={`titlebar ${customFrame && isMac ? 'mac' : ''} ${customFrame ? 'framed' : ''}`} {...drag}>
      <div className="tb-nav">
        <button
          title={tr(`${hidden ? '展开' : '收起'}侧边栏（${MOD}+B）`, `${hidden ? 'Show' : 'Hide'} Sidebar (${MOD}+B)`)}
          onClick={toggleSidebar}
        >
          <Icon name="sidebar" size={16} />
        </button>
        <button title={tr('后退（Alt+←）', 'Back (Alt+←)')} disabled={!canBack} onClick={goBack}>
          <Icon name="back" size={16} />
        </button>
        <button title={tr('前进（Alt+→）', 'Forward (Alt+→)')} disabled={!canForward} onClick={goForward}>
          <Icon name="forward" size={16} />
        </button>
      </div>
      <div className="grow" {...drag} />
      {customFrame && !isMac && <WindowButtons />}
    </div>
  );
}
