import { useCallback, useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useBack } from '../back';
import { getT, trErr } from '../i18n';
import { errorToast } from '../store';
import { rpc } from '../transport';
import type { SessionInfo } from '../types';
import { Icon } from './Icon';

const t = getT('browserUse');

interface BrowserState {
  /** browserMissing / disabled / ready / paused */
  status: string;
  enabled: boolean;
  paused: boolean;
  browserPath?: string | null;
  running: boolean;
  version?: string | null;
  tabs?: { id: string; url: string; title: string; current: boolean }[] | null;
  dialog?: { kind: string; message: string } | null;
}

/** Composer switch for the AI browser (`yi_browser`): like ComputerUsePanel, only on / off */
export function BrowserUsePanel({ hostId, info, connected, revision }: { hostId: string; info: SessionInfo; connected: boolean; revision: number }) {
  const [open, setOpen] = useState(false);
  const [state, setState] = useState<BrowserState>();
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
      setState(await rpc(hostId, 'browserUse.status', { sessionId: info.id }));
    } catch {
      setState(undefined);
    }
  }, [hostId, info.id, connected]);
  useEffect(() => {
    void refresh();
  }, [refresh, info.browserUse, info.status, revision]);
  const change = async (action: string) => {
    setBusy(true);
    setError('');
    try {
      setState(await rpc(hostId, 'session.browserUse', { sessionId: info.id, action }));
      setOpen(false);
    } catch (e) {
      setError(trErr(e instanceof Error ? e.message : String(e)));
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };
  const enabled = !!state?.enabled || !!info.browserUse;
  const current = state?.tabs?.find((x) => x.current);
  const statusLabel = state?.paused ? t('paused') : state?.running ? t('browserRunning') : t('ready');
  const turn = info.status === 'running' || info.status === 'waiting-permission';
  return (
    <>
      <div className="computer-use-bar">
        <button
          className={`tool-chip computer-use-btn ${enabled ? 'active' : ''}`}
          aria-pressed={enabled}
          disabled={!connected}
          aria-label={t('title')}
          title={enabled ? (current?.url ? t('activeTitle', { url: current.url, status: statusLabel }) : statusLabel) : t('title')}
          onClick={() => {
            setOpen(true);
            void refresh();
          }}
        >
          <Icon name="globe" size={15} />
        </button>
      </div>
      {open &&
        createPortal(
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
              }}
            >
              <div className="computer-use-heading">
                <strong>{t('title')}</strong>
                <button className="icon-btn" aria-label={t('close')} onClick={() => setOpen(false)}>
                  <Icon name="x" />
                </button>
              </div>
              <p className="muted">{t('description')}</p>
              {!state ? (
                <p role="status">{t('unavailable')}</p>
              ) : state.status === 'browserMissing' ? (
                <p role="status">{t('browserMissing')}</p>
              ) : (
                <>
                  <p className="muted small">{t('scope')}</p>
                  {state.browserPath && <p className="muted small truncate" title={state.browserPath}>{t('browserPath', { path: state.browserPath })}</p>}
                  {state.enabled && state.running && (
                    <p className="small">{t('runningTabs', { version: state.version ?? '', count: state.tabs?.length ?? 0 })}</p>
                  )}
                  {state.enabled && current && <p className="muted small truncate" title={current.url}>{t('currentPage', { url: current.url })}</p>}
                  {state.enabled && state.dialog && <p className="small">{t('dialogWaiting', { kind: state.dialog.kind, message: state.dialog.message })}</p>}
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
                      <button className="btn primary" disabled={busy || turn} onClick={() => void change('enable')}>
                        {t('enable')}
                      </button>
                    )}
                  </div>
                  {!state.enabled && turn && <p className="muted small">{t('waitForTurn')}</p>}
                </>
              )}
            </section>
          </div>,
          document.body,
        )}
    </>
  );
}
