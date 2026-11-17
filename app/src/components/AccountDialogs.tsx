// Account manager sub-dialogs: the add chooser, the sign-in dialog (device code / link / paste code / terminal),
// the API key drawer, import and export. Each tool gets its own options and fields (`TOOL_SPEC`).
import { useEffect, useMemo, useState, type ReactNode } from 'react';
import { getT, trErr, useLang } from '../i18n';
import { errorToast, toast } from '../store';
import { rpc } from '../transport';
import { useBack } from '../back';
import {
  TOOL_LABEL,
  type AccountLoginUpdate,
  type AccountProfileView,
  type ActualAuth,
  type ApiKeyConfig,
  type ToolAccountsSummary,
  type ToolKind,
} from '../types';
import { Icon } from './Icon';

export const tx = getT('accounts');

export async function copyText(text: string) {
  try {
    await navigator.clipboard.writeText(text);
    toast(tx('copied'), 'info');
  } catch {
    /* clipboard blocked: the text stays visible for manual copy */
  }
}

/** How a tool's in-app sign-in works, which decides the login dialog's layout */
export type LoginKind = 'device' | 'link' | 'paste';

/** One entry of the add chooser */
export type AddOption =
  | { kind: 'login'; mode?: string; loginKind: LoginKind; title: string; sub: string; icon: string }
  | { kind: 'terminal'; title: string; sub: string; icon: string }
  | { kind: 'apiKey'; title: string; sub?: string; icon: string }
  | { kind: 'capture'; source?: 'oauth' | 'apiKey'; title: string; sub?: string; icon: string }
  | { kind: 'import'; title: string; sub: string; icon: string };

interface ApiField {
  env?: string;
  label?: string;
  placeholder?: string;
}

/** API key form fields per tool: `main` fields show at once, `adv` under "Advanced" */
interface ApiSpec {
  key: ApiField;
  baseUrl?: { env?: string; adv?: boolean; label?: string; placeholder?: string };
  model?: { env?: string; adv?: boolean };
  authToken?: { env?: string };
  provider?: boolean;
  keyOnly?: boolean;
  /** `tools.accounts.test` can check it */
  test?: boolean;
}

export const API_SPEC: Partial<Record<ToolKind, ApiSpec>> = {
  claude: {
    key: { env: 'ANTHROPIC_API_KEY', placeholder: 'sk-ant-…' },
    baseUrl: { env: 'ANTHROPIC_BASE_URL', placeholder: 'https://api.anthropic.com' },
    model: { env: 'ANTHROPIC_MODEL' },
    authToken: { env: 'ANTHROPIC_AUTH_TOKEN' },
    test: true,
  },
  codex: {
    key: { env: 'OPENAI_API_KEY', placeholder: 'sk-…' },
    baseUrl: { placeholder: 'https://api.openai.com/v1' },
    model: { adv: true },
    provider: true,
    test: true,
  },
  cursor: {
    key: { label: 'CURSOR_API_KEY', placeholder: 'crsr_… / key_…' },
    baseUrl: { env: 'CURSOR_API_ENDPOINT', adv: true, placeholder: 'https://api2.cursor.sh' },
    authToken: { env: 'CURSOR_AUTH_TOKEN' },
  },
  antigravity: {
    key: { env: 'GEMINI_API_KEY', label: 'Gemini API Key', placeholder: 'AIza…' },
    baseUrl: { env: 'GOOGLE_GEMINI_BASE_URL', adv: true, placeholder: 'https://generativelanguage.googleapis.com' },
    model: { env: 'GEMINI_MODEL', adv: true },
    test: true,
  },
  codebuddy: {
    key: { env: 'CODEBUDDY_API_KEY' },
    baseUrl: { env: 'CODEBUDDY_BASE_URL', placeholder: 'https://…' },
    model: { env: 'CODEBUDDY_MODEL', adv: true },
    authToken: { env: 'CODEBUDDY_AUTH_TOKEN' },
  },
  qoder: { key: { env: 'QODER_PERSONAL_ACCESS_TOKEN', label: 'Personal Access Token' }, keyOnly: true },
  kiro: { key: { env: 'KIRO_API_KEY', label: 'KIRO_API_KEY' }, keyOnly: true },
};

/** The sign-in a tool offers in the app (tool, mode) */
export function loginKindOf(tool: ToolKind, mode?: string): LoginKind {
  if (tool === 'kiro' || (tool === 'codex' && mode !== 'interactive')) return 'device';
  if (tool === 'cursor' || tool === 'qoder') return 'link';
  return 'paste';
}

export function loginTitle(tool: ToolKind) {
  switch (tool) {
    case 'claude':
      return tx('loginTitleClaude');
    case 'codex':
      return tx('loginTitleCodex');
    case 'cursor':
      return tx('loginTitleCursor');
    case 'kiro':
      return tx('loginTitleKiro');
    case 'qoder':
      return tx('loginTitleQoder');
    default:
      return tx('loginTitleAgy');
  }
}

