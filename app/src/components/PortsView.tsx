import { useEffect, useState } from 'react';
import { getT } from '../i18n';
import { errorToast, toast, useStore } from '../store';
import { hubCall, isTauri, onHubEvent, rpc } from '../transport';
import { writeClipboard } from '../clipboard';
import { Icon } from './Icon';
import { errText } from './StatsView';

const t = getT('portsView');

// Port forwarding: connect a port on the host to 127.0.0.1 on this machine so dev servers on remote machines open in the local browser.
// SSH hosts use SSH's own port forwarding; relay hosts go through a relay channel, with yi-agentd connecting to its local port (yi-agentd itself listens on no ports).

export interface ForwardInfo {
  hostId: string;
  port: number;
  localPort: number;
  direct: boolean;
  conns: number;
  error: string | null;
  label: string | null;
}

interface Listening {
  port: number;
  addr: string;
  pid?: number | null;
  process?: string | null;
}

export const isMobileApp = isTauri && /Android|iPhone|iPad/i.test(navigator.userAgent);

/** Open a URL: the system browser on desktop, the in-app browser in the mobile app (Android Custom Tabs, whose menu can switch to another browser), a new tab in the browser build */
export function openUrl(url: string) {
  // On mobile ClientHub can't open the system browser, so hand it to the opener plugin
  if (isMobileApp) import('@tauri-apps/plugin-opener').then((m) => m.openUrl(url, 'inAppBrowser')).catch(errorToast);
  else if (isTauri) hubCall('url.open', { url }).catch(errorToast);
  else window.open(url, '_blank', 'noopener');
}

/** Open in another browser on mobile: Android shows the system app chooser (YiConsoleAndroid injected by MainActivity), otherwise the default browser */
export function openUrlExternal(url: string) {
  const native = (window as { YiConsoleAndroid?: { openExternal(u: string): void } }).YiConsoleAndroid;
  if (isMobileApp && native) native.openExternal(url);
  else if (isMobileApp) import('@tauri-apps/plugin-opener').then((m) => m.openUrl(url)).catch(errorToast);
  else openUrl(url);
}

export const forwardUrl = (f: ForwardInfo) => `http://localhost:${f.localPort}/`;

/** Forward a port on the host (the local host needs no forwarding and gets the original port back) */
export async function forwardPort(hostId: string, port: number, localPort?: number): Promise<ForwardInfo> {
  return hubCall<ForwardInfo>('ports.forward', { id: hostId, port, ...(localPort ? { localPort } : {}) });
}

