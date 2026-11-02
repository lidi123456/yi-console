// 手机的返回键 / 返回手势：Android 的 MainActivity 拦下后调 window.__yiConsoleBack()，
// 返回 true 表示这里处理了，false 时 App 退出。
// 顺序：先关最上面打开的东西（弹窗、菜单、图片查看、文件面板等，后打开的先关），再关命令面板；
// 开着左侧导航时只收起它；最后回到上一个页面。
import { useEffect, useRef } from 'react';
import { tr } from './i18n';
import { getState, goBack, setState, toast } from './store';

const stack: (() => void)[] = [];

/** 打开期间登记一个「返回时怎么关掉自己」；active 为 false 时不登记 */
export function useBack(active: boolean, close: () => void) {
  const ref = useRef(close);
  ref.current = close;
  useEffect(() => {
    if (!active) return;
    const fn = () => ref.current();
    stack.push(fn);
    return () => {
      const i = stack.lastIndexOf(fn);
      if (i >= 0) stack.splice(i, 1);
    };
  }, [active]);
}

export function handleBack(): boolean {
  const top = stack.pop();
  if (top) {
    top();
    return true;
  }
  const s = getState();
  // 命令面板、手机上的左侧导航：只收起，留在原来的页面
  if (s.palette || s.sidebarOpen) {
    setState({ palette: false, sidebarOpen: false });
    return true;
  }
  if (s.canBack) {
    goBack();
    return true;
  }
  // 已经退无可退：2 秒内再按一次才退出，免得误触
  const now = Date.now();
  if (now - lastTry < 2000) return false;
  lastTry = now;
  toast(tr('再按一次退出', 'Press back again to exit'));
  return true;
}

let lastTry = 0;

(window as unknown as { __yiConsoleBack?: () => boolean }).__yiConsoleBack = handleBack;
