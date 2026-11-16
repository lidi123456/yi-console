import { useCallback, useEffect, useMemo, useState, type MouseEvent } from 'react';
import { getT, trErr, useLang } from '../i18n';
import { errorToast, onAccountLogin, onAccountsUpdated, toast } from '../store';
import { rpc } from '../transport';
import {
  TOOL_LABEL,
  supportsAccountsTool,
  type AccountLoginUpdate,
  type AccountProfileView,
  type AccountQuotaEntry,
  type AccountState,
  type ApiKeyConfig,
  type ToolAccountsSummary,
  type ToolKind,
  type ToolsQuotasResponse,
} from '../types';
import { useBack } from '../back';
import { Icon, ToolBadge } from './Icon';
import { QuotaBody, quotaErrText } from './QuotaBody';
import { askConfirm, askText, openMenu } from './Overlay';

const tx = getT('accounts');
const ACCOUNT_TOOLS: ToolKind[] = ['claude', 'codex', 'cursor', 'kiro', 'qoder', 'codebuddy', 'antigravity'];

interface FormState {
  id: string | null;
  authKind: 'oauth' | 'apiKey';
  label: string;
  apiKey: string;
  authToken: string;
  baseUrl: string;
  defaultModel: string;
  providerName: string;
  extraEnvText: string;
  priority: string;
  allowAutoFailover: boolean;
}

const emptyApiKeyForm = (): FormState => ({
  id: null,
  authKind: 'apiKey',
  label: '',
  apiKey: '',
  authToken: '',
  baseUrl: '',
  defaultModel: '',
  providerName: '',
  extraEnvText: '',
  priority: '0',
  allowAutoFailover: false,
});

function formFromProfile(acc: AccountProfileView): FormState {
  const api = acc.apiConfig;
  const extraLines = api?.extraEnv
    ? Object.entries(api.extraEnv)
        .map(([k, v]) => `${k}=${v}`)
        .join('\n')
    : '';
  return {
    id: acc.id,
    authKind: acc.authKind,
    label: acc.label,
    apiKey: api?.apiKey ?? '',
    authToken: api?.authToken ?? '',
    baseUrl: api?.baseUrl ?? '',
    defaultModel: api?.defaultModel ?? '',
    providerName: api?.providerName ?? '',
    extraEnvText: extraLines,
    priority: String(acc.priority ?? 0),
    allowAutoFailover: acc.allowAutoFailover,
  };
}

function parseExtraEnv(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line || line.startsWith('#')) continue;
    const eq = line.indexOf('=');
    if (eq <= 0) continue;
    const k = line.slice(0, eq).trim();
    const v = line.slice(eq + 1);
    if (k) out[k] = v;
  }
  return out;
}

function stateBadge(state: AccountState): { cls: string; text: string } {
  switch (state) {
    case 'ready':
      return { cls: 'acc-badge-ok', text: tx('stateReady') };
    case 'needsLogin':
      return { cls: 'acc-badge-warn', text: tx('stateNeedsLogin') };
    case 'limited':
      return { cls: 'acc-badge-warn', text: tx('stateLimited') };
    case 'error':
      return { cls: 'acc-badge-err', text: tx('stateError') };
  }
}

export interface OpenAccountManagerReq {
  hostId: string;
  tool?: ToolKind;
  sessionId?: string;
  accountId?: string | null;
}

const OPEN_ACCOUNTS_EVENT = 'yi-console:open-accounts';
let accountManagerDepth = 0;

export function isAccountManagerOpen() {
  return accountManagerDepth > 0;
}

export function openAccountManager(req: OpenAccountManagerReq) {
  window.dispatchEvent(new CustomEvent<OpenAccountManagerReq>(OPEN_ACCOUNTS_EVENT, { detail: req }));
}

export function AccountManagerHost() {
  const [req, setReq] = useState<OpenAccountManagerReq | null>(null);
  useEffect(() => {
    const onOpen = (e: Event) => {
      const detail = (e as CustomEvent<OpenAccountManagerReq>).detail;
      if (detail?.hostId) setReq(detail);
    };
    window.addEventListener(OPEN_ACCOUNTS_EVENT, onOpen);
    return () => window.removeEventListener(OPEN_ACCOUNTS_EVENT, onOpen);
  }, []);
  if (!req) return null;
  return (
    <AccountManagerModal
      hostId={req.hostId}
      initialTool={req.tool}
      sessionId={req.sessionId}
      currentSessionAccountId={req.accountId}
      onClose={() => setReq(null)}
    />
  );
}