export const nativeWho = (s?: ToolAccountsSummary) => s?.nativeStatus?.email || s?.nativeStatus?.plan || '';

/** The add chooser's options for a tool, most useful first */
export function addOptions(tool: ToolKind, summary: ToolAccountsSummary | undefined, isLocal: boolean): AddOption[] {
  const ns = summary?.nativeStatus;
  const who = nativeWho(summary);
  const capture: AddOption = {
    kind: 'capture',
    source: tool === 'cursor' && ns?.authMethod === 'env_api_key' && ns?.hasLocalOauth ? 'oauth' : undefined,
    title: tx('optCapture'),
    sub: who ? tx('optCaptureWho', { who }) : tx('optCaptureSub', { tool: TOOL_LABEL[tool] }),
    icon: 'download',
  };
  const importOpt: AddOption = { kind: 'import', title: tx('optImport'), sub: tx('optImportSub'), icon: 'upload' };
  const out: AddOption[] = [];
  switch (tool) {
    case 'claude':
      out.push({ kind: 'login', loginKind: 'paste', title: tx('optLoginClaude'), sub: tx('optLoginClaudeSub'), icon: 'key' });
      out.push({ kind: 'apiKey', title: tx('optApiKey'), sub: tx('optApiKeyClaudeSub'), icon: 'settings' });
      break;
    case 'codex':
      out.push({ kind: 'login', mode: 'device_auth', loginKind: 'device', title: tx('optLoginCodexDevice'), sub: tx('optLoginCodexDeviceSub'), icon: 'key' });
      // The callback lands on a local port of the host, so only a browser on this computer can reach it
      if (isLocal)
        out.push({ kind: 'login', mode: 'interactive', loginKind: 'paste', title: tx('optLoginCodexBrowser'), sub: tx('optLoginCodexBrowserSub'), icon: 'external' });
      out.push({ kind: 'apiKey', title: tx('optApiKeyCodex'), sub: tx('optApiKeyCodexSub'), icon: 'settings' });
      break;
    case 'cursor':
      out.push({ kind: 'login', loginKind: 'link', title: tx('optLoginCursor'), sub: tx('optLoginCursorSub'), icon: 'key' });
      out.push({ kind: 'apiKey', title: 'CURSOR_API_KEY', sub: tx('optApiKeyCursorSub'), icon: 'settings' });
      break;
    case 'kiro':
      out.push({ kind: 'login', loginKind: 'device', title: tx('optLoginKiro'), sub: tx('optLoginKiroSub'), icon: 'key' });
      out.push({ kind: 'terminal', title: tx('optKiroTerminal'), sub: tx('optKiroTerminalSub'), icon: 'terminal' });
      out.push({ kind: 'apiKey', title: 'KIRO_API_KEY', icon: 'settings' });
      break;
    case 'qoder':
      out.push({ kind: 'login', loginKind: 'link', title: tx('optLoginQoder'), sub: tx('optLoginQoderSub'), icon: 'key' });
      out.push({ kind: 'apiKey', title: tx('optApiKeyQoder'), icon: 'settings' });
      break;
    case 'codebuddy':
      out.push({ kind: 'terminal', title: tx('optCodebuddyTerminal'), sub: tx('optCodebuddyTerminalSub'), icon: 'terminal' });
      out.push({ kind: 'apiKey', title: tx('optApiKey'), icon: 'settings' });
      break;
    case 'antigravity':
      out.push({ kind: 'login', loginKind: 'paste', title: tx('optLoginAgy'), sub: tx('optLoginAgySub'), icon: 'key' });
      out.push({ kind: 'apiKey', title: tx('optApiKeyAgy'), sub: tx('optApiKeyAgySub'), icon: 'settings' });
      break;
  }
  // CodeBuddy's terminal guide already ends in a capture; offer it directly once a login is there
  if (tool !== 'codebuddy' || ns?.loggedIn) out.push(capture);
  if (tool === 'codex' && ns?.actual && ns.hasLocalOauth)
    out.push({ kind: 'capture', source: 'oauth', title: tx('optCaptureCodexOauth'), sub: tx('optCaptureCodexOauthSub', { provider: ns.actual.provider }), icon: 'download' });
  if (tool === 'cursor' && ns?.authMethod === 'env_api_key' && !ns.envApiKeyMatchedId)
    out.push({ kind: 'capture', source: 'apiKey', title: tx('optCursorEnvKey'), sub: tx('optCursorEnvKeySub'), icon: 'download' });
  out.push(importOpt);
  // A login already on the machine but not saved: saving it is the quickest way
  if (ns?.loggedIn && !ns.matchedAccountId) {
    const i = out.indexOf(capture);
    if (i > 0) {
      out.splice(i, 1);
      out.unshift(capture);
    }
  }
  return out;
}

