// Bot connectors page (desktop app only): installed connectors and their instances. A trigger or notify instance is a
// configured copy of a connector (one schedule, one session watcher...); Bots pick instances in their editor.
// Connectors other than the built-in ones are WASM packages (.yiconnector) installed here after a permission review.

import { useContext, useEffect, useState } from 'react';
import { getT, isEn, useLang } from '../i18n';
import { errorToast, toast } from '../store';
import { SourceHosts, useSourceHosts } from './BotsSection';
import { useBotCall, useBotList, useBotSource, type OutboxRow, type ConnectorInstance, type ConnectorManifest, type SchemaProp } from '../bots';
import { askConfirm, openMenu, type MenuItem } from './Overlay';
import { Icon } from './Icon';

const t = getT('bots');

type Health = Record<string, { state: string; detail: string; at?: number }>;

type LabelMap = Record<string, string>;
export type LocalProp = SchemaProp & {
  'x-en'?: { title?: string; description?: string; 'x-labels'?: LabelMap };
  /** Either per-language arrays in enum order (built-ins) or a value → label map (zh), with x-en.x-labels in English */
  'x-labels'?: { zh: string[]; en: string[] } | LabelMap;
};

const tr = (m: { zh: string; en: string }) => (isEn() ? m.en || m.zh : m.zh || m.en);

function propTitle(k: string, p: LocalProp) {
  return (isEn() ? p['x-en']?.title : undefined) ?? p.title ?? k;
}

function propHint(p: LocalProp) {
  return (isEn() ? p['x-en']?.description : undefined) ?? p.description;
}

function optionLabel(p: LocalProp, opt: string, i: number) {
  const l = p['x-labels'];
  if (l && Array.isArray((l as { zh?: unknown }).zh)) {
    const arrays = l as { zh: string[]; en: string[] };
    return arrays[isEn() ? 'en' : 'zh']?.[i] ?? opt;
  }
  if (isEn()) return p['x-en']?.['x-labels']?.[opt] ?? opt;
  return (l as LabelMap | undefined)?.[opt] ?? opt;
}

export function Field({ k, p, value, onChange }: { k: string; p: LocalProp; value: unknown; onChange: (v: unknown) => void }) {
  const hosts = useContext(SourceHosts);
  const title = propTitle(k, p);
  const hint = propHint(p);
  let input: React.ReactNode;
  if (p.type === 'boolean') {
    return (
      <label className="check">
        <input type="checkbox" checked={!!value} onChange={(e) => onChange(e.target.checked)} />
        {title}
      </label>
    );
  } else if (p.type === 'array' && p.items?.enum) {
    const cur = Array.isArray(value) ? (value as string[]) : ((p.default as string[]) ?? []);
    input = (
      <div className="row wrap">
        {p.items.enum.map((opt, i) => (
          <label className="check" key={opt}>
            <input
              type="checkbox"
              checked={cur.includes(opt)}
              onChange={(e) => onChange(e.target.checked ? [...cur, opt] : cur.filter((x) => x !== opt))}
            />
            {optionLabel(p, opt, i)}
          </label>
        ))}
      </div>
    );
  } else if (p.type === 'array') {
    // A list of strings, one per line
    const cur = Array.isArray(value) ? (value as string[]).join('\n') : '';
    input = (
      <textarea
        rows={3}
        className="mono"
        value={cur}
        onChange={(e) =>
          onChange(
            e.target.value
              .split('\n')
              .map((x) => x.trim())
              .filter(Boolean),
          )
        }
      />
    );
  } else if (p.enum) {
    input = (
      <select value={String(value ?? p.default ?? '')} onChange={(e) => onChange(e.target.value)}>
        {p.enum.map((o, i) => (
          <option key={o} value={o}>
            {optionLabel(p, o, i)}
          </option>
        ))}
      </select>
    );
  } else if (p['x-ui'] === 'host') {
    input = (
      <select value={String(value ?? '')} onChange={(e) => onChange(e.target.value || undefined)}>
        <option value="">{t('allHosts')}</option>
        {hosts.map((h) => (
          <option key={h.config.id} value={h.config.id}>
            {h.config.name}
          </option>
        ))}
      </select>
    );
  } else if (p['x-ui'] === 'textarea') {
    input = <textarea rows={4} value={String(value ?? '')} onChange={(e) => onChange(e.target.value)} />;
  } else if (p.type === 'number' || p.type === 'integer') {
    input = (
      <input
        type="number"
        step={p.type === 'integer' ? 1 : undefined}
        value={value === undefined || value === null ? '' : String(value)}
        onChange={(e) => {
          if (e.target.value === '') return onChange(undefined);
          const n = Number(e.target.value);
          onChange(p.type === 'integer' ? Math.round(n) : n);
        }}
      />
    );
  } else {
    input = (
      <input
        type={p['x-secret'] ? 'password' : 'text'}
        className={p['x-ui'] === 'cron' ? 'mono' : undefined}
        value={String(value ?? '')}
        onChange={(e) => onChange(e.target.value)}
      />
    );
  }
  return (
    <label>
      {title}
      {input}
      {hint && <span className="muted small">{hint}</span>}
    </label>
  );
}

