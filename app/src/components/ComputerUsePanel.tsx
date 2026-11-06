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
  lastWindow?: Target;
  /** macOS: the driver's grants, read while computer use is on */
  permissions?: { accessibility: boolean; screenRecording: boolean };
  driverError?: string;
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
  // The first macOS read starts the driver and can take a while: don't stack polls behind it
  const reading = useRef(false);
  const refresh = useCallback(async () => {
    if (!connected || reading.current) return;
    reading.current = true;
    try {
      setState(await rpc(hostId, 'computerUse.status', { sessionId: info.id }));
    } catch {
      setState(undefined);
    } finally {
      reading.current = false;
    }
  }, [hostId, info.id, connected]);
  useEffect(() => {
    void refresh();
  }, [refresh, info.computerUse, info.status, revision]);
  const waitingGrant = state?.status === 'permissionsRequired';
  useEffect(() => {
    if (!state?.enabled || !connected) return;
    // Faster while the user is granting in System Settings
    const timer = window.setInterval(() => void refresh(), waitingGrant && open ? 2000 : 5000);
    return () => window.clearInterval(timer);
  }, [state?.enabled, connected, refresh, waitingGrant, open]);
  const change = async (action: string) => {
    setBusy(true);
    setError('');
    try {
      setState(await rpc(hostId, 'session.computerUse', { sessionId: info.id, action }));
      setOpen(false);
    } catch (e) {
      setError(trErr(e instanceof Error ? e.message : String(e)));
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };
  const openSettings = async (kind: 'accessibility' | 'screenRecording') => {
    setError('');
    try {
      setState(await rpc(hostId, 'session.computerUse', { sessionId: info.id, action: `settings:${kind}` }));
    } catch (e) {
      setError(trErr(e instanceof Error ? e.message : String(e)));
    }
  };
  const statusLabel =
    state?.status === 'desktopUnavailable'
      ? t('desktopUnavailable')
      : state?.status === 'permissionsRequired'
        ? t('permissionsRequired')
        : state?.paused
          ? t('paused')
          : t('ready');
  return (
    <>
      <div className="computer-use-bar">
        <button
          className={`tool-chip computer-use-btn ${state?.enabled ? 'active' : ''}`}
          aria-pressed={!!state?.enabled}
          disabled={!connected}
          aria-label={t('title')}
          title={state?.enabled ? (state.lastWindow?.title ? t('activeTitle', { title: state.lastWindow.title, status: statusLabel }) : statusLabel) : t('title')}
          onClick={() => {
            setOpen(true);
            void refresh();
          }}
        >
          <Icon name="laptop" size={15} />
        </button>
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
                const controls = Array.from(e.currentTarget.querySelectorAll<HTMLElement>('button:not(:disabled)'));
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
            {state?.status === 'driverMissing' ? (
              <p role="status">{t('driverMissing')}</p>
            ) : state?.supported ? (
              <>
                <p className="muted small">{t('scope')}</p>
                {state.enabled && state.permissions && waitingGrant && (
                  <div className="computer-use-grants">
                    <p className="small">{t('permissionsHint')}</p>
                    {(['accessibility', 'screenRecording'] as const).map((kind) => (
                      <div className="computer-use-grant" key={kind}>
                        <span>{t(kind)}</span>
                        <span className={state.permissions![kind] ? 'ok' : 'muted'}>{state.permissions![kind] ? t('granted') : t('notGranted')}</span>
                        {!state.permissions![kind] && (
                          <button className="btn small" onClick={() => void openSettings(kind)}>
                            {t('openSettings')}
                          </button>
                        )}
                      </div>
                    ))}
                    <p className="muted small">{t('permissionsRetry')}</p>
                  </div>
                )}
                {state.enabled && state.driverError && (
                  <p className="error-text" role="alert">
                    {t('driverError', { message: trErr(state.driverError) })}
                  </p>
                )}
                {error && (
                  <p className="error-text" role="alert">
                    {error}
                  </p>
                )}
                <div className="dlg-actions">
                  <button className="btn" onClick={() => setOpen(false)}>
                    {state.enabled ? t('close') : t('cancel')}
                  </button>
                  {state.enabled ? (
                    <button className="btn danger" disabled={busy || !connected} onClick={() => void change('stop')}>
                      {t('disable')}
                    </button>
                  ) : (
                    <button
                      className="btn primary"
                      disabled={busy || info.status === 'running' || info.status === 'waiting-permission'}
                      onClick={() => void change('enable')}
                    >
                      {t('enable')}
                    </button>
                  )}
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