/** A layer inside the account manager (scrim + content); Esc / Android back closes the top one */
export function Layer({ onClose, drawer, children, wide }: { onClose: () => void; drawer?: boolean; wide?: boolean; children: ReactNode }) {
  useBack(true, onClose);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && !e.defaultPrevented) {
        e.preventDefault();
        onClose();
      }
    };
    // Capture phase so the top layer handles Esc before the modal itself
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [onClose]);
  return (
    <div className="acc-layer" onMouseDown={onClose}>
      <div className={drawer ? 'acc-drawer' : `acc-dlg ${wide ? 'wide' : ''}`} role="dialog" onMouseDown={(e) => e.stopPropagation()}>
        {children}
      </div>
    </div>
  );
}

function DlgHead({ title, onClose }: { title: ReactNode; onClose: () => void }) {
  return (
    <div className="acc-dlg-head">
      <h3>{title}</h3>
      <button className="icon-btn" title={tx('close')} onClick={onClose}>
        <Icon name="x" size={16} />
      </button>
    </div>
  );
}

export function AddChooser({
  tool,
  options,
  onPick,
  onClose,
}: {
  tool: ToolKind;
  options: AddOption[];
  onPick: (o: AddOption) => void;
  onClose: () => void;
}) {
  useLang();
  return (
    <Layer onClose={onClose}>
      <DlgHead title={tx('addTitle', { tool: TOOL_LABEL[tool] })} onClose={onClose} />
      <div className="acc-opts">
        {options.map((o, i) => (
          <button key={i} type="button" className={`acc-opt ${i === 0 ? 'rec' : ''}`} onClick={() => onPick(o)}>
            <span className="acc-opt-ic">
              <Icon name={o.icon} size={16} />
            </span>
            <span className="acc-opt-text">
              <b>
                {o.title}
                {i === 0 && <span className="acc-chip dark">{tx('recommended')}</span>}
              </b>
              {o.sub && <span className="acc-sub">{o.sub}</span>}
            </span>
            <Icon name="chevron" size={14} />
          </button>
        ))}
      </div>
    </Layer>
  );
}

function Steps({ items, cur }: { items: string[]; cur: number }) {
  return (
    <div className="acc-steps">
      {items.map((s, i) => (
        <span key={s} className="acc-step-wrap">
          {i > 0 && <span className="acc-step-ln" />}
          <span className={`acc-step ${i < cur ? 'done' : i === cur ? 'cur' : ''}`}>
            <i>{i < cur ? <Icon name="check" size={11} /> : i + 1}</i>
            {s}
          </span>
        </span>
      ))}
    </div>
  );
}

const hostOf = (url: string) => {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
};

function useNow(active: boolean) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!active) return;
    const t = setInterval(() => setNow(Date.now()), 15000);
    return () => clearInterval(t);
  }, [active]);
  return now;
}

function CliOutput({ lines }: { lines: string[] }) {
  const [open, setOpen] = useState(false);
  if (!lines.length) return null;
  return (
    <div className="acc-cli">
      <button type="button" className="acc-fold" onClick={() => setOpen((v) => !v)}>
        <Icon name={open ? 'down' : 'chevron'} size={12} /> {tx('cliOutput', { n: lines.length })}
      </button>
      {open && <pre className="acc-log mono small">{lines.join('\n')}</pre>}
    </div>
  );
}