export function PortsView({ hostId }: { hostId: string }) {
  const host = useStore((s) => s.hosts.find((h) => h.config.id === hostId));
  const connected = host?.status.status === 'connected';
  const isLocal = host?.config.kind === 'local';
  const [forwards, setForwards] = useState<ForwardInfo[]>([]);
  const [listening, setListening] = useState<Listening[] | null>(null);
  const [listErr, setListErr] = useState('');
  const [port, setPort] = useState('');
  const [localPort, setLocalPort] = useState('');
  const [busy, setBusy] = useState(false);

  const loadForwards = () =>
    hubCall<ForwardInfo[]>('ports.forwards', { id: hostId })
      .then(setForwards)
      .catch(() => {});
  const loadListening = () =>
    rpc<{ ports: Listening[] }>(hostId, 'ports.list')
      .then((r) => {
        setListening(r.ports);
        setListErr('');
      })
      .catch((e) => setListErr(errText(e)));

  useEffect(() => {
    loadForwards();
    const off = onHubEvent((ev) => ev.type === 'ports.changed' && loadForwards());
    // Connection counts and errors change in the client and aren't in events; refresh periodically while the page is open
    const t = setInterval(loadForwards, 3000);
    return () => {
      off();
      clearInterval(t);
    };
  }, [hostId]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (!connected) return;
    loadListening();
    const t = setInterval(loadListening, 5000);
    return () => clearInterval(t);
  }, [hostId, connected]); // eslint-disable-line react-hooks/exhaustive-deps

  const add = async (p: number, lp?: number, open = false) => {
    setBusy(true);
    try {
      const f = await forwardPort(hostId, p, lp);
      setPort('');
      setLocalPort('');
      await loadForwards();
      if (open) openUrl(forwardUrl(f));
      else if (!f.direct) toast(t('forwardedLocalhostPort', { localPort: f.localPort, v: host?.config.name ?? '', p }));
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };
  const stop = async (f: ForwardInfo) => {
    await hubCall('ports.unforward', { id: hostId, port: f.port }).catch(errorToast);
    loadForwards();
  };
  const submit = () => {
    const p = Number(port);
    const lp = localPort ? Number(localPort) : undefined;
    if (!Number.isInteger(p) || p < 1 || p > 65535) return errorToast(t('portMustBeBetween1'));
    if (lp !== undefined && (!Number.isInteger(lp) || lp < 1 || lp > 65535)) return errorToast(t('localPortMustBeBetween'));
    add(p, lp);
  };

  const byPort = new Map(forwards.map((f) => [f.port, f]));
  const loopback = (a: string) => a.startsWith('127.') || a === '::1';

  return (
    <div className="page ports">
      <div className="page-head">
        <h2>{t('portForwarding')} · {host?.config.name ?? hostId}</h2>
        <button className="btn small" onClick={() => (loadForwards(), connected && loadListening())}>
          <Icon name="refresh" size={14} /> {t('refresh')}
        </button>
      </div>
      <p className="muted small">
        {isLocal
          ? t('thisIsTheLocalMachine')
          : t('remoteIntro', { name: host?.config.name ?? t('theHostFallback') })}
        {!isTauri && t('inTheBrowserBuildThe')}
      </p>

      {!isLocal && (
        <div className="card ports-add">
          <div className="row">
            <label className="ports-field">
              <span>{t('hostPort')}</span>
              <input value={port} onChange={(e) => setPort(e.target.value.replace(/\D/g, ''))} placeholder={t('eG3000')} inputMode="numeric" onKeyDown={(e) => e.key === 'Enter' && submit()} />
            </label>
            <label className="ports-field">
              <span>{t('localPortOptional')}</span>
              <input value={localPort} onChange={(e) => setLocalPort(e.target.value.replace(/\D/g, ''))} placeholder={t('sameNumberByDefaultPicks')} inputMode="numeric" onKeyDown={(e) => e.key === 'Enter' && submit()} />
            </label>
            <button className="btn primary" disabled={busy || !port || !connected} onClick={submit}>
              {t('forward')}
            </button>
          </div>
          {!connected && <div className="muted small">{t('hostIsOfflineConnectIt')}</div>}
        </div>
      )}

      {!isLocal && (
        <>
          <div className="section-title">{t('forwarded')}</div>
          {forwards.length === 0 ? (
            <div className="muted small ports-none">{t('nothingForwardedYetEnterA')}</div>
          ) : (
            <div className="ports-list">
              {forwards.map((f) => (
                <div key={f.port} className="ports-row">
                  <span className="ports-port mono">{f.port}</span>
                  <Icon name="forward" size={12} />
                  <button className="link-btn mono" title={t('openInBrowser')} onClick={() => openUrl(forwardUrl(f))}>
                    localhost:{f.localPort}
                  </button>
                  <span className="grow ports-note truncate">
                    {f.error ? <span className="ports-err" title={f.error}>{f.error}</span> : f.conns > 0 ? <span className="muted">{t('connections', { conns: f.conns })}</span> : null}
                  </span>
                  <button className="icon-btn" title={t('openInBrowser')} onClick={() => openUrl(forwardUrl(f))}>
                    <Icon name="external" size={14} />
                  </button>
                  <button className="icon-btn" title={t('copyAddress')} onClick={() => writeClipboard(forwardUrl(f)).then(() => toast(t('copied')))}>
                    <Icon name="copy" size={14} />
                  </button>
                  <button className="icon-btn" title={t('stopForwarding')} onClick={() => stop(f)}>
                    <Icon name="x" size={14} />
                  </button>
                </div>
              ))}
            </div>
          )}
        </>
      )}

      <div className="section-title">{t('portsListeningOnTheHost')}</div>
      {!connected ? (
        <div className="muted small ports-none">{t('hostIsOffline')}</div>
      ) : listErr ? (
        <div className="muted small ports-none">{listErr}</div>
      ) : listening === null ? (
        <div className="muted small ports-none">{t('loading')}</div>
      ) : listening.length === 0 ? (
        <div className="muted small ports-none">{t('noListeningPortsFound')}</div>
      ) : (
        <div className="ports-list">
          {listening.map((l) => {
            const f = byPort.get(l.port);
            return (
              <div key={l.port} className="ports-row">
                <span className="ports-port mono">{l.port}</span>
                <span className="ports-proc truncate" title={l.pid ? t('pid', { pid: l.pid }) : undefined}>
                  {l.process || <span className="muted">{t('unknownProcess')}</span>}
                </span>
                <span className="grow muted small mono truncate">{loopback(l.addr) ? t('localOnly') : l.addr === '::' || l.addr === '0.0.0.0' ? t('allAddresses') : l.addr}</span>
                {isLocal ? (
                  <button className="btn small" onClick={() => openUrl(`http://localhost:${l.port}/`)}>
                    {t('open')}
                  </button>
                ) : f ? (
                  <button className="btn small" onClick={() => openUrl(forwardUrl(f))} title={t('forwardedToLocalhost', { localPort: f.localPort })}>
                    {t('open')}
                  </button>
                ) : (
                  <>
                    <button className="btn small" disabled={busy} onClick={() => add(l.port)}>
                      {t('forward')}
                    </button>
                    <button className="btn small" disabled={busy} onClick={() => add(l.port, undefined, true)} title={t('forwardAndOpenInThe')}>
                      {t('forwardAndOpen')}
                    </button>
                  </>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