function InstanceEditor({
  inst,
  manifest,
  health,
  onDone,
}: {
  inst: ConnectorInstance;
  manifest: ConnectorManifest;
  health?: Health[string];
  onDone: () => void;
}) {
  const botCall = useBotCall();
  const [v, setV] = useState(inst);
  const [saving, setSaving] = useState(false);
  const props = (manifest.config.properties ?? {}) as Record<string, LocalProp>;
  const save = async () => {
    setSaving(true);
    try {
      await botCall('connectors.saveInstance', { instance: v });
      toast(t('saved'));
      onDone();
    } catch (e) {
      errorToast(e);
    } finally {
      setSaving(false);
    }
  };
  const remove = async () => {
    if (!(await askConfirm({ title: t('deleteInstanceTitle', { name: inst.name }), danger: true }))) return;
    try {
      await botCall('connectors.deleteInstance', { id: inst.id });
      onDone();
    } catch (e) {
      errorToast(e);
    }
  };
  return (
    <div className="card">
      <div className="card-title">
        {inst.id ? inst.name : t('newInstance', { plugin: tr(manifest.name) })}
        {health && health.state !== 'ok' && <span className="pill pill-err" title={health.detail}>{health.detail || health.state}</span>}
      </div>
      <div className="form">
        <label>
          {t('instanceName')}
          <input value={v.name} placeholder={tr(manifest.name)} onChange={(e) => setV({ ...v, name: e.target.value })} />
        </label>
        <label className="check">
          <input type="checkbox" checked={v.enabled} onChange={(e) => setV({ ...v, enabled: e.target.checked })} />
          {t('instanceEnabled')}
        </label>
        {Object.entries(props).map(([k, p]) => (
          <Field key={k} k={k} p={p} value={v.config[k]} onChange={(x) => setV({ ...v, config: { ...v.config, [k]: x } })} />
        ))}
        <div className="row">
          <button className="btn primary" disabled={saving} onClick={save}>
            {t('save')}
          </button>
          <button className="btn" onClick={onDone}>
            {t('cancel')}
          </button>
          {inst.id && (
            <button className="btn danger-ghost" onClick={remove}>
              {t('delete')}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

function capLabel(c: string) {
  const labels: Record<string, string> = {
    trigger: t('capTrigger'),
    notify: t('capNotify'),
    reply: t('capReply'),
    context: t('capContext'),
    tools: t('capTools'),
  };
  return labels[c] ?? c;
}

const fmtTime = (ms: number) => new Date(ms).toLocaleString();

/** Recent outgoing messages of one instance */
function Outbox({ instanceId }: { instanceId: string }) {
  const botCall = useBotCall();
  const [rows, setRows] = useState<OutboxRow[] | null>(null);
  useEffect(() => {
    botCall<OutboxRow[]>('connectors.outbox', { instanceId, limit: 30 }).then(setRows).catch(errorToast);
  }, [instanceId]);
  if (!rows) return <div className="muted small">{t('loading')}</div>;
  if (!rows.length) return <div className="muted small">{t('outboxEmpty')}</div>;
  const stateText: Record<string, string> = {
    pending: t('outboxPending'),
    sent: t('outboxSent'),
    gone: t('outboxGone'),
    failed: t('outboxFailed'),
  };
  return (
    <div className="plugin-sub">
      {rows.map((r) => (
        <div key={r.key} className="bot-event" title={r.error ?? undefined}>
          <span className="muted">{fmtTime(r.createdAt)}</span>
          <span className={`pill${r.state === 'failed' ? ' pill-err' : ''}`}>{stateText[r.state] ?? r.state}</span>
          <span className="bot-event-text">
            {r.kind === 'notify' ? '🔔 ' : ''}
            {r.text}
          </span>
          {r.state === 'pending' && r.attempts > 0 && <span className="muted small">{t('outboxRetry', { n: r.attempts })}</span>}
        </div>
      ))}
    </div>
  );
}

interface ToolInfo {
  name: string;
  description: string;
  readOnly: boolean;
}

function Tools({ instanceId }: { instanceId: string }) {
  const botCall = useBotCall();
  const [tools, setTools] = useState<ToolInfo[] | null>(null);
  useEffect(() => {
    botCall<ToolInfo[]>('connectors.tools', { instanceId }).then(setTools).catch(errorToast);
  }, [instanceId]);
  if (!tools) return <div className="muted small">{t('loading')}</div>;
  if (!tools.length) return <div className="muted small">{t('noTools')}</div>;
  return (
    <div className="plugin-sub">
      {tools.map((x) => (
        <div key={x.name} className="bot-event">
          <span className="mono">{x.name}</span>
          <span className="bot-event-text muted">{x.description}</span>
          <span className="pill">{x.readOnly ? t('toolReadOnly') : t('toolWrites')}</span>
        </div>
      ))}
    </div>
  );
}

interface Inspected {
  token: string;
  manifest: ConnectorManifest;
  installed: ConnectorManifest | null;
  added: { net: string[]; exec: string[] };
}

/** The permission review before a package is written */
function Review({ r, onDone }: { r: Inspected; onDone: (installed: boolean) => void }) {
  const botCall = useBotCall();
  const m = r.manifest;
  const [busy, setBusy] = useState(false);
  const install = async () => {
    setBusy(true);
    try {
      await botCall('connectors.install', { token: r.token });
      toast(t('pluginInstalled', { name: tr(m.name) }));
      onDone(true);
    } catch (e) {
      errorToast(e);
      setBusy(false);
    }
  };
  const isNew = (kind: 'net' | 'exec', v: string) => r.added[kind].includes(v);
  return (
    <div className="card plugin-review">
      <div className="card-title">
        <Icon name="shield" size={14} />
        {r.installed ? t('reviewUpdate', { name: tr(m.name), from: r.installed.version, to: m.version }) : t('reviewInstall', { name: tr(m.name), version: m.version })}
      </div>
      <p className="muted small">{tr(m.description)}</p>
      <div className="form">
        <div>
          <div className="small">{t('permNet')}</div>
          {m.permissions.net.length ? (
            <div className="row wrap">
              {m.permissions.net.map((h) => (
                <span key={h} className={`pill mono${isNew('net', h) && r.installed ? ' pill-warn' : ''}`}>
                  {h}
                  {isNew('net', h) && r.installed ? ` · ${t('permNew')}` : ''}
                </span>
              ))}
            </div>
          ) : (
            <div className="muted small">{t('permNone')}</div>
          )}
        </div>
        <div>
          <div className="small">{t('permExec')}</div>
          {m.permissions.exec.length ? (
            <>
              <div className="row wrap">
                {m.permissions.exec.map((c) => (
                  <span key={c} className="pill pill-err mono">
                    {c}
                    {isNew('exec', c) && r.installed ? ` · ${t('permNew')}` : ''}
                  </span>
                ))}
              </div>
              <div className="small plugin-danger">{t('permExecWarn')}</div>
            </>
          ) : (
            <div className="muted small">{t('permNone')}</div>
          )}
        </div>
        <div className="muted small">{t('permSandbox')}</div>
        <div className="row">
          <button className="btn primary" disabled={busy} onClick={install}>
            {r.installed ? t('update') : t('install')}
          </button>
          <button className="btn" disabled={busy} onClick={() => onDone(false)}>
            {t('cancel')}
          </button>
        </div>
      </div>
    </div>
  );
}

function readBase64(f: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(String(r.result).replace(/^data:[^,]*,/, ''));
    r.onerror = () => reject(r.error);
    r.readAsDataURL(f);
  });
}

/** Pick a package (file / URL / path on this computer) and inspect it */
function InstallPanel({ onInspected, onClose }: { onInspected: (r: Inspected) => void; onClose: () => void }) {
  const botCall = useBotCall();
  const [source, setSource] = useState('');
  const [busy, setBusy] = useState(false);
  const inspect = async (args: Record<string, unknown>) => {
    setBusy(true);
    try {
      onInspected(await botCall<Inspected>('connectors.inspect', args));
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };
  const fromText = () => {
    const s = source.trim();
    if (!s) return;
    inspect(/^https?:\/\//i.test(s) ? { url: s } : { path: s });
  };
  return (
    <div className="card">
      <div className="card-title">{t('installPlugin')}</div>
      <div className="form">
        <label>
          {t('installFile')}
          <input
            type="file"
            accept=".yiconnector,.yiplugin,.zip"
            disabled={busy}
            onChange={async (e) => {
              const f = e.target.files?.[0];
              e.target.value = '';
              if (f) inspect({ data: await readBase64(f) });
            }}
          />
        </label>
        <label>
          {t('installSource')}
          <input value={source} placeholder={t('installSourcePlaceholder')} onChange={(e) => setSource(e.target.value)} onKeyDown={(e) => e.key === 'Enter' && fromText()} />
        </label>
        <div className="row">
          <button className="btn primary" disabled={busy || !source.trim()} onClick={fromText}>
            {t('installNext')}
          </button>
          <button className="btn" onClick={onClose}>
            {t('cancel')}
          </button>
        </div>
      </div>
    </div>
  );
}

export function ConnectorsView({ review: reviewToken }: { review?: string } = {}) {
  useLang();
  const src = useBotSource();
  const botCall = useBotCall();
  const hosts = useSourceHosts(src);
  const available = useBotList(src) !== null;
  const [manifests, setManifests] = useState<ConnectorManifest[]>([]);
  const [health, setHealth] = useState<Health>({});
  const [disabled, setDisabled] = useState<string[]>([]);
  const [wasm, setWasm] = useState(true);
  const [instances, setInstances] = useState<ConnectorInstance[]>([]);
  const [editing, setEditing] = useState<ConnectorInstance | null>(null);
  const [installing, setInstalling] = useState(false);
  const [review, setReview] = useState<Inspected | null>(null);
  useEffect(() => {
    if (reviewToken) botCall<Inspected>('connectors.staged', { token: reviewToken }).then(setReview).catch(errorToast);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reviewToken]);
  /** instance id → which detail is open */
  const [detail, setDetail] = useState<{ id: string; kind: 'outbox' | 'tools' } | null>(null);
  const load = () => {
    botCall<{ connectors: ConnectorManifest[]; health: Health; disabled: string[]; wasm: boolean }>('connectors.list')
      .then((r) => {
        setManifests(r.connectors);
        setHealth(r.health);
        setDisabled(r.disabled ?? []);
        setWasm(r.wasm !== false);
      })
      .catch(errorToast);
    botCall<ConnectorInstance[]>('connectors.instances').then(setInstances).catch(errorToast);
  };
  useEffect(() => {
    if (available) load();
  }, [available, src]);

  if (!available) return <div className="page muted">{t('desktopOnly')}</div>;

  const startNew = (m: ConnectorManifest) => {
    const config: Record<string, unknown> = {};
    for (const [k, p] of Object.entries(m.config.properties ?? {})) if (p.default !== undefined) config[k] = p.default;
    setEditing({ id: '', connectorId: m.id, name: '', config, enabled: true });
  };

  const setEnabled = async (m: ConnectorManifest, enabled: boolean) => {
    try {
      await botCall('connectors.setEnabled', { id: m.id, enabled });
      load();
    } catch (e) {
      errorToast(e);
    }
  };

  const uninstall = async (m: ConnectorManifest, keepData: boolean) => {
    const ok = await askConfirm({
      title: t('uninstallTitle', { name: tr(m.name) }),
      message: keepData ? t('uninstallKeepMessage') : t('uninstallPurgeMessage'),
      okText: t('uninstall'),
      danger: true,
    });
    if (!ok) return;
    try {
      await botCall('connectors.uninstall', { id: m.id, keepData });
      load();
    } catch (e) {
      errorToast(e);
    }
  };

  const connectorMenu = (e: React.MouseEvent, m: ConnectorManifest) => {
    const off = disabled.includes(m.id);
    const items: (MenuItem | '-')[] = [{ label: off ? t('enable') : t('disable'), icon: 'power', run: () => setEnabled(m, off) }];
    if (m.runtime === 'wasm' && !m.bundled) {
      items.push('-', { label: t('uninstallKeep'), icon: 'trash', run: () => uninstall(m, true) });
      items.push({ label: t('uninstallPurge'), icon: 'trash', danger: true, run: () => uninstall(m, false) });
    }
    openMenu(e, items);
  };

  const toggleDetail = (id: string, kind: 'outbox' | 'tools') => setDetail(detail?.id === id && detail.kind === kind ? null : { id, kind });

  return (
    <SourceHosts.Provider value={hosts}>
    <div className="page">
      <div className="page-head">
        <h2>{t('plugins')}</h2>
        {wasm && !installing && !review && (
          <button className="btn small" onClick={() => setInstalling(true)}>
            <Icon name="download" size={13} /> {t('installPlugin')}
          </button>
        )}
      </div>
      <p className="muted small">{t('pluginsHint')}</p>
      {!wasm && <p className="muted small">{t('wasmUnavailable')}</p>}
      {installing && !review && (
        <InstallPanel
          onClose={() => setInstalling(false)}
          onInspected={(r) => {
            setReview(r);
            setInstalling(false);
          }}
        />
      )}
      {review && (
        <Review
          r={review}
          onDone={(done) => {
            setReview(null);
            if (done) load();
          }}
        />
      )}
      {manifests.map((m) => {
        const list = instances.filter((i) => i.connectorId === m.id);
        const off = disabled.includes(m.id);
        return (
          <div className={`card${off ? ' plugin-off' : ''}`} key={m.id}>
            <div className="card-title">
              <Icon name="plug" size={14} />
              {tr(m.name)}
              {m.runtime === 'wasm' && <span className="muted small">v{m.version}</span>}
              {m.bundled && <span className="pill">{t('bundledPlugin')}</span>}
              <span className="muted small">
                {m.capabilities.map((c) => capLabel(c)).join(' · ')}
              </span>
              {!m.singleton && list.length > 0 && <span className="pill">{t('instanceCount', { n: list.length })}</span>}
              {off && <span className="pill">{t('pluginDisabled')}</span>}
              <span className="plugin-add row">
                {!m.singleton && !off && (
                  <button className="btn small" onClick={() => startNew(m)}>
                    <Icon name="plus" size={13} /> {t('addInstance')}
                  </button>
                )}
                {!m.singleton && (
                  <button className="icon-btn" title={t('more')} onClick={(e) => connectorMenu(e, m)}>
                    <Icon name="more" size={15} />
                  </button>
                )}
              </span>
            </div>
            <p className="muted small">{tr(m.description)}</p>
            {m.runtime === 'wasm' && (m.permissions.net.length > 0 || m.permissions.exec.length > 0) && (
              <p className="muted small">
                {m.permissions.net.length > 0 && t('permNetList', { hosts: m.permissions.net.join(', ') })}
                {m.permissions.exec.length > 0 && <span className="plugin-danger"> {t('permExecList', { cmds: m.permissions.exec.join(', ') })}</span>}
              </p>
            )}
            {!m.singleton &&
              list.map((i) =>
                editing?.id === i.id ? null : (
                  <div key={i.id}>
                    <div className="plugin-inst">
                      <span className={`sdot sdot-${health[i.id]?.state === 'error' ? 'err' : i.enabled && !off ? 'ok' : 'off'}`} />
                      <span className="truncate">{i.name}</span>
                      {health[i.id]?.state === 'error' && <span className="muted small truncate">{health[i.id].detail}</span>}
                      <span className="plugin-inst-actions">
                        {(m.capabilities.includes('reply') || m.capabilities.includes('notify')) && (
                          <button className="link-btn small" onClick={() => toggleDetail(i.id, 'outbox')}>
                            {t('outbox')}
                          </button>
                        )}
                        {m.capabilities.includes('tools') && (
                          <button className="link-btn small" onClick={() => toggleDetail(i.id, 'tools')}>
                            {t('tools')}
                          </button>
                        )}
                        <button className="link-btn small" onClick={() => setEditing(i)}>
                          {t('edit')}
                        </button>
                      </span>
                    </div>
                    {detail?.id === i.id && (detail.kind === 'outbox' ? <Outbox instanceId={i.id} /> : <Tools instanceId={i.id} />)}
                  </div>
                ),
              )}
            {editing && editing.connectorId === m.id && (
              <InstanceEditor
                key={editing.id || 'new'}
                inst={editing}
                manifest={m}
                health={health[editing.id]}
                onDone={() => {
                  setEditing(null);
                  load();
                }}
              />
            )}
          </div>
        );
      })}
    </div>
    </SourceHosts.Provider>
  );
}