export function AccountManagerModal({
  hostId,
  initialTool,
  sessionId,
  currentSessionAccountId,
  onClose,
}: {
  hostId: string;
  initialTool?: ToolKind;
  sessionId?: string;
  currentSessionAccountId?: string | null;
  onClose: () => void;
}) {
  const lang = useLang();
  const [tool, setTool] = useState<ToolKind>(() => (initialTool && supportsAccountsTool(initialTool) ? initialTool : 'claude'));
  const [summaries, setSummaries] = useState<Record<string, ToolAccountsSummary>>({});
  const [quotasByAcc, setQuotasByAcc] = useState<Record<string, AccountQuotaEntry>>({});
  const [probingAll, setProbingAll] = useState(false);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [form, setForm] = useState<FormState | null>(null);
  const [showImport, setShowImport] = useState(false);
  const [importText, setImportText] = useState('');
  const [exportBundle, setExportBundle] = useState<string | null>(null);
  const [loginJob, setLoginJob] = useState<AccountLoginUpdate | null>(null);
  const [loginInput, setLoginInput] = useState('');

  useBack(true, onClose);

  useEffect(() => {
    accountManagerDepth++;
    return () => {
      accountManagerDepth = Math.max(0, accountManagerDepth - 1);
    };
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && !e.defaultPrevented) {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  const loadToolQuotas = useCallback(
    async (targetTool: ToolKind, refresh: boolean, refreshInactive: boolean) => {
      if (refresh || refreshInactive) setProbingAll(true);
      try {
        const res = await rpc<ToolsQuotasResponse>(hostId, 'tools.quotas', {
          tool: targetTool,
          refresh,
          refreshInactive,
        });
        const next: Record<string, AccountQuotaEntry> = {};
        for (const grp of res.tools ?? []) {
          for (const entry of grp.accounts ?? []) {
            next[entry.accountId] = entry;
          }
        }
        setQuotasByAcc((prev) => ({ ...prev, ...next }));
      } catch (e) {
        if (refresh || refreshInactive) errorToast(e);
      } finally {
        if (refresh || refreshInactive) setProbingAll(false);
      }
    },
    [hostId],
  );

  const loadAccounts = useCallback(
    async (targetTool?: ToolKind) => {
      setLoading(true);
      try {
        const r = await rpc<{ tools: ToolAccountsSummary[]; logins?: AccountLoginUpdate[] }>(
          hostId,
          'tools.accounts.list',
          { tool: targetTool, checkNative: true },
        );
        setSummaries((prev) => {
          const next = { ...prev };
          for (const s of r.tools ?? []) next[s.tool] = s;
          return next;
        });
        const activeJob = (r.logins ?? []).find((j) => j.tool === (targetTool ?? tool) && j.running);
        if (activeJob) setLoginJob(activeJob);
      } catch (e) {
        errorToast(e);
      } finally {
        setLoading(false);
      }
    },
    [hostId, tool],
  );

  useEffect(() => {
    void loadAccounts();
  }, [loadAccounts]);

  useEffect(() => {
    void loadToolQuotas(tool, false, false);
  }, [tool, loadToolQuotas]);

  useEffect(() => {
    const offAcc = onAccountsUpdated((h, updatedTool) => {
      if (h !== hostId) return;
      void loadAccounts(updatedTool);
      if (!updatedTool || updatedTool === tool) void loadToolQuotas(tool, false, false);
    });
    const offLogin = onAccountLogin((h, upd) => {
      if (h !== hostId || upd.tool !== tool) return;
      setLoginJob(upd);
      if (!upd.running) {
        void loadAccounts(tool);
        if (upd.ok) void loadToolQuotas(tool, false, false);
      }
    });
    return () => {
      offAcc();
      offLogin();
    };
  }, [hostId, tool, loadAccounts, loadToolQuotas]);

  const summary = summaries[tool];

  const toolCounts = useMemo(() => {
    void lang;
    return ACCOUNT_TOOLS.map((k) => ({
      kind: k,
      label: TOOL_LABEL[k],
      count: summaries[k]?.accounts.length ?? 0,
    }));
  }, [summaries, lang]);

  const copyText = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      toast(tx('copied'), 'info');
    } catch {
      /* fallback ignored if clipboard blocked */
    }
  };

  const handleCapture = async (source?: 'oauth' | 'apiKey') => {
    const nativeMethod = summary?.nativeStatus?.authMethod;
    const isApiKeyCapture =
      source === 'apiKey' || (!source && (nativeMethod === 'api_key' || nativeMethod === 'env_api_key'));
    const defaultAutoLabel = `${TOOL_LABEL[tool]} ${isApiKeyCapture ? 'API Key' : 'OAuth'}`;
    const suggested = isApiKeyCapture
      ? summary?.nativeStatus?.email
        ? `${summary.nativeStatus.email} (API Key)`
        : defaultAutoLabel
      : summary?.nativeStatus?.email || summary?.nativeStatus?.plan || defaultAutoLabel;
    const title =
      source === 'apiKey' && tool === 'cursor'
        ? tx('importCursorEnvKey')
        : source === 'oauth' && tool === 'cursor'
          ? tx('captureLocalOauth')
          : tx('captureCurrent');
    const label = await askText({
      title,
      label: tx('capturePrompt'),
      value: suggested,
      okText: tx('save'),
    });
    if (label === null) return;
    const trimmed = label.trim();
    setBusy(true);
    try {
      const view = await rpc<AccountProfileView>(hostId, 'tools.accounts.capture', {
        tool,
        source,
        label: trimmed && trimmed !== defaultAutoLabel ? trimmed : undefined,
      });
      toast(tx('capturedToast', { label: view.label }), 'info');
      await loadAccounts(tool);
      void loadToolQuotas(tool, false, false);
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const startLogin = async (mode?: string, accountId?: string, label?: string) => {
    setBusy(true);
    try {
      const upd = await rpc<AccountLoginUpdate>(hostId, 'tools.accounts.loginStart', {
        tool,
        mode,
        accountId,
        label,
      });
      setLoginJob(upd);
      setLoginInput('');
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const handleLoginClick = (e: MouseEvent) => {
    const canDevice = summary?.supportsDeviceAuth ?? tool === 'codex';
    const canInteractive = summary?.supportsInteractiveLogin ?? true;
    if (canDevice && canInteractive) {
      openMenu(e, [
        { label: tx('loginDeviceAuth'), run: () => void startLogin('device_auth') },
        { label: tx('loginInteractive'), run: () => void startLogin('interactive') },
      ]);
      return;
    }
    void startLogin();
  };

  const handleSendLoginInput = async () => {
    if (!loginJob) return;
    setBusy(true);
    try {
      const upd = await rpc<AccountLoginUpdate>(hostId, 'tools.accounts.loginInput', {
        jobId: loginJob.jobId,
        input: loginInput,
      });
      setLoginJob(upd);
      setLoginInput('');
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const handleCancelLogin = async () => {
    if (!loginJob) return;
    setBusy(true);
    try {
      const upd = await rpc<AccountLoginUpdate>(hostId, 'tools.accounts.loginCancel', {
        jobId: loginJob.jobId,
      });
      setLoginJob(upd);
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const handleToggleAutoFailoverTool = async (next: boolean) => {
    setBusy(true);
    try {
      await rpc(hostId, 'tools.accounts.setPolicy', { tool, autoFailover: next });
      await loadAccounts(tool);
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const handleSaveForm = async () => {
    if (!form) return;
    setBusy(true);
    try {
      const apiConfig: ApiKeyConfig | undefined =
        form.authKind === 'apiKey'
          ? {
              apiKey: form.apiKey.trim(),
              authToken: form.authToken.trim(),
              baseUrl: form.baseUrl.trim(),
              defaultModel: form.defaultModel.trim(),
              providerName: form.providerName.trim(),
              extraEnv: parseExtraEnv(form.extraEnvText),
            }
          : undefined;
      const priority = Number.parseInt(form.priority.trim() || '0', 10);
      const view = await rpc<AccountProfileView>(hostId, 'tools.accounts.save', {
        tool,
        id: form.id ?? undefined,
        label: form.label.trim() || undefined,
        authKind: form.authKind,
        allowAutoFailover: form.allowAutoFailover,
        priority: Number.isFinite(priority) ? priority : 0,
        apiConfig,
      });
      toast(tx('savedToast', { label: view.label }), 'info');
      setForm(null);
      await loadAccounts(tool);
      void loadToolQuotas(tool, false, false);
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const handleActivate = async (acc: AccountProfileView) => {
    setBusy(true);
    try {
      await rpc(hostId, 'tools.accounts.activate', { tool, accountId: acc.id });
      toast(tx('activatedToast', { label: acc.label }), 'info');
      await loadAccounts(tool);
      void loadToolQuotas(tool, false, false);
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const handleSwitchSession = async (acc: AccountProfileView) => {
    if (!sessionId) return;
    setBusy(true);
    try {
      await rpc(hostId, 'session.switchAccount', { sessionId, accountId: acc.id });
      toast(tx('switchedSessionToast', { label: acc.label }), 'info');
      await loadAccounts(tool);
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const handleToggleAccountFailover = async (acc: AccountProfileView, allow: boolean) => {
    setBusy(true);
    try {
      await rpc(hostId, 'tools.accounts.save', {
        tool,
        id: acc.id,
        allowAutoFailover: allow,
      });
      await loadAccounts(tool);
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const handleExport = async (acc: AccountProfileView) => {
    setBusy(true);
    try {
      const bundle = await rpc<unknown>(hostId, 'tools.accounts.export', { tool, accountId: acc.id });
      const text = JSON.stringify(bundle, null, 2);
      setExportBundle(text);
      try {
        await navigator.clipboard.writeText(text);
        toast(tx('exportedToast'), 'info');
      } catch {
        /* export panel remains visible for manual copy */
      }
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const handleDelete = async (acc: AccountProfileView) => {
    const ok = await askConfirm({
      title: tx('delete'),
      message: tx('deleteConfirm', { label: acc.label }),
      okText: tx('delete'),
      danger: true,
    });
    if (!ok) return;
    setBusy(true);
    try {
      await rpc(hostId, 'tools.accounts.delete', { tool, accountId: acc.id });
      toast(tx('deletedToast', { label: acc.label }), 'info');
      await loadAccounts(tool);
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const handleImport = async () => {
    if (!importText.trim()) return;
    setBusy(true);
    try {
      const bundle = JSON.parse(importText);
      const view = await rpc<AccountProfileView>(hostId, 'tools.accounts.import', { bundle });
      toast(tx('importedToast', { label: view.label }), 'info');
      setShowImport(false);
      setImportText('');
      setTool(view.tool);
      await loadAccounts(view.tool);
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-bg" onMouseDown={onClose}>
      <div className="modal acc-modal" role="dialog" onMouseDown={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <div className="row" style={{ gap: 8 }}>
            <ToolBadge tool={tool} size={20} />
            <h3>{tx('title', { tool: TOOL_LABEL[tool] })}</h3>
          </div>
          <button className="icon-btn" title={tx('close')} onClick={onClose}>
            <Icon name="x" size={16} />
          </button>
        </div>

        <div className="acc-tabs" role="tablist" aria-label={tx('toolSelector')}>
          {toolCounts.map((item) => (
            <button
              key={item.kind}
              type="button"
              className={`acc-tab ${tool === item.kind ? 'active' : ''}`}
              onClick={() => {
                setTool(item.kind);
                setForm(null);
                setExportBundle(null);
                setShowImport(false);
                setLoginJob(null);
              }}
            >
              <span>{item.label}</span>
              {item.count > 0 && <span className="acc-tab-count">{item.count}</span>}
            </button>
          ))}
        </div>

        {summary && (
          <div className="acc-summary">
            <div className="muted small">
              {summary.oauthIsolation === 'isolated' ? tx('isolationIsolated') : tx('isolationHostActive')}
            </div>
            <label className="check small">
              <input
                type="checkbox"
                checked={summary.autoFailover}
                disabled={busy}
                onChange={(e) => void handleToggleAutoFailoverTool(e.target.checked)}
              />
              {tx('autoFailoverTool')}
            </label>
          </div>
        )}

        {summary?.nativeStatus && (
          <div className="acc-native">
            <div className="row" style={{ gap: 8, flexWrap: 'wrap' }}>
              <span className="small" style={{ fontWeight: 600 }}>
                {tx('nativeStatusTitle')}:
              </span>
              <span className={`acc-badge ${summary.nativeStatus.loggedIn ? 'acc-badge-ok' : 'acc-badge-muted'}`}>
                {summary.nativeStatus.loggedIn
                  ? summary.nativeStatus.authMethod
                    ? tx('nativeLoggedIn', { method: summary.nativeStatus.authMethod })
                    : tx('nativeLoggedInSimple')
                  : tx('nativeNotLoggedIn')}
              </span>
              {summary.nativeStatus.email && <span className="small muted">{summary.nativeStatus.email}</span>}
              {summary.nativeStatus.plan && <span className="small muted">({summary.nativeStatus.plan})</span>}
              {summary.nativeStatus.matchedAccountId && (
                <span className="small muted">
                  {tx('matchedProfile', {
                    label:
                      summary.accounts.find((a) => a.id === summary.nativeStatus?.matchedAccountId)?.label ??
                      summary.nativeStatus.matchedAccountId,
                  })}
                </span>
              )}
            </div>
            {summary.nativeStatus.envOverride && (
              <div className="row" style={{ gap: 8, flexWrap: 'wrap', alignItems: 'center' }}>
                <div className="acc-warn small">{tx('envOverrideWarn', { env: summary.nativeStatus.envOverride })}</div>
                {tool === 'cursor' &&
                  summary.nativeStatus.authMethod === 'env_api_key' &&
                  !summary.nativeStatus.envApiKeyMatchedId && (
                    <button
                      type="button"
                      className="btn small primary"
                      disabled={busy}
                      onClick={() => void handleCapture('apiKey')}
                    >
                      <Icon name="plus" size={14} /> {tx('importCursorEnvKey')}
                    </button>
                  )}
              </div>
            )}
            {summary.nativeStatus.detail && <div className="muted small">{trErr(summary.nativeStatus.detail)}</div>}
          </div>
        )}

        <div className="acc-actions">
          <button
            className="btn small"
            disabled={busy}
            onClick={() =>
              void handleCapture(
                tool === 'cursor' &&
                  summary?.nativeStatus?.authMethod === 'env_api_key' &&
                  summary?.nativeStatus?.hasLocalOauth
                  ? 'oauth'
                  : undefined,
              )
            }
          >
            <Icon name="download" size={14} />{' '}
            {tool === 'cursor' &&
            summary?.nativeStatus?.authMethod === 'env_api_key' &&
            summary?.nativeStatus?.hasLocalOauth
              ? tx('captureLocalOauth')
              : tx('captureCurrent')}
          </button>
          {(summary?.supportsOauth ?? true) && (
            <button className="btn small" disabled={busy || loginJob?.running} onClick={handleLoginClick}>
              <Icon name="key" size={14} /> {tx('oauthLogin')}
            </button>
          )}
          {(summary?.supportsApiKey ?? true) && (
            <button
              className="btn small"
              disabled={busy}
              onClick={() => {
                setForm(emptyApiKeyForm());
                setShowImport(false);
              }}
            >
              <Icon name="plus" size={14} /> {tool === 'cursor' ? tx('addCursorApiKey') : tx('addApiKey')}
            </button>
          )}
          <button
            className="btn small"
            disabled={busy}
            onClick={() => {
              setShowImport((v) => !v);
              setForm(null);
            }}
          >
            <Icon name="upload" size={14} /> {tx('importBundle')}
          </button>
          <span className="grow" />
          <button
            className="btn small"
            disabled={probingAll}
            onClick={() => void loadToolQuotas(tool, true, false)}
          >
            <Icon name="refresh" size={14} /> {tx('refreshQuotas')}
          </button>
          {summary?.oauthIsolation === 'hostActive' && (summary?.accounts.length ?? 0) > 1 && (
            <button
              className="btn small"
              disabled={probingAll}
              onClick={() => void loadToolQuotas(tool, true, true)}
            >
              <Icon name="refresh" size={14} /> {tx('probeAllQuotas')}
            </button>
          )}
        </div>

        {loginJob && (
          <div className="acc-panel">
            <div className="row" style={{ gap: 8 }}>
              <strong className="small">{tx('loginTitle', { mode: loginJob.mode })}</strong>
              <span className={`acc-badge ${loginJob.running ? 'acc-badge-warn' : loginJob.ok ? 'acc-badge-ok' : 'acc-badge-err'}`}>
                {loginJob.running ? tx('loginRunning') : loginJob.ok ? tx('loginSucceeded') : tx('loginFailed')}
              </span>
              <span className="grow" />
              {loginJob.running ? (
                <button className="btn small danger" disabled={busy} onClick={() => void handleCancelLogin()}>
                  {tx('loginCancel')}
                </button>
              ) : (
                <button className="icon-btn" onClick={() => setLoginJob(null)}>
                  <Icon name="x" size={14} />
                </button>
              )}
            </div>
            {loginJob.url && (
              <div className="row" style={{ gap: 8, flexWrap: 'wrap' }}>
                <a className="btn small primary" href={loginJob.url} target="_blank" rel="noopener noreferrer">
                  <Icon name="external" size={14} /> {tx('loginOpenUrl')}
                </a>
                <button className="btn small" onClick={() => void copyText(loginJob.url!)}>
                  <Icon name="copy" size={14} /> {tx('copy')}
                </button>
              </div>
            )}
            {loginJob.code && (
              <div className="row" style={{ gap: 8 }}>
                <span className="small">{tx('loginCodeLabel')}</span>
                <code className="acc-code">{loginJob.code}</code>
                <button className="btn small" onClick={() => void copyText(loginJob.code!)}>
                  <Icon name="copy" size={14} /> {tx('copy')}
                </button>
              </div>
            )}
            {loginJob.prompt && <div className="small">{loginJob.prompt}</div>}
            {loginJob.running && (
              <div className="row" style={{ gap: 6 }}>
                <input
                  className="grow"
                  value={loginInput}
                  placeholder={tx('loginInputPlaceholder')}
                  onChange={(e) => setLoginInput(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' && loginInput.trim()) void handleSendLoginInput();
                  }}
                />
                <button className="btn small primary" disabled={busy || !loginInput.trim()} onClick={() => void handleSendLoginInput()}>
                  {tx('loginSendInput')}
                </button>
              </div>
            )}
            {loginJob.error && <div className="acc-warn small">{trErr(loginJob.error)}</div>}
            {loginJob.output.length > 0 && <pre className="acc-log mono small">{loginJob.output.join('\n')}</pre>}
          </div>
        )}

        {showImport && (
          <div className="acc-panel">
            <div className="small" style={{ fontWeight: 600 }}>
              {tx('importTitle')}
            </div>
            <textarea
              rows={4}
              className="mono small"
              value={importText}
              placeholder={tx('importPlaceholder')}
              onChange={(e) => setImportText(e.target.value)}
            />
            <div className="row" style={{ gap: 8 }}>
              <label className="btn small">
                <Icon name="upload" size={14} /> {tx('loadFile')}
                <input
                  type="file"
                  accept=".json,application/json"
                  style={{ display: 'none' }}
                  onChange={(e) => {
                    const file = e.target.files?.[0];
                    if (!file) return;
                    file.text().then(setImportText).catch(errorToast);
                  }}
                />
              </label>
              <span className="grow" />
              <button className="btn small" onClick={() => setShowImport(false)}>
                {tx('cancel')}
              </button>
              <button className="btn small primary" disabled={busy || !importText.trim()} onClick={() => void handleImport()}>
                {tx('importSubmit')}
              </button>
            </div>
          </div>
        )}

        {exportBundle !== null && (
          <div className="acc-panel">
            <div className="row">
              <span className="small" style={{ fontWeight: 600 }}>
                {tx('exportTitle')}
              </span>
              <span className="grow" />
              <button className="btn small" onClick={() => void copyText(exportBundle)}>
                <Icon name="copy" size={14} /> {tx('copy')}
              </button>
              <button className="icon-btn" onClick={() => setExportBundle(null)}>
                <Icon name="x" size={14} />
              </button>
            </div>
            <textarea rows={4} className="mono small" readOnly value={exportBundle} />
          </div>
        )}

        {form && (
          <div className="acc-panel form">
            <div className="small" style={{ fontWeight: 600 }}>
              {form.id
                ? tx('formEditTitle', { label: form.label })
                : tool === 'cursor'
                  ? tx('formCreateCursorTitle')
                  : tx('formCreateTitle')}
            </div>
            {tool === 'cursor' && form.authKind === 'apiKey' && (
              <div className="muted small">{tx('cursorApiKeyHint')}</div>
            )}
            <label>
              {tx('fieldLabel')}
              <input
                value={form.label}
                placeholder={tool === 'cursor' && form.authKind === 'apiKey' ? tx('fieldLabelCursorPlaceholder') : tx('fieldLabelPlaceholder')}
                onChange={(e) => setForm({ ...form, label: e.target.value })}
              />
            </label>
            {form.authKind === 'apiKey' && (
              <>
                <div className="acc-form-grid">
                  <label>
                    {tool === 'cursor' ? tx('fieldCursorApiKey') : tx('fieldApiKey')}
                    <input
                      value={form.apiKey}
                      placeholder={tool === 'cursor' ? tx('fieldCursorApiKeyPlaceholder') : undefined}
                      onChange={(e) => setForm({ ...form, apiKey: e.target.value })}
                    />
                  </label>
                  {tool !== 'cursor' && (
                    <label>
                      {tx('fieldAuthToken')}
                      <input
                        value={form.authToken}
                        onChange={(e) => setForm({ ...form, authToken: e.target.value })}
                      />
                    </label>
                  )}
                  <label>
                    {tool === 'cursor' ? tx('fieldCursorEndpoint') : tx('fieldBaseUrl')}
                    <input
                      value={form.baseUrl}
                      placeholder={tool === 'cursor' ? 'https://api2.cursor.sh' : 'https://...'}
                      onChange={(e) => setForm({ ...form, baseUrl: e.target.value })}
                    />
                  </label>
                  {tool !== 'cursor' && (
                    <label>
                      {tx('fieldDefaultModel')}
                      <input
                        value={form.defaultModel}
                        onChange={(e) => setForm({ ...form, defaultModel: e.target.value })}
                      />
                    </label>
                  )}
                  {tool === 'codex' && (
                    <label>
                      {tx('fieldProviderName')}
                      <input
                        value={form.providerName}
                        onChange={(e) => setForm({ ...form, providerName: e.target.value })}
                      />
                    </label>
                  )}
                  <label>
                    {tx('fieldPriority')}
                    <input
                      type="number"
                      value={form.priority}
                      onChange={(e) => setForm({ ...form, priority: e.target.value })}
                    />
                  </label>
                </div>
                <label>
                  {tx('fieldExtraEnv')}
                  <textarea
                    rows={2}
                    className="mono small"
                    value={form.extraEnvText}
                    placeholder={tx('fieldExtraEnvPlaceholder')}
                    onChange={(e) => setForm({ ...form, extraEnvText: e.target.value })}
                  />
                </label>
              </>
            )}
            <label className="check small">
              <input
                type="checkbox"
                checked={form.allowAutoFailover}
                onChange={(e) => setForm({ ...form, allowAutoFailover: e.target.checked })}
              />
              {tx('fieldAllowFailover')}
            </label>
            <div className="row" style={{ gap: 8, justifyContent: 'flex-end' }}>
              <button className="btn small" onClick={() => setForm(null)}>
                {tx('cancel')}
              </button>
              <button className="btn small primary" disabled={busy} onClick={() => void handleSaveForm()}>
                {tx('save')}
              </button>
            </div>
          </div>
        )}

        <div className="acc-list">
          {loading && !summary ? (
            <div className="muted small" style={{ padding: 12 }}>
              …
            </div>
          ) : !summary || summary.accounts.length === 0 ? (
            <div className="muted small acc-empty">{tx('noAccounts')}</div>
          ) : (
            summary.accounts.map((acc) => {
              const st = stateBadge(acc.state);
              const qEntry = quotasByAcc[acc.id];
              const qVal = qEntry?.error ? quotaErrText(qEntry.error) : qEntry?.quota ?? acc.quota ?? undefined;
              const isSessionAcc = sessionId && currentSessionAccountId === acc.id;
              return (
                <div key={acc.id} className={`acc-card ${acc.active ? 'active' : ''}`}>
                  <div className="row" style={{ gap: 6, flexWrap: 'wrap' }}>
                    <strong>{acc.label}</strong>
                    <span className="acc-badge">{acc.authKind === 'apiKey' ? tx('authApiKey') : tx('authOauth')}</span>
                    {acc.active && <span className="acc-badge acc-badge-accent">{tx('activeBadge')}</span>}
                    {isSessionAcc && <span className="acc-badge acc-badge-ok">{tx('sessionCurrentBadge')}</span>}
                    <span className={`acc-badge ${st.cls}`}>{st.text}</span>
                    {acc.inUseSessions > 0 && (
                      <span className="muted small">{tx('inUseCount', { count: acc.inUseSessions })}</span>
                    )}
                    <span className="grow" />
                    <span className="muted small">{tx('priority', { v: acc.priority })}</span>
                  </div>

                  {(() => {
                    const meta = [
                      acc.identity.email,
                      acc.identity.plan,
                      acc.identity.workspace,
                      acc.apiConfig?.baseUrl,
                      acc.apiConfig?.defaultModel,
                      acc.stateDetail ? trErr(acc.stateDetail) : null,
                    ].filter((v, i, arr): v is string => !!v && arr.indexOf(v) === i);
                    return meta.length > 0 ? <div className="muted small acc-meta">{meta.join(' · ')}</div> : null;
                  })()}

                  {qVal && (
                    <div className="quota acc-quota">
                      <div className="muted small" style={{ fontWeight: 600 }}>{tx('quotaTitle')}</div>
                      <QuotaBody q={qVal} />
                      {typeof qVal === 'object' && qVal.stale && (
                        <div className="muted small">{tx('staleHint')}</div>
                      )}
                    </div>
                  )}

                  <div className="row" style={{ gap: 6, flexWrap: 'wrap', marginTop: 6 }}>
                    <label className="check small">
                      <input
                        type="checkbox"
                        checked={acc.allowAutoFailover}
                        disabled={busy}
                        onChange={(e) => void handleToggleAccountFailover(acc, e.target.checked)}
                      />
                      {tx('allowFailover')}
                    </label>
                    <span className="grow" />
                    {sessionId && !isSessionAcc && (
                      <button className="btn small primary" disabled={busy} onClick={() => void handleSwitchSession(acc)}>
                        <Icon name="forward" size={14} /> {tx('useForSession')}
                      </button>
                    )}
                    {!acc.active && (
                      <button className="btn small" disabled={busy} onClick={() => void handleActivate(acc)}>
                        <Icon name="check" size={14} /> {tx('setDefault')}
                      </button>
                    )}
                    {acc.authKind === 'oauth' && (
                      <button className="btn small" disabled={busy || loginJob?.running} onClick={() => void startLogin(undefined, acc.id, acc.label)}>
                        <Icon name="refresh" size={14} /> {tx('relogin')}
                      </button>
                    )}
                    <button
                      className="btn small"
                      disabled={busy}
                      onClick={() => {
                        setForm(formFromProfile(acc));
                        setShowImport(false);
                      }}
                    >
                      <Icon name="edit" size={14} /> {tx('edit')}
                    </button>
                    <button className="btn small" disabled={busy} onClick={() => void handleExport(acc)}>
                      <Icon name="copy" size={14} /> {tx('export')}
                    </button>
                    <button className="btn small danger" disabled={busy} onClick={() => void handleDelete(acc)}>
                      <Icon name="trash" size={14} /> {tx('delete')}
                    </button>
                  </div>
                </div>
              );
            })
          )}
        </div>
      </div>
    </div>
  );
}
