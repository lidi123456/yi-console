import { useCallback, useEffect, useRef, useState } from 'react';
import { useBack } from '../back';
import { getT, trErr } from '../i18n';
import { errorToast } from '../store';
import { rpc } from '../transport';
import type { SessionInfo } from '../types';
import { Icon } from './Icon';

const t = getT('computerUse');
interface Target {
  pid: number;
  windowId: number;
  title: string;
}
interface DesktopState {
  supported: boolean;
  driverVersion: string;
  enabled: boolean;
  paused: boolean;
  status: string;
  target?: Target;
  ownerSessionId?: string;
}

export function ComputerUsePanel({
  hostId,
  info,
  connected,
  revision,
}: {
  hostId: string;
  info: SessionInfo;
  connected: boolean;
  revision: number;
}) {
  const [open, setOpen] = useState(false);
  const [state, setState] = useState<DesktopState>();
  const [targets, setTargets] = useState<Target[]>([]);
  const [selected, setSelected] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const dialog = useRef<HTMLElement>(null);
  useBack(open, () => setOpen(false));
  useEffect(() => {
    if (!open) return;
    const previous = document.activeElement as HTMLElement | null;
    dialog.current?.focus();
    return () => {
      if (previous?.isConnected) previous.focus();
    };
  }, [open]);
  const refresh = useCallback(async () => {
    if (!connected) return;
    try {
      setState(await rpc(hostId, 'computerUse.status', { sessionId: info.id }));
    } catch {
      setState(undefined);
    }
  }, [hostId, info.id, connected]);
  useEffect(() => {
    void refresh();
  }, [refresh, info.computerUse, info.status, revision]);
  useEffect(() => {
    if (!state?.enabled || !connected) return;
    const timer = window.setInterval(() => void refresh(), 5000);
    return () => window.clearInterval(timer);
  }, [state?.enabled, connected, refresh]);
  const scan = async () => {
    setBusy(true);
    setError('');
    try {
      const list = await rpc<Target[]>(hostId, 'computerUse.targets');
      setTargets(list);
      setSelected((old) => (list.some((item) => String(item.windowId) === old) ? old : String(list[0]?.windowId ?? '')));
    } catch (e) {
      setError(trErr(e instanceof Error ? e.message : String(e)));
    } finally {
      setBusy(false);
    }
  };
  const change = async (action: string) => {
    setBusy(true);
    setError('');
    try {
      const target = targets.find((item) => String(item.windowId) === selected);
      setState(await rpc(hostId, 'session.computerUse', { sessionId: info.id, action, ...(action === 'enable' ? { target } : {}) }));
      if (action === 'enable') setOpen(false);
    } catch (e) {
      setError(trErr(e instanceof Error ? e.message : String(e)));
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };
  const statusLabel = state?.status === 'desktopUnavailable' ? t('desktopUnavailable') : state?.paused ? t('paused') : t('ready');
  return (
    <>
      <div className="computer-use-bar">
        <button
          className={`tool-chip computer-use-btn ${state?.enabled ? 'active' : ''}`}
          disabled={!connected}
          aria-label={t('title')}
          title={state?.enabled ? t('activeTitle', { title: state.target?.title ?? '', status: statusLabel }) : t('title')}
          onClick={() => {
            setOpen(true);
            void refresh();
          }}
        >
          <Icon name="laptop" size={15} />
        </button>
        {state?.enabled ? (
          <>
            <span
              className={`computer-use-dot ${state.paused ? 'paused' : ''}`}
              role="status"
              aria-label={statusLabel}
              title={statusLabel}
            />
            <button
              className="icon-btn computer-use-capture"
              title={t('capture')}
              aria-label={t('capture')}
              disabled={busy || !connected || state.paused}
              onClick={() => void change('capture')}
            >
              <Icon name="image" size={15} />
            </button>
            <button
              className="icon-btn computer-use-pause"
              title={state.paused ? t('resume') : t('pause')}
              aria-label={state.paused ? t('resume') : t('pause')}
              disabled={busy || !connected}
              onClick={() => void change(state.paused ? 'resume' : 'pause')}
            >
              <Icon name={state.paused ? 'play' : 'pause'} size={15} />
            </button>
            <button
              className="icon-btn"
              title={t('stop')}
              aria-label={t('stop')}
              disabled={!connected}
              onClick={() => void change('stop')}
            >
              <Icon name="stop" size={15} />
            </button>
          </>
        ) : null}
      </div>
      {open && (
        <div className="modal-bg" onMouseDown={() => setOpen(false)}>
          <section
            ref={dialog}
            tabIndex={-1}
            className="modal computer-use-setup"
            role="dialog"
            aria-modal="true"
            aria-label={t('title')}
            onMouseDown={(e) => e.stopPropagation()}
            onKeyDown={(e) => {
              if (e.key === 'Escape') setOpen(false);
              if (e.key === 'Tab') {
                const controls = Array.from(e.currentTarget.querySelectorAll<HTMLElement>('button:not(:disabled), select:not(:disabled)'));
                const first = controls[0];
                const last = controls[controls.length - 1];
                if (e.shiftKey && (document.activeElement === first || document.activeElement === e.currentTarget)) {
                  e.preventDefault();
                  last?.focus();
                } else if (!e.shiftKey && document.activeElement === last) {
                  e.preventDefault();
                  first?.focus();
                }
              }
            }}
          >
            <div className="computer-use-heading">
              <strong>{t('title')}</strong>
              <button className="icon-btn" aria-label={t('close')} onClick={() => setOpen(false)}>
                <Icon name="x" />
              </button>
            </div>
            <p className="muted">{t('description')}</p>
            {state?.supported ? (
              <>
                <div className="computer-use-select">
                  <label htmlFor="cu-target">{t('targetWindow')}</label>
                  <button className="btn small" disabled={busy} onClick={() => void scan()}>
                    {busy ? t('checking') : t('refreshWindows')}
                  </button>
                </div>
                <select id="cu-target" value={selected} onChange={(e) => setSelected(e.target.value)} disabled={busy}>
                  <option value="">{t('chooseWindow')}</option>
                  {targets.map((target) => (
                    <option key={target.windowId} value={String(target.windowId)}>
                      {target.title}
                    </option>
                  ))}
                </select>
                <p className="muted small">{t('scope')}</p>
                {state.status === 'busy' && <p role="status">{t('occupied')}</p>}
                {error && (
                  <p className="error-text" role="alert">
                    {error}
                  </p>
                )}
                {state.enabled && (
                  <div className="computer-use-dialog-controls">
                    <button className="btn small" disabled={busy || !connected || state.paused} onClick={() => void change('capture')}>
                      {t('capture')}
                    </button>
                    <button className="btn small" disabled={busy || !connected} onClick={() => void change(state.paused ? 'resume' : 'pause')}>
                      {state.paused ? t('resume') : t('pause')}
                    </button>
                    <button className="btn small" disabled={!connected} onClick={() => void change('stop')}>
                      {t('stop')}
                    </button>
                  </div>
                )}
                <div className="dlg-actions">
                  <button className="btn" onClick={() => setOpen(false)}>
                    {t('cancel')}
                  </button>
                  <button
                    className="btn primary"
                    disabled={busy || !selected || info.status === 'running' || info.status === 'waiting-permission'}
                    onClick={() => void change('enable')}
                  >
                    {t('enable')}
                  </button>
                </div>
                {(info.status === 'running' || info.status === 'waiting-permission') && <p className="muted small">{t('waitForTurn')}</p>}
              </>
            ) : (
              <p role="status">{state?.supported === false ? t('windowsOnly') : t('unavailable')}</p>
            )}
          </section>
        </div>
      )}
    </>
  );
}
