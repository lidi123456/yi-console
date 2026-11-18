import { useCallback, useEffect, useMemo, useState, type DragEvent, type MouseEvent } from 'react';
import { getT, trErr, useLang } from '../i18n';
import { errorToast, onAccountLogin, onAccountsUpdated, toast, useStore } from '../store';
import { rpc } from '../transport';
import {
  TOOL_LABEL,
  supportsAccountsTool,
  type AccountLoginUpdate,
  type AccountProfileView,
  type AccountQuotaEntry,
  type AccountState,
  type ToolAccountsSummary,
  type ToolKind,
  type ToolsQuotasResponse,
} from '../types';
import { useBack } from '../back';
import { Icon, ToolBadge } from './Icon';
import { QuotaBody, quotaErrText } from './QuotaBody';
import { askConfirm, askText, openMenu, type MenuItem } from './Overlay';
import { openTerminal } from './SessionActions';
import {
  AccountDrawer,
  ActualAuthLines,
  AddChooser,
  ExportDialog,
  ImportDialog,
  LoginDialog,
  TerminalGuide,
  actualMethodLabel,
  addOptions,
  loginKindOf,
  nativeWho,
  type AddOption,
  type LoginKind,
} from './AccountDialogs';

const tx = getT('accounts');

const ACCOUNT_TOOLS: ToolKind[] = ['claude', 'codex', 'cursor', 'kiro', 'qoder', 'codebuddy', 'antigravity'];

/** The provider replaces the account's ChatGPT login (an OAuth account that doesn't send that login anywhere) */
function actualMismatch(acc: AccountProfileView) {
  return acc.authKind === 'oauth' && !!acc.actual && acc.actual.method !== 'providerOpenaiAuth';
}

function stateChip(state: AccountState): { cls: string; text: string } {
  switch (state) {
    case 'ready':
      return { cls: 'ok', text: tx('stateReady') };
    case 'needsLogin':
      return { cls: 'warn', text: tx('stateNeedsLogin') };
    case 'limited':
      return { cls: 'warn', text: tx('stateLimited') };
    case 'error':
      return { cls: 'err', text: tx('stateError') };
  }
}

function authChip(tool: ToolKind, acc: AccountProfileView) {
  if (acc.actual) return actualMethodLabel(acc.actual);
  if (acc.authKind === 'apiKey') return tool === 'cursor' ? 'CURSOR_API_KEY' : tx('authApiKey');
  return tool === 'codex' ? tx('authChatgpt') : tool === 'antigravity' ? tx('authGoogle') : tx('authOauth');
}

const AVATAR_COLORS = ['#0969da', '#8250df', '#bf8700', '#1a7f37', '#cf222e', '#6e7781', '#bc4c00', '#1b7c83'];
function avatarColor(s: string) {
  let h = 0;
  for (const c of s) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return AVATAR_COLORS[h % AVATAR_COLORS.length];
}

