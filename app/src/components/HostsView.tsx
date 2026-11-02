import QRCode from 'qrcode';
import { useEffect, useRef, useState } from 'react';
import { errorToast, navigate, refreshHosts, toast, useStore, type HostEntry } from '../store';
import { hubCall, rpc } from '../transport';
import type { HostConfig, SshTarget } from '../types';
import { readClipboard, writeClipboard } from '../clipboard';
import { tr, useLang } from '../i18n';
import { askConfirm, askText } from './Overlay';
import { Icon } from './Icon';
import { QrScanner, canScanQr } from './QrScanner';

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
  return s === 'connected' ? tr('已连接', 'Connected') : s === 'connecting' ? tr('连接中…', 'Connecting…') : s === 'error' ? tr('连接失败', 'Connection failed') : tr('未连接', 'Not connected');
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
        <Icon name="key" size={16} /> {changed ? tr('主机密钥已变化！', 'Host key changed!') : tr('首次连接，请确认主机指纹', 'First connection. Confirm the host fingerprint.')}
      </div>
      <div className="mono small">{fp}</div>
      {changed ? (
        <div className="small">
          {tr('可能是服务器重装，也可能是中间人攻击。确认无误后，请先从 known_hosts 中删除旧记录再连接。', 'The server may have been reinstalled, or this may be a man-in-the-middle attack. If you are sure, delete the old entry from known_hosts, then connect.')}
        </div>
      ) : (
        <div className="row">
          <span className="small muted">{tr('请与服务器上 `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` 的输出核对', 'Compare with the output of `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` on the server')}</span>
          <button className="btn primary small" onClick={trust}>
            {tr('信任并连接', 'Trust and connect')}
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
  if (await writeClipboard(text)) toast(tr(`已复制${what}`, `Copied ${what}`));
  else errorToast(tr('复制失败，请手动选中复制', 'Copy failed. Select the text and copy it manually.'));
}

function ago(t: number) {
  if (!t) return tr('从未', 'never');
  const s = Math.max(0, Date.now() / 1000 - t);
  if (s < 60) return tr('刚刚', 'just now');
  if (s < 3600) return tr(`${Math.floor(s / 60)} 分钟前`, `${Math.floor(s / 60)} min ago`);
  if (s < 86400) return tr(`${Math.floor(s / 3600)} 小时前`, `${Math.floor(s / 3600)} h ago`);
  return tr(`${Math.floor(s / 86400)} 天前`, `${Math.floor(s / 86400)} d ago`);
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
            toast(tr(`已配对 ${added?.name ?? ''}`, `Paired ${added?.name ?? ''}`));
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
  const configure = (params: object, ok = tr('中继已开启，正在连接…', 'Relay is on. Connecting…')) =>
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
        title: tr(`移除 ${c.name}？`, `Remove ${c.name}?`),
        message: tr('移除后它会立刻断开，之后要重新扫码配对才能再连进来。', 'It is disconnected right away and has to pair again to reconnect.'),
        okText: tr('移除', 'Remove'),
        danger: true,
      }))
    )
      return;
    try {
      setClients(await rpc<PairedClient[]>(hostId, 'relay.revoke', { id: c.id }));
      toast(tr(`已移除 ${c.name}`, `Removed ${c.name}`));
    } catch (e) {
      errorToast(e);
    }
  };

  const rename = async (c: PairedClient) => {
    const name = await askText({ title: tr('重命名设备', 'Rename device'), value: c.name, okText: tr('保存', 'Save') });
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
      await copy(r.link, tr('中继接入链接（10 分钟内有效，只能用一次）', 'relay access link (valid for 10 minutes, single use)'));
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
        <Icon name="cloud" size={16} /> {tr('让其他设备连接这台电脑', 'Let other devices connect to this computer')}
        <span className="grow" />
        {active && (
          <span className={`pill ${info.online ? 'pill-ok' : info.error ? 'pill-err' : ''}`}>
            {info.online ? tr('可被连接', 'Reachable') : info.error ? tr('中继连接失败', 'Relay connection failed') : tr('正在连接中继…', 'Connecting to relay…')}
          </span>
        )}
      </div>
      <p className="muted small">
        {tr('手机或另一台电脑连不到这台电脑时（不在同一网络、在 NAT 后面），可以经中继服务器连进来。数据端到端加密，中继只转发密文。', 'When a phone or another computer cannot reach this computer (different network, or behind NAT), it can connect through a relay. Traffic is end-to-end encrypted; the relay only forwards ciphertext.')}
      </p>

      <div className="step">
        <div className="step-no">{active && info.online ? <Icon name="check" size={14} /> : '1'}</div>
        <div className="step-body">
          <div className="step-title">{tr('连接中继服务器', 'Connect to a relay')}</div>
          {!setupMode ? (
            <>
              <div className="small">
                {hostOf(info.url)}
                {info.online ? <span className="ok-text">{tr(' · 已连上', ' · Connected')}</span> : !info.error && <span className="muted">{tr(' · 连接中…', ' · Connecting…')}</span>}
              </div>
              {info.error && <div className="error-text small">{info.error}</div>}
              <div className="row">
                <button className="btn small" onClick={() => setChanging(true)}>
                  {info.error ? tr('换一条接入链接', 'Use another access link') : tr('更换中继', 'Change relay')}
                </button>
                <button className="btn small" disabled={busy} onClick={() => run(rpc(hostId, 'relay.configure', { url: info.url, enabled: false }), tr('已关闭，其他设备暂时连不进来', 'Turned off. Other devices cannot connect for now.'))}>
                  {tr('关闭', 'Turn off')}
                </button>
              </div>
            </>
          ) : (
            <>
              {known.length > 0 && (
                <>
                  <div className="muted small">{tr('这个 App 里已经连过中继，让那台电脑代发一个邀请，直接用同一个（中继要开启 --device-invites）：', 'This app already uses a relay. Ask that computer for an invite and use the same relay (the relay must allow --device-invites):')}</div>
                  <div className="row wrap">
                    {known.map((k) => (
                      <button
                        key={k.hostId}
                        className="btn primary small"
                        disabled={busy}
                        onClick={() => run(hubCall('relay.reuse', { hostId, fromHostId: k.hostId }), tr('中继已开启，正在连接…', 'Relay is on. Connecting…'))}
                      >
                        {tr('使用', 'Use')} {hostOf(k.url)}
                        <span className="small" style={{ opacity: 0.7 }}>{tr(`（经 ${k.name}）`, `(via ${k.name})`)}</span>
                      </button>
                    ))}
                  </div>
                </>
              )}
              <div className="muted small">
                {known.length > 0 ? tr('或者粘贴一条', 'Or paste a ') : tr('粘贴一条', 'Paste a ')}
                <b>{tr('中继接入链接', 'relay access link')}</b>
                {tr('（中继运营者在服务器上运行 yi-relay invite 生成，24 小时内有效、只能用一次）：', ' (the relay operator generates it with yi-relay invite on the server; valid for 24 hours, single use):')}
              </div>
              <div className="row">
                <input className="grow" value={link} onChange={(e) => setLink(e.target.value)} placeholder="yiconsole://relay?…" />
                <button className="btn small" onClick={async () => setLink(await readClipboard())}>
                  <Icon name="copy" size={14} /> {tr('粘贴', 'Paste')}
                </button>
              </div>
              {typeof linkInfo === 'string' && <div className="error-text small">{linkInfo}</div>}
              {linkInfo && typeof linkInfo === 'object' && (
                <>
                  <div className="row">
                    <span className="small grow">
                      {tr('中继', 'Relay')} <b>{hostOf(linkInfo.url)}</b>
                      {linkInfo.kind === 'pair' && <span className="muted">{tr(`（取自 ${linkInfo.name} 的配对链接）`, ` (from ${linkInfo.name}’s pairing link)`)}</span>}
                    </span>
                    <button className="btn primary small" disabled={busy} onClick={() => configure({ link: link.trim() })}>
                      {tr('开启', 'Turn on')}
                    </button>
                  </div>
                  {!linkInfo.hasInvite && (
                    <div className="attn-text small">
                      {tr('这条链接里没有邀请码，只有这台电脑之前已经在这个中继上登记过才能用。', 'This link has no invite code. It works only if this computer is already registered with this relay.')}
                    </div>
                  )}
                </>
              )}
              <button className="link-btn small" onClick={() => setManual(!manual)}>
                {manual ? tr('收起手动填写', 'Hide manual entry') : tr('没有链接？手动填写中继地址和邀请码', 'No link? Enter the relay address and invite code')}
              </button>
              {manual && (
                <div className="form">
                  <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder={tr('中继地址，例如 wss://43.110.40.75:7878', 'Relay address, e.g. wss://43.110.40.75:7878')} />
                  <input value={invite} onChange={(e) => setInvite(e.target.value)} placeholder={tr('邀请码（yi-relay invite 生成；已登记过可留空）', 'Invite code (from yi-relay invite; leave empty if already registered)')} />
                  <input value={fp} onChange={(e) => setFp(e.target.value)} placeholder={tr('证书指纹（自签名证书时填写，见 yi-relay 启动日志）', 'Certificate fingerprint (for a self-signed certificate; see the yi-relay startup log)')} />
                  <div className="row">
                    <button className="btn primary small" disabled={!url || busy} onClick={() => configure({ invite })}>
                      {tr('开启', 'Turn on')}
                    </button>
                  </div>
                </div>
              )}
              {changing && (
                <button className="btn small" onClick={() => setChanging(false)}>
                  {tr('取消', 'Cancel')}
                </button>
              )}
            </>
          )}
        </div>
      </div>

      <div className={`step ${active ? '' : 'disabled'}`}>
        <div className="step-no">2</div>
        <div className="step-body">
          <div className="step-title">{tr('在另一台设备上添加这台电脑', 'Add this computer on another device')}</div>
          {!active ? (
            <div className="muted small">{tr('先完成第 1 步。', 'Finish step 1 first.')}</div>
          ) : !offer ? (
            <>
              <div className="muted small">{tr('生成一个一次性配对二维码，10 分钟内有效，只能配对一台设备。', 'Generate a one-time pairing QR code. It is valid for 10 minutes and pairs a single device.')}</div>
              <button className="btn primary small" disabled={busy} onClick={startPair}>
                <Icon name="qr" size={14} /> {tr('生成配对二维码', 'Generate pairing QR code')}
              </button>
            </>
          ) : (
            <div className="pair-row">
              {!expired && <img className="pair-qr" src={offer.qr} alt={tr('配对二维码', 'Pairing QR code')} width={168} height={168} />}
              <div className="pair-help small">
                {expired ? (
                  <div className="attn-text">{tr('二维码已过期。', 'The QR code has expired.')}</div>
                ) : (
                  <>
                    <div>
                      <b>{tr('手机', 'Phone')}</b>
                      {tr('：打开 Yi Console →「主机」→「配对设备」→「扫码」，扫左边的二维码。', ': Open Yi Console → Hosts → Pair device → Scan, and scan the QR code on the left.')}
                    </div>
                    <div>
                      <b>{tr('另一台电脑', 'Another computer')}</b>
                      {tr('：复制配对链接发过去，在 Yi Console「主机」→「配对设备」里粘贴。', ': Copy the pairing link, send it over, and paste it in Yi Console → Hosts → Pair device.')}
                    </div>
                    <div className="muted">
                      {tr(`${Math.floor(left / 60)}:${String(left % 60).padStart(2, '0')} 后过期 · 只能用一次`, `Expires in ${Math.floor(left / 60)}:${String(left % 60).padStart(2, '0')} · single use`)}
                    </div>
                  </>
                )}
                <div className="row wrap">
                  {!expired && (
                    <button className="btn primary small" onClick={() => copy(offer.uri, tr('配对链接', 'pairing link'))}>
                      <Icon name="copy" size={14} /> {tr('复制配对链接', 'Copy pairing link')}
                    </button>
                  )}
                  <button className="btn small" disabled={busy} onClick={startPair}>
                    <Icon name="refresh" size={14} /> {tr('重新生成', 'Regenerate')}
                  </button>
                  <button className="btn small" onClick={() => setOffer(null)}>
                    {tr('完成', 'Done')}
                  </button>
                </div>
                {!info.online && <div className="attn-text">{tr('中继还没连上，对方要等这边连上后才能完成配对。', 'The relay is not connected yet. The other side can pair only after this side connects.')}</div>}
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
            <div className="step-title">{tr('已配对的设备', 'Paired devices')}</div>
            {clients.length === 0 ? (
              <div className="muted small">{tr('还没有。', 'None yet.')}</div>
            ) : (
              <div className="paired-list">
                {clients.map((c) => (
                  <div key={c.id} className="paired-row small">
                    <span className={`dot ${c.online ? 'dot-connected' : ''}`} />
                    <b className="grow">{c.name}</b>
                    <span className="muted">{c.online ? tr('在线', 'online') : tr(`最近 ${ago(c.lastSeen)}`, `last ${ago(c.lastSeen)}`)}</span>
                    <button className="link-btn small" onClick={() => rename(c)}>
                      {tr('改名', 'Rename')}
                    </button>
                    <button className="link-btn small error-text" onClick={() => revoke(c)}>
                      {tr('移除', 'Remove')}
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
          <summary>{tr('更多', 'More')}</summary>
          <div className="row wrap">
            <button className="btn small" disabled={busy || !info.online} onClick={shareRelay}>
              {tr('给另一台电脑生成中继接入链接', 'Create a relay access link for another computer')}
            </button>
            <span className="muted">{tr('一次性、10 分钟有效；中继要开启 --device-invites', 'Single use, valid for 10 minutes; the relay must allow --device-invites')}</span>
          </div>
          <div className="row wrap">
            <button
              className="btn small"
              disabled={busy}
              onClick={async () =>
                (await askConfirm({
                  title: tr('重置所有配对？', 'Reset all pairings?'),
                  message: tr('所有已配对的手机和电脑都会立刻断开，需要重新扫码配对。', 'All paired phones and computers are disconnected right away and must pair again.'),
                  okText: tr('重置', 'Reset'),
                  danger: true,
                })) &&
                run(
                  rpc(hostId, 'relay.reset', {}).then(loadClients),
                  tr('已重置，所有旧配对都失效了', 'Reset. All previous pairings no longer work.'),
                )
              }
            >
              {tr('重置所有配对', 'Reset all pairings')}
            </button>
            <span className="muted">{tr('怀疑有设备泄露时用；单台设备在上面的列表里移除即可', 'Use if a device may be compromised; to remove one device, use the list above')}</span>
          </div>
          <div className="muted">{tr(`设备 ID ${info.deviceId}`, `Device ID ${info.deviceId}`)}</div>
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
        toast(tr('已从剪贴板读取配对链接', 'Pairing link read from the clipboard'));
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
            : tr(
                '这是中继接入链接，是给电脑开启中继用的（在那台电脑「让其他设备连接这台电脑」第 1 步里粘贴）。添加设备要用配对链接。',
                'This is a relay access link for turning the relay on a computer (paste it in step 1 of “Let other devices connect to this computer” on that computer). To add a device, use a pairing link.',
              ),
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
      <div className="card-title">{h.id ? tr(`编辑 ${h.name}`, `Edit ${h.name}`) : h.kind === 'ssh' ? tr('添加 SSH 主机', 'Add SSH host') : tr('配对设备（经中继连接另一台电脑）', 'Pair a device (connect to another computer via relay)')}</div>
      {isNewRelay && (
        <ol className="muted small how-to">
          <li>
            {tr('在要连接的那台电脑上打开 Yi Console →「主机」，找到本机下面的', 'On the computer you want to connect to, open Yi Console → Hosts and find ')}
            <b>{tr('「让其他设备连接这台电脑」', '“Let other devices connect to this computer”')}</b>
            {tr('，按第 1 步开启中继。', ' under this computer, then turn the relay on in step 1.')}
          </li>
          <li>{tr('点第 2 步里的「生成配对二维码」，再点「复制配对链接」发到这里（手机可以直接扫二维码）。二维码 10 分钟内有效，只能用一次。', 'In step 2, click “Generate pairing QR code”, then “Copy pairing link” and send it here (a phone can scan the QR code). The code is valid for 10 minutes and works once.')}</li>
          <li>{tr('粘贴到下面，保存后会自动连接。', 'Paste it below. It connects automatically after you save.')}</li>
        </ol>
      )}
      <div className="form">
        <label>
          {tr('名称', 'Name')}
          <input value={h.name} onChange={(e) => setH({ ...h, name: e.target.value })} placeholder={h.kind === 'relay' ? tr('留空使用设备名', 'Leave empty to use the device name') : tr('例如：开发机', 'e.g. dev machine')} />
        </label>
        {h.kind === 'relay' && (
          <label>
            {tr('配对链接', 'Pairing link')}
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
                    else toast(tr('剪贴板是空的', 'Clipboard is empty'));
                  }}
                >
                  <Icon name="copy" size={14} /> {tr('粘贴', 'Paste')}
                </button>
              )}
              {isAndroid && canScanQr() && (
                <button className="btn small" onClick={() => setScanning(true)}>
                  <Icon name="qr" size={14} /> {tr('扫码', 'Scan')}
                </button>
              )}
            </div>
            {typeof pairInfo === 'string' && <span className="error-text small">{pairInfo}</span>}
            {pairInfo && typeof pairInfo === 'object' && (
              <span className="ok-text small">
                {tr('✓ 将添加 ', '✓ Will add ')}
                <b>{pairInfo.name || tr('设备', 'device')}</b>
                {tr(`，经中继 ${hostOf(pairInfo.url)} 连接`, `, via relay ${hostOf(pairInfo.url)}`)}
              </span>
            )}
            {!pairInfo && (
              <span className="muted small">{tr('没有图形界面的电脑上，可以运行 `yi-agentd relay pair` 打印一次性配对链接和二维码', 'On a computer without a GUI, run `yi-agentd relay pair` to print a one-time pairing link and QR code')}</span>
            )}
          </label>
        )}
        {h.kind === 'ssh' && (
          <>
            {!h.id && (
              <button className="link-btn small" onClick={importCfg}>
                {tr('从 ~/.ssh/config 导入', 'Import from ~/.ssh/config')}
              </button>
            )}
            {imports && (
              <div className="import-list">
                {imports.length === 0 && <span className="muted small">{tr('没有找到可导入的主机', 'No hosts to import')}</span>}
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
                    {c.proxyJump && <span className="muted"> {tr(`经 ${c.proxyJump}`, `via ${c.proxyJump}`)}</span>}
                  </button>
                ))}
              </div>
            )}
            <div className="row">
              <label className="grow">
                {tr('地址', 'Address')}
                <input value={ssh.host} onChange={(e) => setSsh({ host: e.target.value })} placeholder="192.168.1.10" />
              </label>
              <label style={{ width: 90 }}>
                {tr('端口', 'Port')}
                <input type="number" value={ssh.port} onChange={(e) => setSsh({ port: Number(e.target.value) || 22 })} />
              </label>
            </div>
            <label>
              {tr('用户名', 'Username')}
              <input value={ssh.user} onChange={(e) => setSsh({ user: e.target.value })} placeholder="root" />
            </label>
            <div className="field">
              <span>{tr('认证方式', 'Authentication')}</span>
              <div className="seg">
                {(
                  [
                    ['agent', tr('SSH Agent / 默认密钥', 'SSH agent / default key')],
                    ['key', tr('私钥', 'Private key')],
                    ['password', tr('密码', 'Password')],
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
                  {tr('私钥文件路径', 'Private key path')}
                  <input value={ssh.keyPath} onChange={(e) => setSsh({ keyPath: e.target.value })} placeholder="~/.ssh/id_ed25519" />
                </label>
                <label>
                  {tr('或直接粘贴私钥内容（手机上用）', 'Or paste the private key (for phones)')}
                  <textarea rows={3} value={ssh.keyText} onChange={(e) => setSsh({ keyText: e.target.value })} placeholder="-----BEGIN OPENSSH PRIVATE KEY-----" />
                </label>
                <label>
                  {tr('私钥口令（没有可留空）', 'Key passphrase (leave empty if none)')}
                  <input type="password" value={ssh.passphrase} onChange={(e) => setSsh({ passphrase: e.target.value })} />
                </label>
              </>
            )}
            {ssh.auth === 'password' && (
              <label>
                {tr('密码', 'Password')}
                <input type="password" value={ssh.password} onChange={(e) => setSsh({ password: e.target.value })} />
              </label>
            )}
            <label>
              {tr('跳板机', 'Jump host')}
              <select value={ssh.jumpHostId} onChange={(e) => setSsh({ jumpHostId: e.target.value })}>
                <option value="">{tr('不使用', 'None')}</option>
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
              {tr('首次部署时把 yi-agentd 注册为自启服务（重启服务器后会话仍可继续）', 'On first deploy, register yi-agentd as a startup service (sessions continue after a server reboot)')}
            </label>
          </>
        )}
        <label className="check">
          <input type="checkbox" checked={h.autoConnect} onChange={(e) => setH({ ...h, autoConnect: e.target.checked })} />
          {tr('启动时自动连接', 'Connect automatically on startup')}
        </label>
        {!isAndroid && (
          <label className="check">
            <input type="checkbox" checked={h.relayShare !== false} onChange={(e) => setH({ ...h, relayShare: e.target.checked })} />
            {tr('允许已配对的手机经这台电脑访问这台主机', 'Let paired phones reach this host through this computer')}
          </label>
        )}
        <div className="row end">
          <button className="btn" onClick={onCancel}>
            {tr('取消', 'Cancel')}
          </button>
          <button className="btn primary" disabled={saving || (h.kind === 'relay' && typeof pairInfo === 'string')} onClick={save}>
            {saving && pairInfo && typeof pairInfo === 'object' ? tr('正在配对…', 'Pairing…') : isNewRelay ? tr('添加并连接', 'Add and connect') : tr('保存', 'Save')}
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
            {h.config.kind === 'relay' && tr('经中继连接', 'Via relay')}
            {h.config.kind === 'proxy' && tr(`经 ${hosts.find((x) => x.config.id === h.config.viaHostId)?.config.name ?? '已配对电脑'} 转发`, `Forwarded via ${hosts.find((x) => x.config.id === h.config.viaHostId)?.config.name ?? 'a paired computer'}`)}
            {h.config.kind === 'local' && tr('这台电脑', 'This computer')}
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
            ? tr(
                `本机守护进程还在运行旧版本 ${hello?.daemonVersion ?? ''}，新版本 ${h.status.clientVersion} 已就绪，重启守护进程后生效。`,
                `The local daemon is still running old version ${hello?.daemonVersion ?? ''}. Version ${h.status.clientVersion} is ready and takes effect after you restart the daemon.`,
              )
            : tr(
                `远程 yi-agentd 版本 ${hello?.daemonVersion ?? ''} 与客户端 ${h.status.clientVersion} 不一致。`,
                `Remote yi-agentd ${hello?.daemonVersion ?? ''} does not match client ${h.status.clientVersion}.`,
              )}
          {h.config.kind === 'relay' || h.config.kind === 'proxy' ? (
            <span className="muted">{tr('（经中继连接的主机需要在它所在的电脑上更新 Yi Console）', '(Update Yi Console on the computer where this host runs.)')}</span>
          ) : (
            <button className="btn small" disabled={!!busy} onClick={() => run('upgrade', () => hubCall('hosts.upgrade', { id }))}>
              {h.config.kind === 'local' ? tr('重启守护进程', 'Restart daemon') : tr('升级 yi-agentd', 'Upgrade yi-agentd')}
            </button>
          )}
          <span className="muted">{tr('（正在进行的回合会被中断；会话保留，之后发消息会自动接上上下文）', '(The turn in progress will be interrupted. Sessions are kept, and the next message resumes the context.)')}</span>
        </div>
      )}
      {deploy && deploy.length > 0 && (
        <div className="deploy-log small muted">
          {deploy.slice(-4).map((m, i) => (
            <div key={i}>{m}</div>
          ))}
        </div>
      )}
      {hello && (
        <div className="tool-chips">
          {hello.tools
            .filter((t) => t.kind !== 'shell')
            .map((t) => (
              <span key={t.kind} className={`pill ${t.available ? '' : 'pill-off'}`} title={t.path ?? ''}>
                {t.kind} {t.available ? (t.version?.split(' ')[0] ?? '') : tr('未安装', 'Not installed')}
              </span>
            ))}
        </div>
      )}
      <div className="row wrap">
        {connected ? (
          <button className="btn small" disabled={!!busy} onClick={() => run('dis', () => hubCall('hosts.disconnect', { id }))}>
            {tr('断开', 'Disconnect')}
          </button>
        ) : (
          <button className="btn primary small" disabled={!!busy} onClick={() => run('con', () => hubCall('hosts.connect', { id }))}>
            {busy === 'con' ? tr('连接中…', 'Connecting…') : tr('连接', 'Connect')}
          </button>
        )}
        {h.config.kind !== 'local' && h.config.kind !== 'proxy' && (
          <button className="btn small" onClick={onEdit}>
            {tr('编辑', 'Edit')}
          </button>
        )}
        {connected && h.config.kind !== 'relay' && h.config.kind !== 'proxy' && (
          <button
            className="btn small"
            disabled={!!busy}
            onClick={() => run('svc', async () => toast(String(await hubCall('hosts.installService', { id }))))}
          >
            {tr('注册自启服务', 'Register startup service')}
          </button>
        )}
        {connected && (
          <button className="btn small" onClick={() => navigate({ kind: 'settings', hostId: id })}>
            {tr('设置', 'Settings')}
          </button>
        )}
        {h.config.kind !== 'local' && (
          <button
            className="btn small danger-ghost"
            onClick={async () =>
              (await askConfirm({ title: tr(`删除主机 ${h.config.name}？`, `Delete host ${h.config.name}?`), okText: tr('删除', 'Delete'), danger: true })) &&
              run('del', () => hubCall('hosts.delete', { id }))
            }
          >
            {tr('删除', 'Delete')}
          </button>
        )}
      </div>
      {h.config.kind === 'local' && connected && <RelayPanel hostId={id} />}
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
        <h2>{tr('主机管理', 'Hosts')}</h2>
        <div className="row">
          <button className="btn" onClick={() => setEditing(newHost('ssh'))}>
            <Icon name="plus" size={14} /> {tr('SSH 主机', 'SSH host')}
          </button>
          <button className="btn" onClick={() => setEditing(newHost('relay'))} title={tr('用另一台电脑给出的配对链接添加', 'Add with a pairing link from another computer')}>
            <Icon name="cloud" size={14} /> {tr('配对设备', 'Pair device')}
          </button>
          {localOk && (
            <button
              className="btn"
              title={tr('开启中继，生成配对链接给手机或其他电脑', 'Turn on the relay and create a pairing link for a phone or another computer')}
              onClick={() => document.getElementById('relay-panel')?.scrollIntoView({ behavior: 'smooth', block: 'start' })}
            >
              <Icon name="qr" size={14} /> {tr('让这台电脑可被连接', 'Make this computer reachable')}
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
          {tr('网络代理、通知等设置在「系统设置」里', 'Proxy, notifications, and other settings are in Preferences')}
        </button>
      </div>
    </div>
  );
}
