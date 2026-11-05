import QRCode from 'qrcode';
import { useEffect, useRef, useState } from 'react';
import { errorToast, navigate, refreshHosts, toast, useStore, type HostEntry } from '../store';
import { hubCall, rpc } from '../transport';
import type { HostConfig, SshTarget } from '../types';
import { readClipboard, writeClipboard } from '../clipboard';
import { getT, useLang } from '../i18n';
import { askConfirm, askText } from './Overlay';
import { HostTools } from './HostTools';
import { Icon } from './Icon';
import { QrScanner, canScanQr } from './QrScanner';

const tx = getT('hostsView');

const emptySsh = (): SshTarget => ({
  host: '',
  port: 22,
  user: '',
  auth: 'agent',
  keyPath: '',
  keyText: '',
  passphrase: '',
  password: '',
  jumpHostId: '',
});

function newHost(kind: 'ssh' | 'relay'): HostConfig {
  return {
    id: '',
    name: '',
    kind,
    ssh: kind === 'ssh' ? emptySsh() : null,
    pairing: kind === 'relay' ? '' : null,
    autoConnect: true,
    installService: true,
    relayShare: true,
  };
}

const isAndroid = /Android/i.test(navigator.userAgent);

function statusText(h: HostEntry) {
  const s = h.status.status;
  return s === 'connected' ? tx('connected') : s === 'connecting' ? tx('connecting') : s === 'error' ? tx('connectionFailed') : tx('notConnected');
}

function HostKeyPrompt({ h, onDone }: { h: HostEntry; onDone: () => void }) {
  const code = h.status.code;
  const fp = (h.status.error ?? '').replace(/^HOSTKEY_[A-Z]+:/, '');
  const changed = code === 'HOSTKEY_CHANGED';
  const trust = async () => {
    try {
      await hubCall('hosts.trustKey', { id: h.config.id, fingerprint: fp });
      await hubCall('hosts.connect', { id: h.config.id });
      onDone();
    } catch (e) {
      // The jump host may also be unknown; go through it again
      errorToast(e);
      refreshHosts();
    }
  };
  return (
    <div className={`callout ${changed ? 'callout-err' : 'callout-warn'}`}>
      <div className="callout-title">
        <Icon name="key" size={16} /> {changed ? tx('hostKeyChanged') : tx('firstConnectionConfirmTheHost')}
      </div>
      <div className="mono small">{fp}</div>
      {changed ? (
        <div className="small">
          {tx('theServerMayHaveBeen')}
        </div>
      ) : (
        <div className="row">
          <span className="small muted">{tx('compareWithTheOutputOf')}</span>
          <button className="btn primary small" onClick={trust}>
            {tx('trustAndConnect')}
          </button>
        </div>
      )}
    </div>
  );
}

interface RelayInfo {
  configured: boolean;
  enabled?: boolean;
  online?: boolean;
  url?: string;
  fp?: string;
  deviceId?: string;
  name?: string;
  error?: string;
  clients?: number;
}

interface LinkInfo {
  kind: 'pair' | 'relay';
  name?: string;
  url: string;
  pinned: boolean;
  hasInvite?: boolean;
}

interface PairedClient {
  id: string;
  name: string;
  createdAt: number;
  lastSeen: number;
  online: boolean;
}

