import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { getT, useLang } from '../i18n';
import { onAccountsUpdated, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type ToolInfo, type ToolKind, type ToolQuotasGroup, type ToolsQuotasResponse, type Usage } from '../types';
import { fmtTokens } from '../util';
import { openAccountManager } from './AccountManagerModal';
import { ToolBadge } from './Icon';
import { QuotaBody, quotaErrText } from './QuotaBody';

export { QuotaBody };
export const errText = quotaErrText;

const tx = getT('statsView');

interface Stats {
  daily: { day: string; tool: string; usage: Usage }[];
  totals: Record<string, Usage>;
  /** Tokens per model for each tool (from the tool's own logs) */
  models?: Record<string, Record<string, number>>;
  /** Which tools' tokens come from local logs (older daemons don't send this field) */
  fromLogs?: string[];
}

export const TOOL_VAR: Record<string, string> = {
  claude: 'var(--c-claude)',
  codex: 'var(--c-codex)',
  cursor: 'var(--c-cursor)',
  kiro: 'var(--c-kiro)',
  qoder: 'var(--c-qoder)',
  codebuddy: 'var(--c-codebuddy)',
  antigravity: 'var(--c-antigravity)',
  shell: 'var(--c-shell)',
};

const QUOTA_TOOLS: ToolKind[] = ['claude', 'codex', 'cursor', 'kiro', 'qoder', 'codebuddy', 'antigravity'];

export function quotaToolsFor(tools: readonly ToolInfo[] | undefined): ToolKind[] {
  return QUOTA_TOOLS.filter((tool) => tools?.some((t) => t.kind === tool && (t.available || (t.accountCount ?? 0) > 0)));
}

interface ToolQuotaState {
  group?: ToolQuotasGroup;
  error?: string;
  loading: boolean;
}

/** Account quotas across all configured accounts per tool */
export function QuotaCard({ hostId }: { hostId: string }) {
  useLang();
  const toolNames = useStore((s) => quotaToolsFor(s.hosts.find((h) => h.config.id === hostId)?.status.hello?.tools).join(','));
  const tools = useMemo(() => (toolNames ? toolNames.split(',') as ToolKind[] : []), [toolNames]);
  const [byTool, setByTool] = useState<Partial<Record<ToolKind, ToolQuotaState>>>({});
  const pending = useRef(new Map<ToolKind, { reload: boolean }>());

  const loadTool = useCallback(async function loadTool(tool: ToolKind, refresh: boolean, refreshInactive = false) {
    const requests = pending.current;
    if (requests.has(tool)) return;
    const request = { reload: false };
    requests.set(tool, request);
    setByTool((prev) => ({ ...prev, [tool]: { group: prev[tool]?.group, loading: true } }));
    let group: ToolQuotasGroup | undefined;
    let error: string | undefined;
    try {
      const result = await rpc<ToolsQuotasResponse>(hostId, 'tools.quotas', { tool, refresh, refreshInactive });
      group = result.tools?.find((g) => g.tool === tool);
    } catch (e) {
      error = String((e as Error)?.message ?? e);
    }
    if (requests.get(tool) !== request) return;
    requests.delete(tool);
    // An account change can invalidate the result and cache of an in-flight probe.
    if (request.reload) {
      void loadTool(tool, true);
      return;
    }
    setByTool((prev) => ({ ...prev, [tool]: { group: error === undefined ? group : prev[tool]?.group, error, loading: false } }));
  }, [hostId]);

  useEffect(() => {
    const requests = pending.current;
    return () => requests.clear();
  }, [hostId]);

  useEffect(() => {
    for (const tool of tools) void loadTool(tool, false);
    return onAccountsUpdated((h, changedTool) => {
      if (h !== hostId) return;
      for (const tool of tools) {
        if (changedTool && tool !== changedTool) continue;
        const request = pending.current.get(tool);
        if (request) request.reload = true;
        else void loadTool(tool, false);
      }
    });
  }, [hostId, loadTool, tools]);

  if (!tools.length) return null;
  const hasInactiveHost = tools.some((tool) => {
    const group = byTool[tool]?.group;
    return group?.oauthIsolation === 'hostActive' && group.accounts.length > 1;
  });
  const isLoading = (tool: ToolKind) => byTool[tool]?.loading ?? true;
  const allLoading = tools.every(isLoading);
  const refresh = (refreshInactive: boolean) => {
    for (const tool of tools) void loadTool(tool, true, refreshInactive);
  };
  return (
    <div className="card">
      <div className="card-title row" style={{ gap: 6 }}>
        <span>{tx('accountQuota')}</span>
        <span className="grow" />
        {hasInactiveHost && (
          <button className="btn small" disabled={tools.some(isLoading)} onClick={() => refresh(true)}>
            {tx('probeAll')}
          </button>
        )}
        <button className="btn small" disabled={allLoading} onClick={() => refresh(false)}>
          {allLoading ? tx('loading') : tx('refresh')}
        </button>
      </div>
      <div className="quota-list">
        {tools.map((tool) => <ToolQuotaCard key={tool} hostId={hostId} tool={tool} state={byTool[tool]} />)}
      </div>
    </div>
  );
}

