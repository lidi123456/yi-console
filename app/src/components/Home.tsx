import { tr } from '../i18n';
import { navigate, setState, useStore } from '../store';
import type { SessionInfo } from '../types';
import { fmtUsage, shortPath, STATUS_LABEL, timeAgo } from '../util';
import { Icon, ToolBadge } from './Icon';
import { sessionStatusClass } from './Sidebar';

export function Home() {
  const hosts = useStore((s) => s.hosts);
  const sessions = useStore((s) => s.sessions);
  const all: { hostId: string; hostName: string; s: SessionInfo }[] = [];
  for (const h of hosts) for (const s of sessions[h.config.id] ?? []) if (!s.parentId) all.push({ hostId: h.config.id, hostName: h.config.name, s });
  const attn = all.filter((x) => x.s.pendingPermissions > 0);
  const active = all.filter((x) => ['running', 'starting'].includes(x.s.status) && x.s.pendingPermissions === 0);
  const recent = all
    .filter((x) => !attn.includes(x) && !active.includes(x))
    .sort((a, b) => b.s.updatedAt - a.s.updatedAt)
    .slice(0, 12);
  const connected = hosts.filter((h) => h.status.status === 'connected');

  const Row = ({ x }: { x: (typeof all)[number] }) => (
    <button className="list-row" onClick={() => navigate({ kind: 'session', hostId: x.hostId, sessionId: x.s.id })}>
      <ToolBadge tool={x.s.tool} size={22} />
      <div className="grow min0">
        <div className="truncate">{x.s.title || tr('新会话', 'New Session')}</div>
        <div className="muted small truncate">
          {x.hostName} · {shortPath(x.s.cwd, hosts.find((h) => h.config.id === x.hostId)?.status.hello?.home)} · {timeAgo(x.s.updatedAt)}
        </div>
      </div>
      <span className="muted small hide-mobile">{fmtUsage(x.s.usage)}</span>
      {/* Files sessions have no process, so no status */}
      {x.s.tool !== 'files' && (
        <>
          <span className={`status-chip st-${x.s.status}`}>
            {x.s.pendingPermissions > 0 ? tr(`${x.s.pendingPermissions} 个待审批`, `${x.s.pendingPermissions} pending`) : STATUS_LABEL[x.s.status]}
          </span>
          <span className={`sdot sdot-${sessionStatusClass(x.s)}`} />
        </>
      )}
    </button>
  );

  return (
    <div className="page">
      <div className="page-head">
        <h2>{tr('总览', 'Overview')}</h2>
        {connected.length > 0 && (
          <button className="btn primary" onClick={() => setState({ newSession: { hostId: connected[0].config.id } })}>
            <Icon name="plus" size={14} /> {tr('新建会话', 'New Session')}
          </button>
        )}
      </div>
      {hosts.length === 0 && <div className="muted">{tr('加载中…', 'Loading…')}</div>}
      {hosts.length > 0 && connected.length === 0 && (
        <div className="card">
          <div className="card-title">{tr('还没有连上任何主机', 'No hosts connected')}</div>
          <p className="muted small">{tr('添加一台 SSH 主机，或者扫码配对一台通过中继连接的电脑。', 'Add an SSH host, or pair a computer connected via relay.')}</p>
          <button className="btn primary" onClick={() => navigate({ kind: 'hosts' })}>
            {tr('打开主机管理', 'Manage Hosts')}
          </button>
        </div>
      )}
      {attn.length > 0 && (
        <section>
          <h4 className="section-title attn-text">
            <Icon name="shield" size={14} /> {tr('等待你审批', 'Waiting for Approval')}
          </h4>
          <div className="list card-list">
            {attn.map((x) => (
              <Row key={x.s.id} x={x} />
            ))}
          </div>
        </section>
      )}
      {active.length > 0 && (
        <section>
          <h4 className="section-title">{tr('运行中', 'Running')}</h4>
          <div className="list card-list">
            {active.map((x) => (
              <Row key={x.s.id} x={x} />
            ))}
          </div>
        </section>
      )}
      {recent.length > 0 && (
        <section>
          <h4 className="section-title">{tr('最近', 'Recent')}</h4>
          <div className="list card-list">
            {recent.map((x) => (
              <Row key={x.s.id} x={x} />
            ))}
          </div>
        </section>
      )}
    </div>
  );
}