const hostOf = (url = '') => url.replace(/^wss?:\/\//, '');

async function copy(text: string, what: string) {
  if (await writeClipboard(text)) toast(tx('copied', { what }));
  else errorToast(tx('copyFailedSelectTheText'));
}

function ago(t: number) {
  if (!t) return tx('never');
  const s = Math.max(0, Date.now() / 1000 - t);
  if (s < 60) return tx('justNow');
  if (s < 3600) return tx('minAgo', { v: Math.floor(s / 60) });
  if (s < 86400) return tx('hAgo', { v: Math.floor(s / 3600) });
  return tx('dAgo', { v: Math.floor(s / 86400) });
}

/** On this machine, "let other devices connect to this computer": ① connect to a relay (register with an invite the first time) → ② generate a one-time pairing QR code → manage paired devices */
function RelayPanel({ hostId }: { hostId: string }) {
  const [info, setInfo] = useState<RelayInfo | null>(null);
  const [known, setKnown] = useState<{ hostId: string; name: string; url: string }[]>([]);
  const [link, setLink] = useState('');
  const [linkInfo, setLinkInfo] = useState<LinkInfo | string | null>(null);
  const [manual, setManual] = useState(false);
  const [changing, setChanging] = useState(false);
  const [url, setUrl] = useState('');
  const [invite, setInvite] = useState('');
  const [fp, setFp] = useState('');
  const [busy, setBusy] = useState(false);
  const [offer, setOffer] = useState<{ uri: string; qr: string; expiresAt: number } | null>(null);
  const [now, setNow] = useState(Date.now() / 1000);
  const [clients, setClients] = useState<PairedClient[]>([]);

  const load = () =>
    rpc<RelayInfo>(hostId, 'relay.pairing', {})
      .then((r) => {
        setInfo(r);
        if (r.url) setUrl(r.url);
        setFp(r.fp ?? '');
        return r;
      })
      .catch(() => setInfo(null));
  const loadClients = () =>
    rpc<PairedClient[]>(hostId, 'relay.clients', {})
      .then((list) => {
        setClients((old) => {
          // A device was added while the QR code was shown, or a paired device paired again (pairing time changed): pairing succeeded
          const added = list.find((c) => !old.some((o) => o.id === c.id && o.createdAt === c.createdAt));
          if (offer && added) {
            toast(tx('paired', { v: added?.name ?? '' }));
            setOffer(null);
          }
          return list;
        });
      })
      .catch(() => {});
  useEffect(() => {
    load();
    loadClients();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId]);
  // When saved relay devices change (one just paired), list the reusable relays again
  const relayHosts = useStore((s) => s.hosts.filter((h) => h.config.kind === 'relay').map((h) => h.config.id).join(','));
  useEffect(() => {
    hubCall<{ hostId: string; name: string; url: string }[]>('relay.known').then(setKnown).catch(() => {});
  }, [relayHosts]);

  const active = !!(info?.configured && info.enabled);
  // Just enabled and not yet connected: refresh the status every 2 s, stop once connected or on error
  useEffect(() => {
    if (!active || info?.online || info?.error) return;
    const t = setTimeout(load, 2000);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active, info]);

  // While the QR code is shown: count down, and check every 3 s whether pairing succeeded
  useEffect(() => {
    if (!offer) return;
    const t = setInterval(() => setNow(Date.now() / 1000), 1000);
    const p = setInterval(loadClients, 3000);
    return () => {
      clearInterval(t);
      clearInterval(p);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [offer]);

  useEffect(() => {
    const l = link.trim();
    if (!l) return setLinkInfo(null);
    hubCall<LinkInfo>('relay.parseLink', { uri: l })
      .then(setLinkInfo)
      .catch((e) => setLinkInfo(String(e?.message ?? e)));
  }, [link]);

  const run = async (p: Promise<unknown>, ok: string) => {
    setBusy(true);
    try {
      await p;
      toast(ok);
      setLink('');
      setInvite('');
      setChanging(false);
      setManual(false);
      setTimeout(load, 800);
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };
  const configure = (params: object, ok = tx('relayIsOnConnecting')) =>
    run(rpc(hostId, 'relay.configure', { url, fp, enabled: true, ...params }), ok);

  const startPair = async () => {
    setBusy(true);
    try {
      const r = await rpc<{ uri: string; expiresAt: number }>(hostId, 'relay.pairStart', {});
      const qr = await QRCode.toDataURL(r.uri, { margin: 1, width: 240 });
      setNow(Date.now() / 1000);
      setOffer({ uri: r.uri, qr, expiresAt: r.expiresAt });
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const revoke = async (c: PairedClient) => {
    if (
      !(await askConfirm({
        title: tx('remove', { name: c.name }),
        message: tx('itIsDisconnectedRightAway'),
        okText: tx('remove2'),
        danger: true,
      }))
    )
      return;
    try {
      setClients(await rpc<PairedClient[]>(hostId, 'relay.revoke', { id: c.id }));
      toast(tx('removed', { name: c.name }));
    } catch (e) {
      errorToast(e);
    }
  };

  const rename = async (c: PairedClient) => {
    const name = await askText({ title: tx('renameDevice'), value: c.name, okText: tx('save') });
    if (!name || name === c.name) return;
    try {
      setClients(await rpc<PairedClient[]>(hostId, 'relay.renameClient', { id: c.id, name }));
    } catch (e) {
      errorToast(e);
    }
  };

  const shareRelay = async () => {
    setBusy(true);
    try {
      const r = await rpc<{ link: string }>(hostId, 'relay.invite', {});
      await copy(r.link, tx('relayAccessLinkValidFor'));
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  if (!info) return null;
  const setupMode = !active || changing;
  const left = offer ? Math.max(0, Math.floor(offer.expiresAt - now)) : 0;
  const expired = !!offer && left <= 0;

  return (
    <div className="card relay-panel" id="relay-panel">
      <div className="card-title">
        <Icon name="cloud" size={16} /> {tx('letOtherDevicesConnectTo')}
        <span className="grow" />
        {active && (
          <span className={`pill ${info.online ? 'pill-ok' : info.error ? 'pill-err' : ''}`}>
            {info.online ? tx('reachable') : info.error ? tx('relayConnectionFailed') : tx('connectingToRelay')}
          </span>
        )}
      </div>
      <p className="muted small">
        {tx('whenAPhoneOrAnother')}
      </p>

      <div className="step">
        <div className="step-no">{active && info.online ? <Icon name="check" size={14} /> : '1'}</div>
        <div className="step-body">
          <div className="step-title">{tx('connectToARelay')}</div>
          {!setupMode ? (
            <>
              <div className="small">
                {hostOf(info.url)}
                {info.online ? <span className="ok-text">{tx('connected2')}</span> : !info.error && <span className="muted">{tx('connecting2')}</span>}
              </div>
              {info.error && <div className="error-text small">{info.error}</div>}
              <div className="row">
                <button className="btn small" onClick={() => setChanging(true)}>
                  {info.error ? tx('useAnotherAccessLink') : tx('changeRelay')}
                </button>
                <button className="btn small" disabled={busy} onClick={() => run(rpc(hostId, 'relay.configure', { url: info.url, enabled: false }), tx('turnedOffOtherDevicesCannot'))}>
                  {tx('turnOff')}
                </button>
              </div>
            </>
          ) : (
            <>
              {known.length > 0 && (
                <>
                  <div className="muted small">{tx('thisAppAlreadyUsesA')}</div>
                  <div className="row wrap">
                    {known.map((k) => (
                      <button
                        key={k.hostId}
                        className="btn primary small"
                        disabled={busy}
                        onClick={() => run(hubCall('relay.reuse', { hostId, fromHostId: k.hostId }), tx('relayIsOnConnecting'))}
                      >
                        {tx('use')} {hostOf(k.url)}
                        <span className="small" style={{ opacity: 0.7 }}>{tx('via', { name: k.name })}</span>
                      </button>
                    ))}
                  </div>
                </>
              )}
              <div className="muted small">
                {known.length > 0 ? tx('orPasteA') : tx('pasteA')}
                <b>{tx('relayAccessLink')}</b>
                {tx('theRelayOperatorGeneratesIt')}
              </div>
              <div className="row">
                <input className="grow" value={link} onChange={(e) => setLink(e.target.value)} placeholder="yiconsole://relay?…" />
                <button className="btn small" onClick={async () => setLink(await readClipboard())}>
                  <Icon name="copy" size={14} /> {tx('paste')}
                </button>
              </div>
              {typeof linkInfo === 'string' && <div className="error-text small">{linkInfo}</div>}
              {linkInfo && typeof linkInfo === 'object' && (
                <>
                  <div className="row">
                    <span className="small grow">
                      {tx('relay')} <b>{hostOf(linkInfo.url)}</b>
                      {linkInfo.kind === 'pair' && <span className="muted">{tx('fromSPairingLink', { name: linkInfo.name ?? '' })}</span>}
                    </span>
                    <button className="btn primary small" disabled={busy} onClick={() => configure({ link: link.trim() })}>
                      {tx('turnOn')}
                    </button>
                  </div>
                  {!linkInfo.hasInvite && (
                    <div className="attn-text small">
                      {tx('thisLinkHasNoInvite')}
                    </div>
                  )}
                </>
              )}
              <button className="link-btn small" onClick={() => setManual(!manual)}>
                {manual ? tx('hideManualEntry') : tx('noLinkEnterTheRelay')}
              </button>
              {manual && (
                <div className="form">
                  <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder={tx('relayAddressEGWss')} />
                  <input value={invite} onChange={(e) => setInvite(e.target.value)} placeholder={tx('inviteCodeFromYiRelay')} />
                  <input value={fp} onChange={(e) => setFp(e.target.value)} placeholder={tx('certificateFingerprintForASelf')} />
                  <div className="row">
                    <button className="btn primary small" disabled={!url || busy} onClick={() => configure({ invite })}>
                      {tx('turnOn')}
                    </button>
                  </div>
                </div>
              )}
              {changing && (
                <button className="btn small" onClick={() => setChanging(false)}>
                  {tx('cancel')}
                </button>
              )}
            </>
          )}
        </div>
      </div>

      <div className={`step ${active ? '' : 'disabled'}`}>
        <div className="step-no">2</div>
        <div className="step-body">
          <div className="step-title">{tx('addThisComputerOnAnother')}</div>
          {!active ? (
            <div className="muted small">{tx('finishStep1First')}</div>
          ) : !offer ? (
            <>
              <div className="muted small">{tx('generateAOneTimePairing')}</div>
              <button className="btn primary small" disabled={busy} onClick={startPair}>
                <Icon name="qr" size={14} /> {tx('generatePairingQrCode')}
              </button>
            </>
          ) : (
            <div className="pair-row">
              {!expired && <img className="pair-qr" src={offer.qr} alt={tx('pairingQrCode')} width={168} height={168} />}
              <div className="pair-help small">
                {expired ? (
                  <div className="attn-text">{tx('theQrCodeHasExpired')}</div>
                ) : (
                  <>
                    <div>
                      <b>{tx('phone')}</b>
                      {tx('openYiConsoleHostsPair')}
                    </div>
                    <div>
                      <b>{tx('anotherComputer')}</b>
                      {tx('copyThePairingLinkSend')}
                    </div>
                    <div className="muted">
                      {tx('expiresInSingleUse', { v: Math.floor(left / 60), v2: String(left % 60).padStart(2, '0') })}
                    </div>
                  </>
                )}
                <div className="row wrap">
                  {!expired && (
                    <button className="btn primary small" onClick={() => copy(offer.uri, tx('pairingLink'))}>
                      <Icon name="copy" size={14} /> {tx('copyPairingLink')}
                    </button>
                  )}
                  <button className="btn small" disabled={busy} onClick={startPair}>
                    <Icon name="refresh" size={14} /> {tx('regenerate')}
                  </button>
                  <button className="btn small" onClick={() => setOffer(null)}>
                    {tx('done')}
                  </button>
                </div>
                {!info.online && <div className="attn-text">{tx('theRelayIsNotConnected')}</div>}
              </div>
            </div>
          )}
        </div>
      </div>

      {active && (
        <div className="step">
          <div className="step-no">
            <Icon name="key" size={12} />
          </div>
          <div className="step-body">
            <div className="step-title">{tx('pairedDevices')}</div>
            {clients.length === 0 ? (
              <div className="muted small">{tx('noneYet')}</div>
            ) : (
              <div className="paired-list">
                {clients.map((c) => (
                  <div key={c.id} className="paired-row small">
                    <span className={`dot ${c.online ? 'dot-connected' : ''}`} />
                    <b className="grow">{c.name}</b>
                    <span className="muted">{c.online ? tx('online') : tx('last', { v: ago(c.lastSeen) })}</span>
                    <button className="link-btn small" onClick={() => rename(c)}>
                      {tx('rename')}
                    </button>
                    <button className="link-btn small error-text" onClick={() => revoke(c)}>
                      {tx('remove2')}
                    </button>
                  </div>
                ))}
              </div>
            )}
          </div>
        </div>
      )}

      {active && (
        <details className="relay-more small">
          <summary>{tx('more')}</summary>
          <div className="row wrap">
            <button className="btn small" disabled={busy || !info.online} onClick={shareRelay}>
              {tx('createARelayAccessLink')}
            </button>
            <span className="muted">{tx('singleUseValidFor10')}</span>
          </div>
          <div className="row wrap">
            <button
              className="btn small"
              disabled={busy}
              onClick={async () =>
                (await askConfirm({
                  title: tx('resetAllPairings'),
                  message: tx('allPairedPhonesAndComputers'),
                  okText: tx('reset'),
                  danger: true,
                })) &&
                run(
                  rpc(hostId, 'relay.reset', {}).then(loadClients),
                  tx('resetAllPreviousPairingsNo'),
                )
              }
            >
              {tx('resetAllPairings2')}
            </button>
            <span className="muted">{tx('useIfADeviceMay')}</span>
          </div>
          <div className="muted">{tx('deviceId', { deviceId: info.deviceId ?? '' })}</div>
        </details>
      )}
    </div>
  );
}

function HostForm({ initial, hosts, onSaved, onCancel }: { initial: HostConfig; hosts: HostEntry[]; onSaved: (h: HostConfig) => void; onCancel: () => void }) {
  const [, lang] = useLang();
  const [h, setH] = useState<HostConfig>(initial);
  const [scanning, setScanning] = useState(false);
  const [imports, setImports] = useState<any[] | null>(null);
  const ssh = h.ssh ?? emptySsh();
  const setSsh = (p: Partial<SshTarget>) => setH({ ...h, ssh: { ...ssh, ...p } });
  const [pairInfo, setPairInfo] = useState<LinkInfo | string | null>(null);
  const isNewRelay = h.kind === 'relay' && !h.id;

  // When adding a relay device, if the clipboard holds a pairing link, fill it in (the other computer just clicked "Copy pairing link")
  useEffect(() => {
    if (!isNewRelay) return;
    readClipboard().then((c) => {
      if (c.startsWith('yiconsole://pair?')) {
        setH((x) => (x.pairing ? x : { ...x, pairing: c }));
        toast(tx('pairingLinkReadFromThe'));
      }
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const pairing = h.kind === 'relay' ? (h.pairing ?? '') : '';
  useEffect(() => {
    const l = pairing.trim();
    if (!l || l === '********') return setPairInfo(null);
    hubCall<LinkInfo>('relay.parseLink', { uri: l })
      .then((r) =>
        setPairInfo(
          r.kind === 'pair'
            ? r
            : tx('thisIsARelayAccess'),
        ),
      )
      .catch((e) => setPairInfo(String(e?.message ?? e)));
  }, [pairing, lang]);

  const [saving, setSaving] = useState(false);
  const save = async () => {
    setSaving(true);
    try {
      // A pairing link has to complete pairing through the relay first, which can take a few seconds
      const saved = await hubCall<HostConfig>('hosts.save', { host: h });
      await refreshHosts();
      onSaved(saved);
    } catch (e) {
      errorToast(e);
    } finally {
      setSaving(false);
    }
  };

  const importCfg = async () => {
    try {
      setImports(await hubCall('hosts.importSshConfig'));
    } catch (e) {
      errorToast(e);
    }
  };

  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    ref.current?.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
  }, []);

  return (
    <div className="card" ref={ref}>
      {scanning && (
        <QrScanner
          onClose={() => setScanning(false)}
          onResult={(c) => {
            setScanning(false);
            setH((x) => ({ ...x, pairing: c.trim() }));
          }}
        />
      )}
      <div className="card-title">{h.id ? tx('edit', { name: h.name }) : h.kind === 'ssh' ? tx('addSshHost') : tx('pairADeviceConnectTo')}</div>
      {isNewRelay && (
        <ol className="muted small how-to">
          <li>
            {tx('onTheComputerYouWant')}
            <b>{tx('letOtherDevicesConnectTo2')}</b>
            {tx('underThisComputerThenTurn')}
          </li>
          <li>{tx('inStep2ClickGenerate')}</li>
          <li>{tx('pasteItBelowItConnects')}</li>
        </ol>
      )}
      <div className="form">
        <label>
          {tx('name')}
          <input value={h.name} onChange={(e) => setH({ ...h, name: e.target.value })} placeholder={h.kind === 'relay' ? tx('leaveEmptyToUseThe') : tx('eGDevMachine')} />
        </label>
        {h.kind === 'relay' && (
          <label>
            {tx('pairingLink2')}
            <div className="row">
              <input
                className="grow"
                value={h.pairing ?? ''}
                onChange={(e) => setH({ ...h, pairing: e.target.value })}
                placeholder="yiconsole://pair?..."
              />
              {!isAndroid && (
                <button
                  className="btn small"
                  onClick={async () => {
                    const c = await readClipboard();
                    if (c) setH({ ...h, pairing: c });
                    else toast(tx('clipboardIsEmpty'));
                  }}
                >
                  <Icon name="copy" size={14} /> {tx('paste')}
                </button>
              )}
              {isAndroid && canScanQr() && (
                <button className="btn small" onClick={() => setScanning(true)}>
                  <Icon name="qr" size={14} /> {tx('scan')}
                </button>
              )}
            </div>
            {typeof pairInfo === 'string' && <span className="error-text small">{pairInfo}</span>}
            {pairInfo && typeof pairInfo === 'object' && (
              <span className="ok-text small">
                {tx('willAdd')}
                <b>{pairInfo.name || tx('device')}</b>
                {tx('viaRelay', { v: hostOf(pairInfo.url) })}
              </span>
            )}
            {!pairInfo && (
              <span className="muted small">{tx('onAComputerWithoutA')}</span>
            )}
          </label>
        )}
        {h.kind === 'ssh' && (
          <>
            {!h.id && (
              <button className="link-btn small" onClick={importCfg}>
                {tx('importFromSshConfig')}
              </button>
            )}
            {imports && (
              <div className="import-list">
                {imports.length === 0 && <span className="muted small">{tx('noHostsToImport')}</span>}
                {imports.map((c) => (
                  <button
                    key={c.alias}
                    onClick={() => {
                      const jump = hosts.find((x) => x.config.name === c.proxyJump);
                      setH({
                        ...h,
                        name: c.alias,
                        ssh: {
                          ...ssh,
                          host: c.host,
                          port: c.port,
                          user: c.user || ssh.user,
                          auth: c.identityFile ? 'key' : 'agent',
                          keyPath: c.identityFile,
                          jumpHostId: jump?.config.id ?? '',
                        },
                      });
                      setImports(null);
                    }}
                  >
                    <b>{c.alias}</b> <span className="muted">{c.user ? c.user + '@' : ''}{c.host}:{c.port}</span>
                    {c.proxyJump && <span className="muted"> {tx('via2', { proxyJump: c.proxyJump })}</span>}
                  </button>
                ))}
              </div>
            )}
            <div className="row">
              <label className="grow">
                {tx('address')}
                <input value={ssh.host} onChange={(e) => setSsh({ host: e.target.value })} placeholder="192.168.1.10" />
              </label>
              <label style={{ width: 90 }}>
                {tx('port')}
                <input type="number" value={ssh.port} onChange={(e) => setSsh({ port: Number(e.target.value) || 22 })} />
              </label>
            </div>
            <label>
              {tx('username')}
              <input value={ssh.user} onChange={(e) => setSsh({ user: e.target.value })} placeholder="root" />
            </label>
            <div className="field">
              <span>{tx('authentication')}</span>
              <div className="seg">
                {(
                  [
                    ['agent', tx('sshAgentDefaultKey')],
                    ['key', tx('privateKey')],
                    ['password', tx('password')],
                  ] as const
                ).map(([v, l]) => (
                  <button key={v} className={ssh.auth === v ? 'active' : ''} onClick={() => setSsh({ auth: v })}>
                    {l}
                  </button>
                ))}
              </div>
            </div>
            {ssh.auth === 'key' && (
              <>
                <label>
                  {tx('privateKeyPath')}
                  <input value={ssh.keyPath} onChange={(e) => setSsh({ keyPath: e.target.value })} placeholder="~/.ssh/id_ed25519" />
                </label>
                <label>
                  {tx('orPasteThePrivateKey')}
                  <textarea rows={3} value={ssh.keyText} onChange={(e) => setSsh({ keyText: e.target.value })} placeholder="-----BEGIN OPENSSH PRIVATE KEY-----" />
                </label>
                <label>
                  {tx('keyPassphraseLeaveEmptyIf')}
                  <input type="password" value={ssh.passphrase} onChange={(e) => setSsh({ passphrase: e.target.value })} />
                </label>
              </>
            )}
            {ssh.auth === 'password' && (
              <label>
                {tx('password')}
                <input type="password" value={ssh.password} onChange={(e) => setSsh({ password: e.target.value })} />
              </label>
            )}
            <label>
              {tx('jumpHost')}
              <select value={ssh.jumpHostId} onChange={(e) => setSsh({ jumpHostId: e.target.value })}>
                <option value="">{tx('none')}</option>
                {hosts
                  .filter((x) => x.config.kind === 'ssh' && x.config.id !== h.id)
                  .map((x) => (
                    <option key={x.config.id} value={x.config.id}>
                      {x.config.name}
                    </option>
                  ))}
              </select>
            </label>
            <label className="check">
              <input type="checkbox" checked={h.installService} onChange={(e) => setH({ ...h, installService: e.target.checked })} />
              {tx('onFirstDeployRegisterYi')}
            </label>
          </>
        )}
        <label className="check">
          <input type="checkbox" checked={h.autoConnect} onChange={(e) => setH({ ...h, autoConnect: e.target.checked })} />
          {tx('connectAutomaticallyOnStartup')}
        </label>
        {!isAndroid && (
          <label className="check">
            <input type="checkbox" checked={h.relayShare !== false} onChange={(e) => setH({ ...h, relayShare: e.target.checked })} />
            {tx('letPairedPhonesReachThis')}
          </label>
        )}
        <div className="row end">
          <button className="btn" onClick={onCancel}>
            {tx('cancel')}
          </button>
          <button className="btn primary" disabled={saving || (h.kind === 'relay' && typeof pairInfo === 'string')} onClick={save}>
            {saving && pairInfo && typeof pairInfo === 'object' ? tx('pairing') : isNewRelay ? tx('addAndConnect') : tx('save')}
          </button>
        </div>
      </div>
    </div>
  );
}

function HostCard({ h, onEdit }: { h: HostEntry; onEdit: () => void }) {
  const deploy = useStore((s) => s.deploy[h.config.id]);
  const hosts = useStore((s) => s.hosts);
  const [busy, setBusy] = useState('');
  const hello = h.status.hello;
  const run = async (label: string, f: () => Promise<unknown>) => {
    setBusy(label);
    try {
      await f();
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy('');
      refreshHosts();
    }
  };
  const id = h.config.id;
  const connected = h.status.status === 'connected';
  return (
    <div className="card host-card">
      <div className="row">
        <Icon name={h.config.kind === 'local' ? 'laptop' : h.config.kind === 'relay' ? 'cloud' : 'server'} />
        <div className="grow">
          <div className="card-title">{h.config.name}</div>
          <div className="muted small">
            {h.config.kind === 'ssh' && `${h.config.ssh?.user}@${h.config.ssh?.host}:${h.config.ssh?.port}`}
            {h.config.kind === 'relay' && tx('viaRelay2')}
            {h.config.kind === 'proxy' && tx('forwardedVia', { name: hosts.find((x) => x.config.id === h.config.viaHostId)?.config.name ?? tx('aPairedComputer') })}
            {h.config.kind === 'local' && tx('thisComputer')}
            {hello && ` · ${hello.os}/${hello.arch} · yi-agentd ${hello.daemonVersion}`}
          </div>
        </div>
        <span className={`status-chip ${connected ? 'st-idle' : h.status.status === 'error' ? 'st-error' : 'st-exited'}`}>{statusText(h)}</span>
      </div>
      {h.status.status === 'error' && h.status.code?.startsWith('HOSTKEY') ? (
        <HostKeyPrompt h={h} onDone={() => refreshHosts()} />
      ) : (
        h.status.status === 'error' && <div className="error-text small">{h.status.error?.replace(/^[A-Z_]+:/, '')}</div>
      )}
      {h.status.versionMismatch && (
        <div className="callout callout-warn small">
          {h.config.kind === 'local'
            ? tx('theLocalDaemonIsStill', { v: hello?.daemonVersion ?? '', clientVersion: h.status.clientVersion ?? '' })
            : tx('remoteYiAgentdDoesNot', { v: hello?.daemonVersion ?? '', clientVersion: h.status.clientVersion ?? '' })}
          {h.config.kind === 'relay' || h.config.kind === 'proxy' ? (
            <span className="muted">{tx('updateYiConsoleOnThe')}</span>
          ) : (
            <button className="btn small" disabled={!!busy} onClick={() => run('upgrade', () => hubCall('hosts.upgrade', { id }))}>
              {h.config.kind === 'local' ? tx('restartDaemon') : tx('upgradeYiAgentd')}
            </button>
          )}
          <span className="muted">{tx('theTurnInProgressWill')}</span>
        </div>
      )}
      {deploy && deploy.length > 0 && (
        <div className="deploy-log small muted">
          {deploy.slice(-4).map((m, i) => (
            <div key={i}>{m}</div>
          ))}
        </div>
      )}
      <div className="row wrap">
        {connected ? (
          <button className="btn small" disabled={!!busy} onClick={() => run('dis', () => hubCall('hosts.disconnect', { id }))}>
            {tx('disconnect')}
          </button>
        ) : (
          <button className="btn primary small" disabled={!!busy} onClick={() => run('con', () => hubCall('hosts.connect', { id }))}>
            {busy === 'con' ? tx('connecting') : tx('connect')}
          </button>
        )}
        {h.config.kind !== 'local' && h.config.kind !== 'proxy' && (
          <button className="btn small" onClick={onEdit}>
            {tx('edit2')}
          </button>
        )}
        {connected && h.config.kind !== 'relay' && h.config.kind !== 'proxy' && (
          <button
            className="btn small"
            disabled={!!busy}
            onClick={() => run('svc', async () => toast(String(await hubCall('hosts.installService', { id }))))}
          >
            {tx('registerStartupService')}
          </button>
        )}
        {connected && (
          <button className="btn small" onClick={() => navigate({ kind: 'settings', hostId: id })}>
            {tx('settings')}
          </button>
        )}
        {h.config.kind !== 'local' && (
          <button
            className="btn small danger-ghost"
            onClick={async () =>
              (await askConfirm({ title: tx('deleteHost', { name: h.config.name }), okText: tx('delete'), danger: true })) &&
              run('del', () => hubCall('hosts.delete', { id }))
            }
          >
            {tx('delete')}
          </button>
        )}
      </div>
      {h.config.kind === 'local' && connected && <RelayPanel hostId={id} />}
      {hello && connected && <HostTools hostId={id} tools={hello.tools} />}
    </div>
  );
}

export function HostsView({ editId }: { editId?: string }) {
  const hosts = useStore((s) => s.hosts);
  const [editing, setEditing] = useState<HostConfig | null>(null);
  const localOk = hosts.some((h) => h.config.kind === 'local' && h.status.status === 'connected');
  useEffect(() => {
    refreshHosts().catch(errorToast);
  }, []);
  useEffect(() => {
    if (editId) {
      // If a host key needs confirming, handle it on the card; other errors (auth failed, private key unreadable, etc.) open the editor directly
      const h = hosts.find((x) => x.config.id === editId);
      if (h && !h.status.code?.startsWith('HOSTKEY')) setEditing(h.config);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [editId]);

  // The edit form replaces the host's card in place; new ones go at the top of the list
  const form = (initial: HostConfig) => (
    <HostForm
      key={initial.id || 'new'}
      initial={initial}
      hosts={hosts}
      onCancel={() => setEditing(null)}
      onSaved={async (h) => {
        setEditing(null);
        try {
          await hubCall('hosts.connect', { id: h.id });
        } catch {
          /* the error is shown on the card */
        }
        refreshHosts();
      }}
    />
  );

  return (
    <div className="page">
      <div className="page-head">
        <h2>{tx('hosts')}</h2>
        <div className="row">
          <button className="btn" onClick={() => setEditing(newHost('ssh'))}>
            <Icon name="plus" size={14} /> {tx('sshHost')}
          </button>
          <button className="btn" onClick={() => setEditing(newHost('relay'))} title={tx('addWithAPairingLink')}>
            <Icon name="cloud" size={14} /> {tx('pairDevice')}
          </button>
          {localOk && (
            <button
              className="btn"
              title={tx('turnOnTheRelayAnd')}
              onClick={() => document.getElementById('relay-panel')?.scrollIntoView({ behavior: 'smooth', block: 'start' })}
            >
              <Icon name="qr" size={14} /> {tx('makeThisComputerReachable')}
            </button>
          )}
        </div>
      </div>
      {editing && !editing.id && form(editing)}
      <div className="cards">
        {hosts.map((h) =>
          editing?.id === h.config.id ? (
            form(editing)
          ) : (
            <HostCard key={h.config.id} h={h} onEdit={() => setEditing(h.config)} />
          ),
        )}
        <button className="link-btn small" onClick={() => navigate({ kind: 'prefs' })}>
          {tx('proxyNotificationsAndOtherSettings')}
        </button>
      </div>
    </div>
  );
}