function ToolQuotaCard({ hostId, tool, state }: { hostId: string; tool: ToolKind; state?: ToolQuotaState }) {
  const { group, error, loading = true } = state ?? {};
  const native = group?.nativeQuota;
  const failure = error ?? group?.error;
  return (
    <div className="quota" aria-busy={loading}>
      <div className="row" style={{ gap: 6 }}>
        <ToolBadge tool={tool} size={18} />
        <b>{TOOL_LABEL[tool]}</b>
        <span className="grow" />
        <button className="link-btn small" onClick={() => openAccountManager({ hostId, tool })}>
          {tx('manageAccounts')}
        </button>
      </div>
      {loading && <QuotaBody q={undefined} />}
      {failure != null && <QuotaBody q={quotaErrText(failure)} />}
      {group?.accounts.length ? (
        <div className="quota-accounts">
          {group.accounts.map((acc) => (
            <div key={acc.accountId} className={`quota-acc ${acc.active ? 'active' : ''}`}>
              <div className="row small" style={{ gap: 6, flexWrap: 'wrap' }}>
                <strong>{acc.label}</strong>
                <span className="acc-badge">{acc.authKind === 'apiKey' ? 'API' : 'OAuth'}</span>
                {acc.active && <span className="acc-badge acc-badge-accent">{tx('activeAccount')}</span>}
                {acc.quota?.account?.email && <span className="muted">{acc.quota.account.email}</span>}
              </div>
              <QuotaBody q={acc.error ? quotaErrText(acc.error) : acc.quota ?? null} />
            </div>
          ))}
        </div>
      ) : (
        <>
          {native && (native.account?.plan || native.account?.email) && (
            <div className="muted small">{[native.account.plan, native.account.email].filter(Boolean).join(' · ')}</div>
          )}
          {native && <QuotaBody q={native} />}
          {!loading && failure == null && !native && <QuotaBody q={null} />}
        </>
      )}
    </div>
  );
}

const tokensOf = (u: Usage) => u.inputTokens + u.outputTokens + u.cacheReadTokens + u.cacheWriteTokens;