/** Sign-in dialog: the CLI's sign-in in progress, then naming the new account */
export function LoginDialog({
  hostId,
  tool,
  job,
  loginKind,
  relogin,
  summary,
  isolation,
  onRetry,
  onClose,
  onSaved,
  onReload,
}: {
  hostId: string;
  tool: ToolKind;
  job: AccountLoginUpdate | null;
  loginKind: LoginKind;
  /** Signing in again to this account (not a new one) */
  relogin?: AccountProfileView;
  summary?: ToolAccountsSummary;
  isolation?: string;
  onRetry: () => void;
  onClose: () => void;
  onSaved: () => void;
  /** Reload the account list (after the quota probe filled in the email) */
  onReload: () => void;
}) {
  useLang();
  const [input, setInput] = useState('');
  const [busy, setBusy] = useState(false);
  const [name, setName] = useState<string | null>(null);
  const [makeDefault, setMakeDefault] = useState(false);
  const running = !job || job.running;
  const now = useNow(running && !!job?.expiresAt);
  const acc = job?.ok && job.accountId ? summary?.accounts.find((a) => a.id === job.accountId) : undefined;
  // Qoder / Kiro logins carry no email; the quota probe reads it and renames the placeholder label
  const placeholderLabel = !!acc && !acc.identity.email && /oauth|pending/i.test(acc.label);
  const [probed, setProbed] = useState(false);
  useEffect(() => {
    if (!acc || probed || !placeholderLabel) return;
    setProbed(true);
    void rpc(hostId, 'tools.quota', { tool, accountId: acc.id, refresh: true })
      .catch(() => {})
      .finally(onReload);
  }, [acc, probed, placeholderLabel, hostId, tool, onReload]);
  const shownName = name ?? (acc && !placeholderLabel ? acc.label : '');

  const send = async () => {
    if (!job || !input.trim()) return;
    setBusy(true);
    try {
      await rpc(hostId, 'tools.accounts.loginInput', { jobId: job.jobId, input: input.trim() });
      setInput('');
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };
  const cancel = async () => {
    if (job?.running) await rpc(hostId, 'tools.accounts.loginCancel', { jobId: job.jobId }).catch(() => {});
    onClose();
  };
  const save = async () => {
    if (!acc) return onClose();
    setBusy(true);
    try {
      const label = shownName.trim();
      if (label && label !== acc.label) await rpc(hostId, 'tools.accounts.save', { tool, id: acc.id, label });
      if (makeDefault && !acc.active) await rpc(hostId, 'tools.accounts.activate', { tool, accountId: acc.id });
      toast(tx('savedToast', { label: label || acc.label }), 'info');
      onSaved();
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };
  const discard = async () => {
    if (!acc) return onClose();
    setBusy(true);
    try {
      await rpc(hostId, 'tools.accounts.delete', { tool, accountId: acc.id });
      onSaved();
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const title = relogin ? tx('loginTitleRelogin', { label: relogin.label }) : loginTitle(tool);
  const steps = [tx('stepChoose'), tx('stepAuthorize'), tx('stepSave')];
  const step = job?.ok ? 2 : 1;
  const url = job?.url ?? undefined;
  const minsLeft = job?.expiresAt ? Math.ceil((job.expiresAt - now) / 60000) : null;
  const failed = job && !job.running && !job.ok;
  // Closing while it runs cancels it; after a success the account is kept as it is
  const close = () => (job?.ok ? onClose() : void cancel());

  return (
    <Layer onClose={close}>
      <DlgHead title={title} onClose={close} />
      <Steps items={steps} cur={failed ? 1 : step} />
      {job?.ok ? (
        <>
          <div className="acc-ok-banner">
            <b>
              <Icon name="check" size={14} /> {tx('loggedIn')}
            </b>
            {acc && (acc.identity.email || acc.identity.plan) ? (
              <span>{[acc.identity.email, acc.identity.plan].filter(Boolean).join(' · ')}</span>
            ) : (
              placeholderLabel && (
                <span className="acc-wait">
                  <span className="acc-spin" />
                  {tx('readingEmail')}
                </span>
              )
            )}
          </div>
          <label className="acc-field">
            <span>{tx('nameLabel')}</span>
            <input value={shownName} placeholder={tx('namePlaceholderWait')} onChange={(e) => setName(e.target.value)} autoFocus />
          </label>
          {acc && !acc.active && (
            <label className="acc-switch">
              <input type="checkbox" checked={makeDefault} onChange={(e) => setMakeDefault(e.target.checked)} />
              <i />
              {tx('setAsDefault', { tool: TOOL_LABEL[tool] })}
            </label>
          )}
          <div className="acc-dlg-foot">
            {!relogin && (
              <button className="btn" disabled={busy} onClick={() => void discard()}>
                {tx('dontSave')}
              </button>
            )}
            <button className="btn primary" disabled={busy} onClick={() => void save()}>
              {tx('save')}
            </button>
          </div>
        </>
      ) : failed ? (
        <>
          <div className="acc-err-box">
            <b>{tx('loginFailed')}</b>
            {job.error && <span>{trErr(job.error)}</span>}
          </div>
          <CliOutput lines={job.output} />
          <div className="acc-dlg-foot">
            <button className="btn" onClick={onClose}>
              {tx('close')}
            </button>
            <button className="btn primary" onClick={onRetry}>
              {tx('retry')}
            </button>
          </div>
        </>
      ) : (
        <>
          {loginKind === 'device' && (
            <>
              <div>{tx('deviceIntro')}</div>
              <div className="acc-code-big">{job?.code ?? '····-····'}</div>
              <div className="acc-btn-row">
                {url ? (
                  <a className="btn primary grow" href={url} target="_blank" rel="noopener noreferrer">
                    <Icon name="external" size={14} /> {tx('openHost', { host: hostOf(url) })}
                  </a>
                ) : (
                  <button className="btn primary grow" disabled>
                    {tx('loginStarting')}
                  </button>
                )}
                <button className="btn" disabled={!job?.code} onClick={() => job?.code && void copyText(job.code)}>
                  <Icon name="copy" size={14} /> {tx('copyCode')}
                </button>
                <button className="btn" disabled={!url} onClick={() => url && void copyText(url)}>
                  <Icon name="copy" size={14} /> {tx('copyLink')}
                </button>
              </div>
            </>
          )}
          {loginKind === 'link' && (
            <>
              <div>{tx('linkIntro')}</div>
              <div className="acc-btn-row">
                {url ? (
                  <a className="btn primary grow" href={url} target="_blank" rel="noopener noreferrer">
                    <Icon name="external" size={14} /> {tx('openHost', { host: hostOf(url) })}
                  </a>
                ) : (
                  <button className="btn primary grow" disabled>
                    {tx('loginStarting')}
                  </button>
                )}
                <button className="btn" disabled={!url} onClick={() => url && void copyText(url)}>
                  <Icon name="copy" size={14} /> {tx('copyLink')}
                </button>
              </div>
              <div className="acc-hint">{tx('linkRemoteHint')}</div>
            </>
          )}
          {loginKind === 'paste' && (
            <>
              <ol className="acc-ol">
                <li>{tx('pasteStep1')}</li>
                <li>{tx('pasteStep2')}</li>
              </ol>
              <div className="acc-btn-row">
                {url ? (
                  <a className="btn primary grow" href={url} target="_blank" rel="noopener noreferrer">
                    <Icon name="external" size={14} /> {tx('openHost', { host: hostOf(url) })}
                  </a>
                ) : (
                  <button className="btn primary grow" disabled>
                    {tx('loginStarting')}
                  </button>
                )}
                <button className="btn" disabled={!url} onClick={() => url && void copyText(url)}>
                  <Icon name="copy" size={14} /> {tx('copyLink')}
                </button>
              </div>
            </>
          )}
          {(loginKind === 'paste' || job?.needsInput) && (
            <label className="acc-field">
              <span>{job?.needsInput && job.prompt && loginKind !== 'paste' ? `${tx('cliPrompt')} ${job.prompt}` : tx('pasteLabel')}</span>
              <span className="acc-btn-row">
                <input
                  className="grow"
                  value={input}
                  placeholder={tx('pastePlaceholder')}
                  disabled={!job}
                  onChange={(e) => setInput(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') void send();
                  }}
                />
                <button className="btn primary" disabled={busy || !input.trim() || !job} onClick={() => void send()}>
                  {tx('submit')}
                </button>
              </span>
            </label>
          )}
          {loginKind === 'paste' && <div className="acc-hint">{tx('pasteLocalHint')}</div>}
          <div className="acc-wait">
            <span className="acc-spin" />
            {tx('waitingBrowser')}
            {minsLeft != null && <span>{minsLeft > 0 ? tx('expiresIn', { n: minsLeft }) : tx('expired')}</span>}
          </div>
          {isolation === 'hostActive' && <div className="acc-note">{tx('hostActiveLoginNote', { tool: TOOL_LABEL[tool] })}</div>}
          <CliOutput lines={job?.output ?? []} />
          <div className="acc-dlg-foot">
            <button className="btn" onClick={() => void cancel()}>
              {tx('cancelLogin')}
            </button>
          </div>
        </>
      )}
    </Layer>
  );
}

/** Sign-ins the app can't run (CodeBuddy's /login, Kiro's Google / GitHub / Identity Center): a guide, then capture */
export function TerminalGuide({
  tool,
  summary,
  onRecheck,
  onCapture,
  onClose,
  onOpenTerminal,
}: {
  tool: ToolKind;
  summary?: ToolAccountsSummary;
  onRecheck: () => Promise<void>;
  onCapture: () => void;
  onClose: () => void;
  /** Open a terminal session on the host (leaves the account manager) */
  onOpenTerminal: () => void;
}) {
  useLang();
  const [checking, setChecking] = useState(false);
  const ns = summary?.nativeStatus;
  const matched = ns?.matchedAccountId ? summary?.accounts.find((a) => a.id === ns.matchedAccountId) : undefined;
  const cmd = tool === 'kiro' ? 'kiro-cli login' : 'codebuddy';
  return (
    <Layer onClose={onClose}>
      <DlgHead title={tool === 'kiro' ? tx('terminalTitleKiro') : tx('terminalTitleCodebuddy')} onClose={onClose} />
      <ol className="acc-ol">
        <li>{tx('terminalStep1')}</li>
        <li>
          {tool === 'kiro' ? tx('terminalStep2Kiro') : tx('terminalStep2Codebuddy')}
          <div className="acc-cmd">
            <code>{cmd}</code>
            <button className="icon-btn" title={tx('copy')} onClick={() => void copyText(cmd)}>
              <Icon name="copy" size={14} />
            </button>
          </div>
        </li>
        <li>{tx('terminalStep3')}</li>
      </ol>
      <div className="acc-strip">
        <span className={`acc-dot ${ns?.loggedIn ? '' : 'g'}`} />
        <span className="grow">
          {matched
            ? tx('terminalSaved', { label: matched.label })
            : ns?.loggedIn
              ? tx('terminalDetected', { who: nativeWho(summary) || TOOL_LABEL[tool] })
              : tx('terminalNotDetected', { tool: TOOL_LABEL[tool] })}
        </span>
        <button
          className="btn small ghost"
          disabled={checking}
          onClick={() => {
            setChecking(true);
            void onRecheck().finally(() => setChecking(false));
          }}
        >
          <Icon name="refresh" size={13} /> {tx('recheck')}
        </button>
      </div>
      <div className="acc-note warn">{tool === 'kiro' ? tx('kiroTerminalNote') : tx('codebuddyIdeNote')}</div>
      <div className="acc-dlg-foot">
        <button
          className="btn"
          onClick={onOpenTerminal}
        >
          <Icon name="terminal" size={14} /> {tx('openTerminal')}
        </button>
        <button className="btn primary" disabled={!ns?.loggedIn || !!matched} onClick={onCapture}>
          {tx('optCapture')}
        </button>
      </div>
    </Layer>
  );
}

interface EnvRow {
  k: string;
  v: string;
}

export interface DrawerTarget {
  /** Editing an existing account; absent: a new API key profile */
  acc?: AccountProfileView;
}

/** Create or edit an account: API key profiles get the tool's fields, login accounts only name and failover */
export function AccountDrawer({
  hostId,
  tool,
  target,
  onClose,
  onSaved,
}: {
  hostId: string;
  tool: ToolKind;
  target: DrawerTarget;
  onClose: () => void;
  onSaved: () => void;
}) {
  useLang();
  const acc = target.acc;
  const isApi = !acc || acc.authKind === 'apiKey';
  const spec = API_SPEC[tool] ?? { key: {} };
  const api = acc?.apiConfig;
  const [label, setLabel] = useState(acc?.label ?? '');
  const [apiKey, setApiKey] = useState(api?.apiKey ?? '');
  const [authToken, setAuthToken] = useState(api?.authToken ?? '');
  const [baseUrl, setBaseUrl] = useState(api?.baseUrl ?? '');
  const [model, setModel] = useState(api?.defaultModel ?? '');
  const [provider, setProvider] = useState(api?.providerName ?? '');
  const [codexProvider, setCodexProvider] = useState(!!(api?.providerName || acc?.actual));
  const [env, setEnv] = useState<EnvRow[]>(() => Object.entries(api?.extraEnv ?? {}).map(([k, v]) => ({ k, v })));
  const [allow, setAllow] = useState(acc ? acc.allowAutoFailover : false);
  const [reveal, setReveal] = useState(false);
  const [busy, setBusy] = useState(false);
  const [test, setTest] = useState<{ running?: boolean; ok?: boolean; n?: number; err?: string } | null>(null);
  const [adv, setAdv] = useState(() => !!(api?.authToken || Object.keys(api?.extraEnv ?? {}).length));
  const keyEnv = tool === 'codex' && codexProvider ? acc?.actual?.envKey || undefined : spec.key.env;

  const config = (): ApiKeyConfig => ({
    apiKey: apiKey.trim(),
    authToken: authToken.trim(),
    baseUrl: baseUrl.trim(),
    defaultModel: model.trim(),
    providerName: tool === 'codex' && codexProvider ? provider.trim() : '',
    extraEnv: Object.fromEntries(env.filter((r) => r.k.trim()).map((r) => [r.k.trim(), r.v])),
  });

  const save = async () => {
    setBusy(true);
    try {
      const view = await rpc<AccountProfileView>(hostId, 'tools.accounts.save', {
        tool,
        id: acc?.id,
        label: label.trim() || undefined,
        authKind: isApi ? 'apiKey' : 'oauth',
        allowAutoFailover: allow,
        apiConfig: isApi ? config() : undefined,
      });
      toast(tx('savedToast', { label: view.label }), 'info');
      onSaved();
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };
  const runTest = async () => {
    setTest({ running: true });
    try {
      const r = await rpc<{ ok: boolean; models?: number; error?: string }>(hostId, 'tools.accounts.test', {
        tool,
        apiConfig: config(),
        accountId: acc?.id,
      });
      setTest(r.ok ? { ok: true, n: r.models ?? 0 } : { ok: false, err: r.error });
    } catch (e) {
      setTest({ ok: false, err: String((e as Error)?.message ?? e) });
    }
  };
  const pasteKey = async () => {
    try {
      setApiKey((await navigator.clipboard.readText()).trim());
    } catch {
      /* clipboard read not allowed */
    }
  };

  const kindName = tool === 'cursor' ? 'CURSOR_API_KEY' : spec.key.label ?? 'API Key';
  const title = acc ? tx('drawerEdit', { label: acc.label }) : tx('drawerNew', { kind: kindName });
  const envTag = (e?: string) => (e ? <span className="acc-env">{e}</span> : null);
  const urlField = spec.baseUrl && (
    <label className="acc-field">
      <span>
        {tool === 'cursor' ? tx('fieldEndpoint') : tx('fieldBaseUrl')} {tool !== 'cursor' && <em>{tx('fieldBaseUrlHint')}</em>}
        {envTag(spec.baseUrl.env)}
      </span>
      <input value={baseUrl} placeholder={spec.baseUrl.placeholder} onChange={(e) => setBaseUrl(e.target.value)} />
    </label>
  );
  const modelField = spec.model && (
    <label className="acc-field">
      <span>
        {tx('fieldModel')}
        {envTag(spec.model.env)}
      </span>
      <input value={model} placeholder={tx('fieldModelPlaceholder', { tool: TOOL_LABEL[tool] })} onChange={(e) => setModel(e.target.value)} />
    </label>
  );

  return (
    <Layer onClose={onClose} drawer>
      <div className="acc-drawer-head">
        <b>{title}</b>
        <span className="grow" />
        <button className="icon-btn" title={tx('close')} onClick={onClose}>
          <Icon name="x" size={16} />
        </button>
      </div>
      <div className="acc-drawer-body">
        {isApi && tool === 'cursor' && <div className="acc-hint">{tx('cursorKeyHint')}</div>}
        {isApi && tool === 'cursor' && (
          <KeyInput label={kindName} env={undefined} value={apiKey} placeholder={spec.key.placeholder} reveal={reveal} onReveal={() => setReveal((v) => !v)} onPaste={() => void pasteKey()} onChange={setApiKey} />
        )}
        <label className="acc-field">
          <span>{tool === 'cursor' && isApi ? tx('fieldNameCursor') : tx('fieldName')}</span>
          <input value={label} placeholder={tx('fieldNamePlaceholder')} onChange={(e) => setLabel(e.target.value)} />
        </label>
        {!isApi && <div className="acc-hint">{tx('oauthEditHint')}</div>}
        {isApi && tool !== 'cursor' && (
          <div className="acc-sec">
            <div className="acc-sec-t">{tx('sectionEndpoint')}</div>
            {spec.provider && (
              <div className="acc-field">
                <span>{tx('fieldInterface')}</span>
                <div className="acc-seg">
                  <button type="button" className={!codexProvider ? 'on' : ''} onClick={() => setCodexProvider(false)}>
                    {tx('codexKindOfficial')}
                  </button>
                  <button type="button" className={codexProvider ? 'on' : ''} onClick={() => setCodexProvider(true)}>
                    {tx('codexKindProvider')}
                  </button>
                </div>
              </div>
            )}
            {spec.provider && codexProvider && (
              <label className="acc-field">
                <span>
                  {tx('fieldProvider')}
                  {envTag('config.toml model_provider')}
                </span>
                <input value={provider} placeholder="myapi" onChange={(e) => setProvider(e.target.value)} />
              </label>
            )}
            {spec.provider && urlField}
            <KeyInput
              label={spec.key.label ?? tx('fieldApiKey')}
              env={keyEnv}
              value={apiKey}
              placeholder={spec.key.placeholder}
              reveal={reveal}
              onReveal={() => setReveal((v) => !v)}
              onPaste={() => void pasteKey()}
              onChange={setApiKey}
            />
            {!spec.provider && !spec.baseUrl?.adv && urlField}
            {!spec.model?.adv && modelField}
            {spec.keyOnly && <div className="acc-hint">{tx('keyOnlyHint', { tool: TOOL_LABEL[tool] })}</div>}
            {spec.test && (
              <div className="acc-btn-row">
                <button className="btn small" disabled={test?.running || !apiKey.trim()} onClick={() => void runTest()}>
                  {tx('testConnection')}
                </button>
                {test && (
                  <span className={`acc-hint ${test.ok === false ? 'err' : test.ok ? 'ok' : ''}`}>
                    {test.running ? tx('testing') : test.ok ? tx('testOk', { n: test.n ?? 0 }) : tx('testFail', { err: trErr(test.err ?? '') })}
                  </span>
                )}
              </div>
            )}
          </div>
        )}
        <div className="acc-sec">
          <div className="acc-sec-t">{tx('sectionFailover')}</div>
          <label className="acc-switch">
            <input type="checkbox" checked={allow} onChange={(e) => setAllow(e.target.checked)} />
            <i />
            {tx('allowFailoverLabel')}
          </label>
        </div>
        {isApi && (
          <>
            <button type="button" className="acc-fold strong" onClick={() => setAdv((v) => !v)}>
              <Icon name={adv ? 'down' : 'chevron'} size={12} /> {tx('advanced')}
            </button>
            {adv && (
              <div className="acc-adv">
                {spec.baseUrl?.adv && urlField}
                {spec.model?.adv && modelField}
                {spec.authToken && (
                  <label className="acc-field">
                    <span>
                      {tx('fieldAuthToken')} <em>{tx('fieldAuthTokenHint')}</em>
                      {envTag(spec.authToken.env)}
                    </span>
                    <input value={authToken} type={reveal ? 'text' : 'password'} onChange={(e) => setAuthToken(e.target.value)} />
                  </label>
                )}
                <div className="acc-field">
                  <span>{tx('fieldExtraEnv')}</span>
                  {env.map((r, i) => (
                    <div key={i} className="acc-kv">
                      <input className="mono" value={r.k} placeholder={tx('envName')} onChange={(e) => setEnv(env.map((x, j) => (j === i ? { ...x, k: e.target.value } : x)))} />
                      <input className="mono" value={r.v} placeholder={tx('envValue')} onChange={(e) => setEnv(env.map((x, j) => (j === i ? { ...x, v: e.target.value } : x)))} />
                      <button className="icon-btn" onClick={() => setEnv(env.filter((_, j) => j !== i))}>
                        <Icon name="x" size={14} />
                      </button>
                    </div>
                  ))}
                  <button type="button" className="acc-link" onClick={() => setEnv([...env, { k: '', v: '' }])}>
                    <Icon name="plus" size={12} /> {tx('addEnv')}
                  </button>
                </div>
              </div>
            )}
          </>
        )}
      </div>
      <div className="acc-drawer-foot">
        <span className="acc-hint grow">{tx('drawerFootHint')}</span>
        <button className="btn" onClick={onClose}>
          {tx('cancel')}
        </button>
        <button className="btn primary" disabled={busy || (isApi && !acc && !apiKey.trim())} onClick={() => void save()}>
          {tx('save')}
        </button>
      </div>
    </Layer>
  );
}

function KeyInput({
  label,
  env,
  value,
  placeholder,
  reveal,
  onReveal,
  onPaste,
  onChange,
}: {
  label: string;
  env?: string;
  value: string;
  placeholder?: string;
  reveal: boolean;
  onReveal: () => void;
  onPaste: () => void;
  onChange: (v: string) => void;
}) {
  return (
    <label className="acc-field">
      <span>
        {label}
        {env && env !== label && <span className="acc-env">{env}</span>}
      </span>
      <span className="acc-input-wrap">
        <input className="grow" value={value} type={reveal ? 'text' : 'password'} placeholder={placeholder} autoComplete="off" onChange={(e) => onChange(e.target.value)} />
        <button type="button" className="icon-btn" title={tx('reveal')} onClick={onReveal}>
          <Icon name="eye" size={14} />
        </button>
        <button type="button" className="icon-btn" title={tx('paste')} onClick={onPaste}>
          <Icon name="paste" size={14} />
        </button>
      </span>
    </label>
  );
}

export function ImportDialog({ hostId, onClose, onDone }: { hostId: string; onClose: () => void; onDone: (v: AccountProfileView) => void }) {
  useLang();
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const submit = async () => {
    setBusy(true);
    try {
      const view = await rpc<AccountProfileView>(hostId, 'tools.accounts.import', { bundle: JSON.parse(text) });
      toast(tx('importedToast', { label: view.label }), 'info');
      onDone(view);
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Layer onClose={onClose}>
      <DlgHead title={tx('importTitle')} onClose={onClose} />
      <textarea rows={7} className="mono small" value={text} placeholder={tx('importPlaceholder')} onChange={(e) => setText(e.target.value)} />
      <div className="acc-dlg-foot">
        <label className="btn">
          <Icon name="upload" size={14} /> {tx('loadFile')}
          <input
            type="file"
            accept=".json,application/json"
            style={{ display: 'none' }}
            onChange={(e) => {
              const file = e.target.files?.[0];
              if (file) file.text().then(setText).catch(errorToast);
            }}
          />
        </label>
        <span className="grow" />
        <button className="btn" onClick={onClose}>
          {tx('cancel')}
        </button>
        <button className="btn primary" disabled={busy || !text.trim()} onClick={() => void submit()}>
          {tx('importSubmit')}
        </button>
      </div>
    </Layer>
  );
}

export function ExportDialog({ text, onClose }: { text: string; onClose: () => void }) {
  useLang();
  return (
    <Layer onClose={onClose}>
      <DlgHead title={tx('exportTitle')} onClose={onClose} />
      <div className="acc-note warn">{tx('exportWarn')}</div>
      <textarea rows={8} className="mono small" readOnly value={text} />
      <div className="acc-dlg-foot">
        <button className="btn primary" onClick={() => void copyText(text)}>
          <Icon name="copy" size={14} /> {tx('copy')}
        </button>
      </div>
    </Layer>
  );
}

/** Codex: which provider config.toml selects and where its key comes from */
export function ActualAuthLines({ a, oauthUnused }: { a: ActualAuth; oauthUnused: boolean }) {
  useLang();
  return (
    <>
      <div className="small">
        {a.baseUrl ? tx('actualProviderLine', { provider: a.provider, url: a.baseUrl }) : tx('actualProviderLineNoUrl', { provider: a.provider })}
      </div>
      {a.method === 'providerApiKey' && a.envKey && (
        <div className={a.keySet ? 'muted small' : 'acc-warn small'}>
          {a.keySet ? tx('actualKeyFromEnv', { env: a.envKey }) : tx('actualKeyMissing', { env: a.envKey })}
        </div>
      )}
      {a.method === 'providerOpenaiAuth' && <div className="muted small">{tx('actualOpenaiAuthLine')}</div>}
      {oauthUnused && a.method !== 'providerOpenaiAuth' && <div className="muted small">{tx('actualOauthUnused')}</div>}
    </>
  );
}

/** Short name for what a Codex custom provider authenticates with */
export function actualMethodLabel(a: ActualAuth) {
  return a.method === 'providerApiKey'
    ? tx('actualProviderApiKey', { provider: a.provider })
    : a.method === 'providerOpenaiAuth'
      ? tx('actualProviderOpenaiAuth', { provider: a.provider })
      : tx('actualProviderNoAuth', { provider: a.provider });
}

export const useMemoLang = <T,>(f: () => T, deps: unknown[]) => {
  const lang = useLang();
  // eslint-disable-next-line react-hooks/exhaustive-deps
  return useMemo(f, [lang, ...deps]);
};
