import { useEffect, useState } from 'react';
import { useBack } from '../back';
import { tr } from '../i18n';
import { rpc } from '../transport';
import { TOOL_LABEL, type Quota, type SessionInfo } from '../types';
import { fmtTokens } from '../util';
import { Icon, ToolBadge } from './Icon';
import { errText, QuotaBody } from './StatsView';

/** "Usage" on the session page: how much this session used, and how much quota is left on this tool account */
export function UsageDialog({ hostId, info, onClose }: { hostId: string; info: SessionInfo; onClose: () => void }) {
  const [quota, setQuota] = useState<Quota | string | undefined>(undefined);
  useBack(true, onClose);
  const load = (refresh: boolean) => {
    setQuota(undefined);
    rpc<Quota>(hostId, 'tools.quota', { tool: info.tool, refresh })
      .then(setQuota)
      .catch((e) => setQuota(errText(e)));
  };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(() => load(false), [hostId, info.tool]);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  const u = info.usage;
  const total = u.inputTokens + u.outputTokens + u.cacheReadTokens + u.cacheWriteTokens;
  const rows: [string, string][] = [
    [tr('输入', 'Input'), fmtTokens(u.inputTokens)],
    [tr('输出', 'Output'), fmtTokens(u.outputTokens)],
    [tr('缓存读取', 'Cache read'), fmtTokens(u.cacheReadTokens)],
    [tr('缓存写入', 'Cache write'), fmtTokens(u.cacheWriteTokens)],
    [tr('合计', 'Total'), fmtTokens(total)],
  ];
  if (u.costUsd) rows.push([tr('费用', 'Cost'), `$${u.costUsd.toFixed(u.costUsd < 1 ? 3 : 2)}`]);
  if (u.credits) rows.push(['Credits', u.credits.toFixed(2)]);
  if (info.contextPercent != null) rows.push([tr('上下文', 'Context'), `${Math.round(info.contextPercent)}%`]);
  if (info.model) rows.push([tr('模型', 'Model'), info.model]);
  const noTokens = total === 0 && (info.tool === 'cursor' || info.mode === 'terminal');

  return (
    <div className="modal-bg" onClick={onClose}>
      <div className="modal usage-dlg" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <h3>{tr('用量', 'Usage')}</h3>
          <button className="icon-btn" onClick={onClose}>
            <Icon name="x" />
          </button>
        </div>
        <div className="card-title">{tr('这个会话', 'This session')}</div>
        {noTokens ? (
          <div className="muted small">
            {info.mode === 'terminal'
              ? tr('终端模式拿不到 token 用量，切到对话模式后的回合才会统计。', 'Terminal mode does not report token usage. Turns after switching to chat are counted.')
              : tr('Cursor 不回传单轮 token 用量，只能看下面的账号额度。', 'Cursor does not report per-turn tokens. Account quota is below.')}
          </div>
        ) : (
          <div className="usage-grid">
            {rows.map(([k, v]) => (
              <div key={k} className="usage-cell">
                <span className="muted small">{k}</span>
                <b className="truncate" title={v}>
                  {v}
                </b>
              </div>
            ))}
          </div>
        )}
        {info.tool !== 'shell' && (
          <>
            <div className="card-title row usage-quota-head">
              <ToolBadge tool={info.tool} size={16} />
              <span>{TOOL_LABEL[info.tool]} {tr('账号额度', 'quota')}</span>
              {typeof quota === 'object' && (quota.account?.plan || quota.account?.email) && (
                <span className="muted small">{[quota.account.plan, quota.account.email].filter(Boolean).join(' · ')}</span>
              )}
              <span className="grow" />
              <button className="btn small" disabled={quota === undefined} onClick={() => load(true)}>
                {tr('刷新', 'Refresh')}
              </button>
            </div>
            <div className="quota">
              <QuotaBody q={quota} />
              {typeof quota === 'object' && quota.windows.length === 0 && !quota.note && (
                <div className="muted small">{tr('这个工具没有报告额度信息', 'This tool did not report quota')}</div>
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