export function StatsView({ hostId }: { hostId: string }) {
  const [days, setDays] = useState(30);
  const [metric, setMetric] = useState<'tokens' | 'credits' | 'cost'>('tokens');
  const [data, setData] = useState<Stats | null>(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(true);
  const [hover, setHover] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setLoading(true);
    setError('');
    rpc<Stats>(hostId, 'stats.usage', { days, tzOffset: -new Date().getTimezoneOffset() })
      .then((d) => live && setData(d))
      .catch((e) => live && setError(errText(e)))
      .finally(() => live && setLoading(false));
    return () => {
      live = false;
    };
  }, [hostId, days]);

  const val = (u: Usage) => (metric === 'tokens' ? tokensOf(u) : metric === 'credits' ? u.credits : u.costUsd);

  const chart = useMemo(() => {
    const dayList: string[] = [];
    for (let i = days - 1; i >= 0; i--) {
      const d = new Date(Date.now() - i * 86400000);
      dayList.push(`${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`);
    }
    const by: Record<string, Record<string, number>> = {};
    for (const r of data?.daily ?? []) {
      (by[r.day] ??= {})[r.tool] = val(r.usage);
    }
    const max = Math.max(1e-9, ...dayList.map((d) => Object.values(by[d] ?? {}).reduce((a, b) => a + b, 0)));
    return { dayList, by, max };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [data, days, metric]);

  const tools = Object.keys(data?.totals ?? {}) as ToolKind[];
  const fmt = (v: number) => (metric === 'tokens' ? fmtTokens(v) : metric === 'credits' ? `${v.toFixed(2)} credits` : `$${v.toFixed(2)}`);
  const W = 640,
    H = 180,
    bw = W / chart.dayList.length;

  return (
    <div className="page">
      <div className="page-head">
        <h2>{tx('usage')}</h2>
        <div className="row">
          <div className="seg">
            {[7, 30, 90].map((d) => (
              <button key={d} className={days === d ? 'active' : ''} onClick={() => setDays(d)}>
                {tx('d', { d })}
              </button>
            ))}
          </div>
          <div className="seg">
            <button className={metric === 'tokens' ? 'active' : ''} onClick={() => setMetric('tokens')}>
              Tokens
            </button>
            <button className={metric === 'credits' ? 'active' : ''} onClick={() => setMetric('credits')}>
              {tx('credits')}
            </button>
            <button className={metric === 'cost' ? 'active' : ''} onClick={() => setMetric('cost')}>
              {tx('cost')}
            </button>
          </div>
        </div>
      </div>
      <QuotaCard hostId={hostId} />
      <p className="muted small">
        {data?.fromLogs
          ? tx('claudeCodeCodexQoderAnd')
          : tx('onlyStructuredYiConsoleSessions')}
      </p>
      {data && !data.fromLogs && (
        <div className="muted small">{tx('thisHostSDaemonIs2')}</div>
      )}
      <div className="stat-tiles">
        {loading && !data && <div className="muted">{tx('calculatingTheFirstReadOf')}</div>}
        {error && <div className="muted">{tx('failed')}{error}</div>}
        {!loading && !error && tools.length === 0 && <div className="muted">{tx('noUsageInThisRange')}</div>}
        {tools.map((t) => {
          const u = data!.totals[t];
          const tok = tokensOf(u);
          return (
            <div key={t} className="tile">
              <div className="row">
                <ToolBadge tool={t} size={18} /> {TOOL_LABEL[t] ?? t}
              </div>
              {tok > 0 || u.credits <= 0 ? (
                <>
                  <div className="tile-num">{fmtTokens(tok)}</div>
                  <div className="muted small">
                    {tx('in')} {fmtTokens(u.inputTokens)} · {tx('out')} {fmtTokens(u.outputTokens)} · {tx('cache')} {fmtTokens(u.cacheReadTokens)}
                  </div>
                  {u.costUsd > 0 && <div className="small">{tx('cost')} ${u.costUsd.toFixed(2)}</div>}
                  {u.credits > 0 && <div className="small">{u.credits.toFixed(2)} credits</div>}
                </>
              ) : (
                <>
                  <div className="tile-num">{u.credits.toFixed(2)} credits</div>
                  <div className="muted small">{tx('billedInCreditsNoTokens')}</div>
                </>
              )}
              {Object.entries(data!.models?.[t] ?? {})
                .sort((a, b) => b[1] - a[1])
                .slice(0, 3)
                .map(([m, n]) => (
                  <div key={m} className="muted small truncate" title={m}>
                    {m} · {tok > 0 ? fmtTokens(n) : `${n.toFixed(2)} credits`}
                  </div>
                ))}
            </div>
          );
        })}
      </div>
      {tools.length > 0 && (
        <div className="card chart-card">
          <svg viewBox={`0 0 ${W} ${H + 22}`} className="chart" role="img" aria-label={tx('dailyUsage')}>
            {[0.25, 0.5, 0.75, 1].map((f) => (
              <line key={f} x1={0} x2={W} y1={H - H * f} y2={H - H * f} className="grid" />
            ))}
            {chart.dayList.map((d, i) => {
              let y = H;
              const segs = tools.map((t) => {
                const v = chart.by[d]?.[t] ?? 0;
                const h = (v / chart.max) * (H - 6);
                y -= h;
                return v > 0 ? (
                  <rect key={t} x={i * bw + bw * 0.15} y={y} width={bw * 0.7} height={Math.max(h - 1, 0.5)} fill={TOOL_VAR[t]} rx={1.5} />
                ) : null;
              });
              return (
                <g key={d} onMouseEnter={() => setHover(d)} onMouseLeave={() => setHover(null)}>
                  <rect x={i * bw} y={0} width={bw} height={H} fill="transparent" />
                  {segs}
                  {(chart.dayList.length - 1 - i) % Math.ceil(chart.dayList.length / 8) === 0 && (
                    <text
                      x={i === 0 ? 0 : i === chart.dayList.length - 1 ? (i + 1) * bw : i * bw + bw / 2}
                      y={H + 15}
                      className="axis"
                      textAnchor={i === 0 ? 'start' : i === chart.dayList.length - 1 ? 'end' : 'middle'}
                    >
                      {d.slice(5)}
                    </text>
                  )}
                </g>
              );
            })}
          </svg>
          <div className="chart-foot small">
            {hover ? (
              <>
                <b>{hover}</b>{' '}
                {tools.map((t) => (
                  <span key={t} className="legend">
                    <i style={{ background: TOOL_VAR[t] }} /> {TOOL_LABEL[t] ?? t} {fmt(chart.by[hover]?.[t] ?? 0)}
                  </span>
                ))}
              </>
            ) : (
              tools.map((t) => (
                <span key={t} className="legend">
                  <i style={{ background: TOOL_VAR[t] }} /> {TOOL_LABEL[t] ?? t}
                </span>
              ))
            )}
          </div>
        </div>
      )}
    </div>
  );
}