/** Rail dot: red if any account is broken, yellow if any needs attention, green when all are usable */
function toolDot(s?: ToolAccountsSummary) {
  if (!s?.accounts.length) return '';
  if (s.accounts.some((a) => a.state === 'error')) return 'e';
  if (s.accounts.some((a) => a.state !== 'ready')) return 'w';
  return 'ok';
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

type LayerState =
  | { k: 'add' }
  | { k: 'login'; mode?: string; loginKind: LoginKind; relogin?: AccountProfileView }
  | { k: 'terminal' }
  | { k: 'drawer'; acc?: AccountProfileView }
  | { k: 'import' }
  | { k: 'export'; text: string };

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
  const isLocal = useStore((st) => st.hosts.find((h) => h.config.id === hostId)?.config.kind === 'local');
  const [tool, setTool] = useState<ToolKind>(() => (initialTool && supportsAccountsTool(initialTool) ? initialTool : 'claude'));
  const [summaries, setSummaries] = useState<Record<string, ToolAccountsSummary>>({});
  const [quotasByAcc, setQuotasByAcc] = useState<Record<string, AccountQuotaEntry>>({});
  const [probing, setProbing] = useState(false);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [layer, setLayer] = useState<LayerState | null>(null);
  const [loginJob, setLoginJob] = useState<AccountLoginUpdate | null>(null);
  const [details, setDetails] = useState(false);
  const [sessionAccId, setSessionAccId] = useState(currentSessionAccountId ?? null);
  const [dragId, setDragId] = useState<string | null>(null);
  const [order, setOrder] = useState<string[] | null>(null);

  useBack(true, onClose);

  useEffect(() => {
    accountManagerDepth++;
    return () => {
      accountManagerDepth = Math.max(0, accountManagerDepth - 1);
    };
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && !e.defaultPrevented && !layer) {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose, layer]);

  const loadToolQuotas = useCallback(
    async (targetTool: ToolKind, refresh: boolean, refreshInactive: boolean) => {
      if (refresh || refreshInactive) setProbing(true);
      try {
        const res = await rpc<ToolsQuotasResponse>(hostId, 'tools.quotas', { tool: targetTool, refresh, refreshInactive });
        const next: Record<string, AccountQuotaEntry> = {};
        for (const grp of res.tools ?? []) for (const entry of grp.accounts ?? []) next[entry.accountId] = entry;
        setQuotasByAcc((prev) => ({ ...prev, ...next }));
      } catch (e) {
        if (refresh || refreshInactive) errorToast(e);
      } finally {
        if (refresh || refreshInactive) setProbing(false);
      }
    },
    [hostId],
  );

  const loadAccounts = useCallback(
    async (targetTool?: ToolKind) => {
      setLoading(true);
      try {
        const r = await rpc<{ tools: ToolAccountsSummary[]; logins?: AccountLoginUpdate[] }>(hostId, 'tools.accounts.list', {
          tool: targetTool,
          checkNative: true,
        });
        setSummaries((prev) => {
          const next = { ...prev };
          for (const s of r.tools ?? []) next[s.tool] = s;
          return next;
        });
        setOrder(null);
        // A sign-in still running from before (the window was closed meanwhile): show it again
        const active = (r.logins ?? []).find((j) => j.tool === (targetTool ?? tool) && j.running);
        if (active) {
          setLoginJob(active);
          setLayer((l) => l ?? { k: 'login', mode: active.mode, loginKind: loginKindOf(active.tool, active.mode) });
        }
      } catch (e) {
        errorToast(e);
      } finally {
        setLoading(false);
      }
    },
    [hostId, tool],
  );
  const reloadTool = useCallback(() => void loadAccounts(tool), [loadAccounts, tool]);

  useEffect(() => {
    void loadAccounts();
    // Only on open: later reloads are per tool
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId]);

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
      setLoginJob((cur) => (cur && cur.jobId !== upd.jobId && cur.running ? cur : upd));
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
  const isolation = summary?.oauthIsolation;
  const canLogin = summary ? summary.supportsInteractiveLogin || summary.supportsDeviceAuth : true;
  const accounts = useMemo(() => {
    const list = summary?.accounts ?? [];
    if (!order) return list;
    return [...list].sort((a, b) => order.indexOf(a.id) - order.indexOf(b.id));
  }, [summary, order]);
  const options = useMemo(() => {
    void lang;
    return addOptions(tool, summary, isLocal);
  }, [tool, summary, isLocal, lang]);

  const run = async (f: () => Promise<void>) => {
    setBusy(true);
    try {
      await f();
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  const startLogin = async (mode?: string, relogin?: AccountProfileView) => {
    setLoginJob(null);
    setLayer({ k: 'login', mode, loginKind: loginKindOf(tool, mode), relogin });
    try {
      const upd = await rpc<AccountLoginUpdate>(hostId, 'tools.accounts.loginStart', {
        tool,
        mode,
        accountId: relogin?.id,
        label: relogin?.label,
      });
      setLoginJob(upd);
    } catch (e) {
      errorToast(e);
      setLayer(null);
    }
  };

  const capture = async (source?: 'oauth' | 'apiKey') => {
    const ns = summary?.nativeStatus;
    const suggested =
      tool === 'codex' && source !== 'oauth' && ns?.actual?.method === 'providerApiKey'
        ? `codex (${ns.actual.provider})`
        : nativeWho(summary) || `${TOOL_LABEL[tool]} ${source === 'apiKey' ? 'API Key' : ''}`.trim();
    const label = await askText({ title: tx('captureNameTitle'), label: tx('captureNameLabel'), value: suggested, okText: tx('save') });
    if (label === null) return;
    await run(async () => {
      const view = await rpc<AccountProfileView>(hostId, 'tools.accounts.capture', { tool, source, label: label.trim() || undefined });
      toast(tx('capturedToast', { label: view.label }), 'info');
      setLayer(null);
      await loadAccounts(tool);
      void loadToolQuotas(tool, false, false);
    });
  };

  const pick = (o: AddOption) => {
    switch (o.kind) {
      case 'login':
        void startLogin(o.mode);
        break;
      case 'terminal':
        setLayer({ k: 'terminal' });
        break;
      case 'apiKey':
        setLayer({ k: 'drawer' });
        break;
      case 'capture':
        setLayer(null);
        void capture(o.source);
        break;
      case 'import':
        setLayer({ k: 'import' });
        break;
    }
  };

  const activate = async (acc: AccountProfileView) => {
    if (isolation === 'hostActive') {
      const running = (summary?.accounts ?? []).reduce((n, a) => n + a.inUseSessions, 0);
      if (running > 0) {
        const ok = await askConfirm({
          title: tx('activateConfirmTitle'),
          message: tx('activateConfirm', { count: running, tool: TOOL_LABEL[tool], label: acc.label }),
          okText: tx('setDefault'),
        });
        if (!ok) return;
      }
    }
    await run(async () => {
      await rpc(hostId, 'tools.accounts.activate', { tool, accountId: acc.id });
      toast(tx('activatedToast', { label: acc.label }), 'info');
      await loadAccounts(tool);
      void loadToolQuotas(tool, false, false);
    });
  };

  const switchSession = (acc: AccountProfileView) =>
    run(async () => {
      if (!sessionId) return;
      await rpc(hostId, 'session.switchAccount', { sessionId, accountId: acc.id });
      setSessionAccId(acc.id);
      toast(tx('switchedSessionToast', { label: acc.label }), 'info');
      await loadAccounts(tool);
    });

  const setFailover = (acc: AccountProfileView, allow: boolean) =>
    run(async () => {
      await rpc(hostId, 'tools.accounts.save', { tool, id: acc.id, allowAutoFailover: allow });
      await loadAccounts(tool);
    });

  const exportAcc = (acc: AccountProfileView) =>
    run(async () => {
      const bundle = await rpc<unknown>(hostId, 'tools.accounts.export', { tool, accountId: acc.id });
      const text = JSON.stringify(bundle, null, 2);
      setLayer({ k: 'export', text });
      try {
        await navigator.clipboard.writeText(text);
        toast(tx('exportedToast'), 'info');
      } catch {
        /* the dialog shows it for manual copy */
      }
    });

  const remove = async (acc: AccountProfileView) => {
    const ok = await askConfirm({ title: tx('delete'), message: tx('deleteConfirm', { label: acc.label }), okText: tx('delete'), danger: true });
    if (!ok) return;
    await run(async () => {
      await rpc(hostId, 'tools.accounts.delete', { tool, accountId: acc.id });
      toast(tx('deletedToast', { label: acc.label }), 'info');
      await loadAccounts(tool);
    });
  };

  const convert = async (acc: AccountProfileView, to: 'apiKey' | 'oauth') => {
    const provider = acc.actual?.provider ?? '';
    const ok = await askConfirm({
      title: to === 'apiKey' ? tx('convertToApiKey') : tx('convertToOauth'),
      message:
        to === 'apiKey'
          ? tx('convertToApiKeyConfirm', { label: acc.label, provider, env: acc.actual?.envKey ?? '' })
          : tx('convertToOauthConfirm', { label: acc.label, provider }),
      okText: to === 'apiKey' ? tx('convertToApiKey') : tx('convertToOauth'),
    });
    if (!ok) return;
    await run(async () => {
      const view = await rpc<AccountProfileView>(hostId, 'tools.accounts.convert', { tool, accountId: acc.id, to });
      toast(tx('convertedToast', { label: view.label }), 'info');
      await loadAccounts(tool);
      void loadToolQuotas(tool, false, false);
    });
  };

  const saveOrder = (ids: string[]) =>
    run(async () => {
      setOrder(ids);
      await rpc(hostId, 'tools.accounts.reorder', { tool, ids });
      await loadAccounts(tool);
    });
  const move = (acc: AccountProfileView, d: -1 | 1) => {
    const ids = accounts.map((a) => a.id);
    const i = ids.indexOf(acc.id);
    const j = i + d;
    if (j < 0 || j >= ids.length) return;
    [ids[i], ids[j]] = [ids[j], ids[i]];
    void saveOrder(ids);
  };

  const onDragOver = (e: DragEvent, over: AccountProfileView) => {
    if (!dragId || dragId === over.id) return;
    e.preventDefault();
    const ids = accounts.map((a) => a.id);
    const from = ids.indexOf(dragId);
    const to = ids.indexOf(over.id);
    ids.splice(from, 1);
    ids.splice(to, 0, dragId);
    setOrder(ids);
  };

  // Kiro's in-app sign-in is Builder ID only: re-signing a Google / GitHub / Identity Center account would replace it
  const canRelogin = (acc: AccountProfileView) =>
    acc.authKind === 'oauth' && canLogin && (tool !== 'kiro' || acc.identity.workspace === 'Builder ID');
  const reloginMode = tool === 'codex' ? 'device_auth' : undefined;

  const primaryAction = (acc: AccountProfileView) => {
    if (acc.state === 'needsLogin' && canRelogin(acc)) return 'relogin';
    if (sessionId && sessionAccId !== acc.id) return 'session';
    if (!acc.active) return 'default';
    return null;
  };

  const cardMenu = (e: MouseEvent, acc: AccountProfileView, idx: number) => {
    const primary = primaryAction(acc);
    const items: (MenuItem | '-')[] = [{ label: tx('edit'), icon: 'edit', run: () => setLayer({ k: 'drawer', acc }) }];
    if (canRelogin(acc) && primary !== 'relogin') items.push({ label: tx('relogin'), icon: 'refresh', run: () => void startLogin(reloginMode, acc) });
    if (sessionId && sessionAccId !== acc.id && primary !== 'session') items.push({ label: tx('useForSession'), icon: 'forward', run: () => void switchSession(acc) });
    if (!acc.active && primary !== 'default') items.push({ label: tx('setDefault'), icon: 'check', run: () => void activate(acc) });
    items.push({
      label: acc.allowAutoFailover ? tx('leaveFailover') : tx('joinFailover'),
      icon: 'refresh',
      run: () => void setFailover(acc, !acc.allowAutoFailover),
    });
    if (accounts.length > 1) {
      items.push('-');
      if (idx > 0) items.push({ label: tx('moveUp'), icon: 'up', run: () => move(acc, -1) });
      if (idx < accounts.length - 1) items.push({ label: tx('moveDown'), icon: 'down', run: () => move(acc, 1) });
    }
    items.push('-', { label: tx('export'), icon: 'upload', run: () => void exportAcc(acc) });
    items.push('-', { label: tx('delete'), icon: 'trash', danger: true, run: () => void remove(acc) });
    openMenu(e, items);
  };

  const refreshClick = (e: MouseEvent) => {
    const refresh = () => void loadToolQuotas(tool, true, false).then(() => loadAccounts(tool));
    if (isolation === 'hostActive' && accounts.length > 1) {
      openMenu(e, [
        { label: tx('refreshQuotas'), icon: 'refresh', run: refresh },
        { label: tx('probeAllQuotas'), icon: 'refresh', run: () => void loadToolQuotas(tool, true, true).then(() => loadAccounts(tool)) },
      ]);
      return;
    }
    refresh();
  };

  const ns = summary?.nativeStatus;
  const matched = ns?.matchedAccountId ? summary?.accounts.find((a) => a.id === ns.matchedAccountId) : undefined;
  const nativeLine = !ns
    ? null
    : ns.actual
      ? ns.loggedIn
        ? tx('nativeLoggedIn', { tool: TOOL_LABEL[tool], method: actualMethodLabel(ns.actual) })
        : tx('nativeKeyMissing', { tool: TOOL_LABEL[tool], method: actualMethodLabel(ns.actual) })
      : ns.loggedIn
        ? nativeWho(summary)
          ? tx('nativeLoggedInWho', { tool: TOOL_LABEL[tool], who: nativeWho(summary) })
          : tx('nativeLoggedIn', { tool: TOOL_LABEL[tool], method: ns.authMethod ?? 'OAuth' })
        : tx('nativeNotLoggedIn', { tool: TOOL_LABEL[tool] });
  const hasDetails = !!(ns?.actual || ns?.envOverride || ns?.detail);
  const captureOpt = options.find((o) => o.kind === 'capture');
  const captureSource = captureOpt?.kind === 'capture' ? captureOpt.source : undefined;

  const switchTool = (k: ToolKind) => {
    if (k === tool) return;
    setTool(k);
    setLayer(null);
    setLoginJob(null);
    setDetails(false);
    setOrder(null);
    if (!summaries[k]) void loadAccounts(k);
  };

  return (
    <div className="modal-bg" onMouseDown={() => { if (!layer) onClose(); }}>
      <div className="modal acc2" role="dialog" onMouseDown={(e) => e.stopPropagation()}>
        <nav className="acc2-rail" aria-label={tx('toolSelector')}>
          <div className="acc2-rail-title">{tx('title')}</div>
          {ACCOUNT_TOOLS.map((k) => {
            const s = summaries[k];
            const n = s?.accounts.length ?? 0;
            const dot = toolDot(s);
            return (
              <button key={k} type="button" className={`acc2-ti ${tool === k ? 'on' : ''}`} onClick={() => switchTool(k)}>
                <ToolBadge tool={k} size={18} />
                <span className="acc2-ti-name">{TOOL_LABEL[k]}</span>
                <span className="acc2-ti-n">
                  {dot && <span className={`acc-dot ${dot}`} />}
                  {n > 0 ? n : '—'}
                </span>
              </button>
            );
          })}
          <span className="grow" />
          <button type="button" className="acc2-ti small" onClick={() => setLayer({ k: 'import' })}>
            <Icon name="upload" size={14} />
            <span className="acc2-ti-name">{tx('importBundle')}</span>
          </button>
        </nav>

        <div className="acc2-main">
          <div className="acc2-head">
            <ToolBadge tool={tool} size={22} />
            <h3>{TOOL_LABEL[tool]}</h3>
            {isolation && (
              <span
                className={`acc-chip ${isolation === 'hostActive' ? 'warn' : ''}`}
                title={isolation === 'hostActive' ? tx('chipHostActiveTip') : tx('chipIsolatedTip')}
              >
                {tool === 'codebuddy' ? tx('chipSharedIde') : isolation === 'hostActive' ? tx('chipHostActive') : tx('chipIsolated')}
              </span>
            )}
            <span className="grow" />
            {summary && (
              <label className="acc-switch small" title={tx('autoFailoverTip')}>
                <input
                  type="checkbox"
                  checked={summary.autoFailover}
                  disabled={busy}
                  onChange={(e) =>
                    void run(async () => {
                      await rpc(hostId, 'tools.accounts.setPolicy', { tool, autoFailover: e.target.checked });
                      await loadAccounts(tool);
                    })
                  }
                />
                <i />
                <span className="acc2-hide-sm">{tx('autoFailover')}</span>
              </label>
            )}
            <button className="icon-btn" title={tx('refreshQuotas')} disabled={probing} onClick={refreshClick}>
              <Icon name="refresh" size={15} />
            </button>
            <button className="btn primary" onClick={() => setLayer({ k: 'add' })}>
              <Icon name="plus" size={13} /> <span className="acc2-hide-sm">{tx('addAccount')}</span>
              <span className="acc2-show-sm">{tx('addShort')}</span>
            </button>
            <button className="icon-btn" title={tx('close')} onClick={onClose}>
              <Icon name="x" size={15} />
            </button>
          </div>

          {nativeLine && (
            <div className="acc-strip">
              <span className={`acc-dot ${ns?.loggedIn ? '' : 'g'}`} />
              <span className="grow">
                {nativeLine}
                {ns?.loggedIn && <span className="muted"> · {matched ? tx('nativeSaved', { label: matched.label }) : tx('nativeNotSaved')}</span>}
              </span>
              {hasDetails && (
                <button type="button" className="acc-link" onClick={() => setDetails((v) => !v)}>
                  {tx('details')} <Icon name={details ? 'up' : 'down'} size={12} />
                </button>
              )}
            </div>
          )}
          {details && ns && (
            <div className="acc-details">
              {ns.envOverride && <div className="acc-warn small">{tx('envOverrideWarn', { env: ns.envOverride })}</div>}
              {ns.actual && <ActualAuthLines a={ns.actual} oauthUnused={!!ns.hasLocalOauth} />}
              {ns.detail && <div className="muted small">{trErr(ns.detail)}</div>}
            </div>
          )}
          {accounts.length > 0 && (isolation === 'hostActive' || accounts.length > 1) && (
            <div className="acc-hint">
              {isolation === 'hostActive' && <span>{tx('hostActiveHint', { tool: TOOL_LABEL[tool] })} </span>}
              {accounts.length > 1 && <span>{tx('orderHint')}</span>}
            </div>
          )}

          <div className="acc2-list">
            {loading && !summary ? (
              <div className="muted small acc2-empty">…</div>
            ) : accounts.length === 0 ? (
              <div className="acc2-empty">
                <b>{tx('emptyTitle', { tool: TOOL_LABEL[tool] })}</b>
                {ns?.loggedIn && !matched && nativeWho(summary) && <span className="acc-sub">{tx('emptyNative', { who: nativeWho(summary) })}</span>}
                {ns?.loggedIn && !matched ? (
                  <>
                    <button className="btn primary" disabled={busy} onClick={() => void capture(captureSource)}>
                      {tx('emptySaveNative')}
                    </button>
                    <button type="button" className="acc-link" onClick={() => setLayer({ k: 'add' })}>
                      {tx('emptyOtherWays')}
                    </button>
                  </>
                ) : (
                  <button className="btn primary" onClick={() => setLayer({ k: 'add' })}>
                    <Icon name="plus" size={13} /> {tx('addAccount')}
                  </button>
                )}
              </div>
            ) : (
              accounts.map((acc, idx) => {
                const st = stateChip(acc.state);
                const qEntry = quotasByAcc[acc.id];
                // The state line already says why (e.g. an expired login); don't repeat it as the quota error
                const qVal = qEntry?.error
                  ? qEntry.error === acc.stateDetail
                    ? undefined
                    : quotaErrText(qEntry.error)
                  : qEntry?.quota ?? acc.quota ?? undefined;
                const mismatch = actualMismatch(acc);
                const isSessionAcc = !!sessionId && sessionAccId === acc.id;
                const sub = [
                  mismatch || acc.identity.email === acc.label ? null : acc.identity.email,
                  acc.identity.workspace,
                  acc.actual?.baseUrl ?? acc.apiConfig?.baseUrl,
                  acc.apiConfig?.defaultModel,
                  acc.stateDetail ? trErr(acc.stateDetail) : null,
                ].filter((v, i, arr): v is string => !!v && arr.indexOf(v) === i);
                const foot = [
                  acc.inUseSessions > 0 ? tx('inUseCount', { count: acc.inUseSessions }) : null,
                  !acc.allowAutoFailover && summary?.autoFailover ? tx('noFailover') : null,
                ].filter(Boolean);
                const primary = primaryAction(acc);
                return (
                  <div
                    key={acc.id}
                    className={`acc2-card ${acc.active ? 'def' : ''} ${dragId === acc.id ? 'dragging' : ''}`}
                    onDragOver={(e) => onDragOver(e, acc)}
                    onDrop={(e) => {
                      e.preventDefault();
                      if (dragId && order) void saveOrder(order);
                      setDragId(null);
                    }}
                  >
                    {accounts.length > 1 && (
                      <span
                        className="acc2-drag"
                        draggable
                        title={tx('orderHint')}
                        onDragStart={(e) => {
                          setDragId(acc.id);
                          e.dataTransfer.effectAllowed = 'move';
                          const card = (e.currentTarget as HTMLElement).parentElement;
                          if (card) e.dataTransfer.setDragImage(card, 20, 20);
                        }}
                        onDragEnd={() => {
                          setDragId(null);
                          if (order) setOrder(null);
                        }}
                      >
                        <Icon name="grip" size={14} />
                      </span>
                    )}
                    <div className="acc2-av" style={{ background: avatarColor(acc.label) }}>
                      {(acc.label.trim()[0] ?? '?').toUpperCase()}
                    </div>
                    <div className="acc2-body">
                      <div className="acc2-nm">
                        <span className="acc2-label">{acc.label}</span>
                        {acc.active && <span className="acc-chip dark">{tx('activeBadge')}</span>}
                        {isSessionAcc && <span className="acc-chip ok">{tx('sessionCurrentBadge')}</span>}
                        <span className="acc-chip" title={mismatch ? tx('savedAsOauth') : undefined}>
                          {authChip(tool, acc)}
                        </span>
                        {acc.identity.plan &&
                          !mismatch &&
                          acc.identity.plan !== acc.actual?.provider &&
                          acc.identity.plan !== acc.identity.workspace &&
                          !/^https?:\/\//i.test(acc.identity.plan) && <span className="acc-chip blue">{acc.identity.plan}</span>}
                        <span className={`acc-chip ${st.cls}`}>{st.text}</span>
                      </div>
                      {sub.length > 0 && <div className="acc-sub">{sub.join(' · ')}</div>}
                      {mismatch && acc.actual && (
                        <div className="acc-mismatch">
                          <div className="acc-warn small">
                            {tx('codexMismatchWarn', { provider: acc.actual.provider, email: acc.identity.email ?? acc.label })}
                          </div>
                          <div className="acc-btn-row">
                            {acc.actual.method === 'providerApiKey' && acc.actual.envKey && (
                              <button className="btn xs primary" disabled={busy} onClick={() => void convert(acc, 'apiKey')}>
                                {tx('convertToApiKey')}
                              </button>
                            )}
                            <button className="btn xs" disabled={busy} onClick={() => void convert(acc, 'oauth')}>
                              {tx('convertToOauth')}
                            </button>
                          </div>
                        </div>
                      )}
                      {qVal && !mismatch && (
                        <div className="quota acc2-quota">
                          <QuotaBody q={qVal} />
                          {typeof qVal === 'object' && qVal.stale && <div className="muted small">{tx('quotaStale')}</div>}
                        </div>
                      )}
                      {foot.length > 0 && <div className="acc2-foot">{foot.join(' · ')}</div>}
                    </div>
                    <div className="acc2-acts">
                      {primary === 'relogin' && (
                        <button className="btn xs primary" disabled={busy || loginJob?.running} onClick={() => void startLogin(reloginMode, acc)}>
                          {tx('relogin')}
                        </button>
                      )}
                      {primary === 'session' && (
                        <button className="btn xs" disabled={busy} onClick={() => void switchSession(acc)}>
                          {tx('useForSession')}
                        </button>
                      )}
                      {primary === 'default' && (
                        <button className="btn xs" disabled={busy} onClick={() => void activate(acc)}>
                          {tx('setDefault')}
                        </button>
                      )}
                      <button className="icon-btn" title={tx('more')} onClick={(e) => cardMenu(e, acc, idx)}>
                        <Icon name="more" size={14} />
                      </button>
                    </div>
                  </div>
                );
              })
            )}
          </div>
        </div>

        {layer?.k === 'add' && <AddChooser tool={tool} options={options} onPick={pick} onClose={() => setLayer(null)} />}
        {layer?.k === 'login' && (
          <LoginDialog
            hostId={hostId}
            tool={tool}
            job={loginJob}
            loginKind={layer.loginKind}
            relogin={layer.relogin}
            summary={summary}
            isolation={isolation}
            onRetry={() => void startLogin(layer.mode, layer.relogin)}
            onClose={() => {
              setLayer(null);
              setLoginJob(null);
            }}
            onSaved={() => {
              setLayer(null);
              setLoginJob(null);
              void loadAccounts(tool);
              void loadToolQuotas(tool, false, false);
            }}
            onReload={reloadTool}
          />
        )}
        {layer?.k === 'terminal' && (
          <TerminalGuide
            tool={tool}
            summary={summary}
            onRecheck={() => loadAccounts(tool)}
            onCapture={() => void capture()}
            onClose={() => setLayer(null)}
            onOpenTerminal={() => {
              onClose();
              void openTerminal(hostId);
            }}
          />
        )}
        {layer?.k === 'drawer' && (
          <AccountDrawer
            hostId={hostId}
            tool={tool}
            target={{ acc: layer.acc }}
            onClose={() => setLayer(null)}
            onSaved={() => {
              setLayer(null);
              void loadAccounts(tool);
              void loadToolQuotas(tool, false, false);
            }}
          />
        )}
        {layer?.k === 'import' && (
          <ImportDialog
            hostId={hostId}
            onClose={() => setLayer(null)}
            onDone={(v) => {
              setLayer(null);
              setTool(v.tool);
              void loadAccounts(v.tool);
            }}
          />
        )}
        {layer?.k === 'export' && <ExportDialog text={layer.text} onClose={() => setLayer(null)} />}
      </div>
    </div>
  );
}
