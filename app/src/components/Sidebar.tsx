import { useMemo, useState } from 'react';
import { isEn, tr } from '../i18n';
import { hubCall, rpc } from '../transport';
import { errorToast, navigate, setState, toast, useStore, type HostEntry } from '../store';
import type { SessionInfo } from '../types';
import { basename, fuzzyScore, lsGet, lsSet } from '../util';
import { openMenu, type MenuItem } from './Overlay';
import { writeClipboard } from '../clipboard';
import { deleteDirSessions, openTerminal, renameSession, sessionMenu } from './SessionActions';
import { Icon, ToolBadge } from './Icon';
import { MOD } from './Palette';

const SIDE_MIN = 244;
const SIDE_MAX = 480;
const clampSide = (w: number) => Math.round(Math.max(SIDE_MIN, Math.min(SIDE_MAX, Number(w) || SIDE_MIN)));

function StatusDot({ s }: { s?: string }) {
  return <span className={`dot dot-${s ?? 'disconnected'}`} />;
}

function hostIcon(kind: string) {
  return kind === 'local' ? 'laptop' : kind === 'relay' || kind === 'proxy' ? 'cloud' : 'server';
}

export function sessionStatusClass(s: SessionInfo) {
  if (s.pendingPermissions > 0 || s.status === 'waiting-permission') return 'attn';
  if (s.status === 'running' || s.status === 'starting') return 'busy';
  if (s.status === 'error') return 'err';
  if (s.status === 'exited') return 'off';
  return 'idle';
}

function hostStatusLabel(s?: string) {
  switch (s) {
    case 'connected':
      return tr('已连接', 'Connected');
    case 'connecting':
      return tr('连接中', 'Connecting');
    case 'error':
      return tr('连接失败', 'Connection failed');
    default:
      return tr('未连接', 'Disconnected');
  }
}

function statusText(cls: string) {
  switch (cls) {
    case 'attn':
      return tr('等待审批', 'Waiting for approval');
    case 'busy':
      return tr('运行中', 'Running');
    case 'err':
      return tr('出错', 'Error');
    case 'off':
      return tr('已退出', 'Exited');
    default:
      return tr('活动中，等待输入', 'Active, waiting for input');
  }
}

function pin(hostId: string, s: SessionInfo) {
  rpc(hostId, 'session.pin', { sessionId: s.id, pinned: !s.pinned }).catch(errorToast);
}

/** Time on the right of the sidebar: just now / 5 min / 3 h / yesterday / 4 days / 10/12 */
function shortAgo(ms: number): string {
  if (!ms) return '';
  const d = (Date.now() - ms) / 1000;
  if (d < 60) return tr('刚刚', 'now');
  if (d < 3600) return isEn() ? `${Math.floor(d / 60)}m` : `${Math.floor(d / 60)} 分钟`;
  if (d < 86400) return isEn() ? `${Math.floor(d / 3600)}h` : `${Math.floor(d / 3600)} 小时`;
  if (d < 2 * 86400) return tr('昨天', 'Yesterday');
  if (d < 7 * 86400) return isEn() ? `${Math.floor(d / 86400)}d` : `${Math.floor(d / 86400)} 天`;
  const t = new Date(ms);
  return `${t.getMonth() + 1}/${t.getDate()}`;
}

function SessionRow({ hostId, s, active, showDir }: { hostId: string; s: SessionInfo; active: boolean; showDir?: boolean }) {
  const cls = sessionStatusClass(s);
  // Only show states that need attention: awaiting approval, running, error; otherwise show the time
  const status =
    s.pendingPermissions > 0 ? (
      <span className="pill pill-attn">{s.pendingPermissions}</span>
    ) : cls === 'attn' || cls === 'busy' || cls === 'err' ? (
      <span className={`sdot sdot-${cls}`} />
    ) : (
      <span className="row-time">{shortAgo(s.updatedAt)}</span>
    );
  return (
    <div className={`session-row-wrap ${active ? 'active' : ''}`}>
      <button
        className={`session-row s-${cls} ${active ? 'active' : ''}`}
        onClick={() => navigate({ kind: 'session', hostId, sessionId: s.id })}
        onContextMenu={(e) => openMenu(e, sessionMenu(hostId, s))}
        onKeyDown={(e) => {
          if (e.key === 'F2') {
            e.preventDefault();
            renameSession(hostId, s);
          }
        }}
        title={`${s.title || tr('新会话', 'New Session')}\n${s.cwd}\n${statusText(cls)}${s.mode === 'terminal' ? tr(' · 终端模式', ' · Terminal mode') : ''}${s.mode === 'terminal' && cls !== 'off' && cls !== 'err' ? tr('\n进程不会自动结束，不用时请停止进程', '\nNot stopped automatically; stop the process when done') : ''}`}
      >
        <span className="row-badge">
          <ToolBadge tool={s.tool} size={16} />
          {s.mode === 'terminal' && s.tool !== 'shell' && (
            <span className="row-badge-term">
              <Icon name="terminal" size={9} />
            </span>
          )}
        </span>
        <span className="session-title">
          {s.title || tr('新会话', 'New Session')}
          {showDir && <span className="row-dir"> · {basename(s.cwd)}</span>}
        </span>
        {(s.queued?.length ?? 0) > 0 && <span className="pill" title={tr('排队中的消息', 'Queued messages')}>{s.queued!.length}</span>}
        <span className="row-end">{status}</span>
      </button>
      <div className="row-actions">
        <button className={`row-act ${s.pinned ? 'on' : ''}`} title={s.pinned ? tr('取消置顶', 'Unpin') : tr('置顶', 'Pin')} onClick={() => pin(hostId, s)}>
          <Icon name="pin" size={13} />
        </button>
        <button
          className="row-act"
          title={tr('更多', 'More')}
          onClick={(e) => {
            const r = e.currentTarget.getBoundingClientRect();
            openMenu({ clientX: r.left, clientY: r.bottom + 2 }, sessionMenu(hostId, s));
          }}
        >
          <Icon name="more" size={14} />
        </button>
      </div>
    </div>
  );
}

/** Sessions idle longer than this and not running go under "Earlier" */
const STALE_MS = 3 * 86400_000;

/** Context menu for directory rows in the sidebar */
function dirMenu(hostId: string, cwd: string, connected: boolean): (MenuItem | '-')[] {
  return [
    { label: tr('搜索此目录的历史会话', 'Search History in Directory'), icon: 'history', disabled: !connected, run: () => navigate({ kind: 'history', hostId, cwd }) },
    { label: tr('在此目录新建会话', 'New Session in Directory'), icon: 'plus', disabled: !connected, run: () => setState({ newSession: { hostId, cwd } }) },
    { label: tr('浏览目录', 'Browse Directory'), icon: 'folder', disabled: !connected, run: () => navigate({ kind: 'files', hostId, path: cwd }) },
    { label: tr('在此目录打开终端', 'Open Terminal in Directory'), icon: 'terminal', disabled: !connected, run: () => openTerminal(hostId, cwd) },
    { label: tr('复制路径', 'Copy Path'), icon: 'copy', run: () => writeClipboard(cwd).then(() => toast(tr('已复制路径', 'Path copied'))) },
    '-',
    { label: tr('删除此目录的所有会话…', 'Delete All Sessions in Directory…'), icon: 'trash', danger: true, disabled: !connected, run: () => deleteDirSessions(hostId, cwd) },
  ];
}

/** The "…" menu and context menu of host rows: the buttons that used to sit under the host are all here now */
function hostMenu(h: HostEntry, connect: () => void): (MenuItem | '-')[] {
  const id = h.config.id;
  const connected = h.status.status === 'connected';
  if (!connected) {
    return [
      { label: tr('连接', 'Connect'), icon: 'power', run: connect },
      { label: tr('编辑主机', 'Edit Host'), icon: 'edit', run: () => navigate({ kind: 'hosts', editId: id }) },
    ];
  }
  return [
    { label: tr('新建会话', 'New Session'), icon: 'plus', hint: `${MOD}+N`, run: () => setState({ newSession: { hostId: id } }) },
    { label: tr('历史会话', 'Session History'), icon: 'history', run: () => navigate({ kind: 'history', hostId: id }) },
    { label: tr('浏览文件', 'Browse Files'), icon: 'folder', run: () => navigate({ kind: 'files', hostId: id }) },
    { label: tr('打开终端', 'Open Terminal'), icon: 'terminal', run: () => openTerminal(id) },
    { label: tr('用量统计', 'Usage Stats'), icon: 'chart', run: () => navigate({ kind: 'stats', hostId: id }) },
    { label: tr('端口转发', 'Port Forwarding'), icon: 'external', run: () => navigate({ kind: 'ports', hostId: id }) },
    '-',
    { label: tr('主机设置', 'Host Settings'), icon: 'settings', run: () => navigate({ kind: 'settings', hostId: id }) },
    { label: tr('编辑连接', 'Edit Connection'), icon: 'edit', run: () => navigate({ kind: 'hosts', editId: id }) },
    ...(h.config.kind !== 'local'
      ? [{ label: tr('断开连接', 'Disconnect'), icon: 'power', run: () => hubCall('hosts.disconnect', { id }).catch(errorToast) }]
      : []),
  ];
}

function HostBlock({ h, filter }: { h: HostEntry; filter: string }) {
  const id = h.config.id;
  const sessions = useStore((s) => s.sessions[id]);
  const view = useStore((s) => s.view);
  const [collapsed, setCollapsed] = useState<boolean>(() => lsGet(`collapse-${id}`, false));
  const [showOld, setShowOld] = useState(false);
  // Collapsed project directories, remembered locally per host
  const [folded, setFolded] = useState<string[]>(() => lsGet(`fold-dirs-${id}`, []));
  const fold = (cwd: string) => {
    const next = folded.includes(cwd) ? folded.filter((x) => x !== cwd) : [...folded, cwd];
    lsSet(`fold-dirs-${id}`, next);
    setFolded(next);
  };
  const connected = h.status.status === 'connected';

  const q = filter.trim();
  const { pinned, groups, old, matched } = useMemo(() => {
    const all = (sessions ?? []).filter((s) => !s.parentId && (!q || fuzzyScore(`${s.title} ${s.cwd}`, q) >= 0));
    const now = Date.now();
    const pinned = all.filter((s) => s.pinned);
    const rest = all.filter((s) => !s.pinned);
    // Don't collapse while searching
    const isOld = (s: SessionInfo) => !q && (s.status === 'exited' || s.status === 'error') && now - s.updatedAt > STALE_MS;
    const m = new Map<string, SessionInfo[]>();
    for (const s of rest.filter((x) => !isOld(x))) {
      if (!m.has(s.cwd)) m.set(s.cwd, []);
      m.get(s.cwd)!.push(s);
    }
    return { pinned, groups: [...m.entries()], old: rest.filter(isOld), matched: all.length };
  }, [sessions, q]);

  const total = (sessions ?? []).filter((s) => !s.parentId).length;
  const hostCls = (sessions ?? []).filter((s) => !s.parentId).map(sessionStatusClass);
  const hostSum = hostCls.includes('attn') ? 'attn' : hostCls.includes('busy') ? 'busy' : null;

  const toggle = () => {
    lsSet(`collapse-${id}`, !collapsed);
    setCollapsed(!collapsed);
  };

  const connect = async () => {
    try {
      await hubCall('hosts.connect', { id });
    } catch (e) {
      errorToast(e);
      navigate({ kind: 'hosts', editId: id });
    }
  };

  if (q && matched === 0) return null;
  const isActive = (s: SessionInfo) => view.kind === 'session' && view.hostId === id && view.sessionId === s.id;
  return (
    <div className="host-block">
      <div
        className={`host-head ${connected ? '' : 'off'}`}
        onContextMenu={(e) => openMenu(e, hostMenu(h, connect))}
      >
        <button
          className="host-name"
          onClick={toggle}
          title={
            isEn()
              ? `${h.config.name}\n${hostStatusLabel(h.status.status)}\nClick to ${collapsed ? 'expand' : 'collapse'}, right-click for more`
              : `${h.config.name}\n${hostStatusLabel(h.status.status)}\n点击${collapsed ? '展开' : '收起'}，右键更多操作`
          }
        >
          <span className="host-icon">
            <Icon name={hostIcon(h.config.kind)} size={13} />
            <StatusDot s={h.status.status} />
          </span>
          <span className="truncate">{h.config.name}</span>
          {collapsed && !q && (
            <span className="project-count">
              {total}
              {hostSum && <span className={`sdot sdot-${hostSum}`} />}
            </span>
          )}
          <span className={`chev ${collapsed ? '' : 'open'}`}>
            <Icon name="chevron" size={12} />
          </span>
        </button>
        {connected ? (
          <button className="host-act" title={tr('新建会话', 'New Session')} onClick={() => setState({ newSession: { hostId: id } })}>
            <Icon name="plus" size={15} />
          </button>
        ) : (
          <button className="host-connect" onClick={connect}>
            {tr('连接', 'Connect')}
          </button>
        )}
        <button
          className="host-act"
          title={tr('更多', 'More')}
          onClick={(e) => {
            const r = e.currentTarget.getBoundingClientRect();
            openMenu({ clientX: r.left, clientY: r.bottom + 2 }, hostMenu(h, connect));
          }}
        >
          <Icon name="more" size={15} />
        </button>
      </div>
      {(!collapsed || q) && (
        <div className="host-body">
          {!connected && h.status.status === 'error' && (
            <div className="host-error" onClick={() => navigate({ kind: 'hosts', editId: id })}>
              {h.status.error?.replace(/^[A-Z_]+:/, '')}
            </div>
          )}
          {pinned.length > 0 && (
            <div className="project">
              <div className="project-name">{tr('置顶', 'Pinned')}</div>
              {pinned.map((s) => (
                <SessionRow key={s.id} hostId={id} s={s} active={isActive(s)} showDir />
              ))}
            </div>
          )}
          {groups.map(([cwd, list]) => {
            // Expand everything while searching; when collapsed, still show the currently open session
            const isFolded = !q && folded.includes(cwd);
            const rows = isFolded ? list.filter(isActive) : list;
            const cls = list.map(sessionStatusClass);
            const sum = cls.includes('attn') ? 'attn' : cls.includes('busy') ? 'busy' : null;
            return (
              <div className="project" key={cwd}>
                <div className="project-head">
                <button
                  className={`project-add ${connected ? '' : 'hidden'}`}
                  title={tr(`在 ${basename(cwd)} 新建会话`, `New session in ${basename(cwd)}`)}
                  onClick={() => setState({ newSession: { hostId: id, cwd } })}
                >
                  <Icon name="plus" size={13} />
                </button>
                <button
                  className="project-name project-toggle"
                  title={
                    isEn()
                      ? `${cwd}\nClick to ${isFolded ? 'expand' : 'collapse'}, right-click for more`
                      : `${cwd}\n点击${isFolded ? '展开' : '折叠'}，右键更多操作`
                  }
                  onClick={() => fold(cwd)}
                  onContextMenu={(e) => openMenu(e, dirMenu(id, cwd, connected))}
                >
                  <span className="truncate">{basename(cwd)}</span>
                  <span className={`chev ${isFolded ? '' : 'open'}`}>
                    <Icon name="chevron" size={11} />
                  </span>
                  {isFolded && (
                    <span className="project-count">
                      {list.length}
                      {sum && (
                        <span
                          className={`sdot sdot-${sum}`}
                          title={sum === 'attn' ? tr('有会话等待审批', 'Session waiting for approval') : tr('有会话在运行', 'Session running')}
                        />
                      )}
                    </span>
                  )}
                </button>
                </div>
                {rows.map((s) => (
                  <SessionRow key={s.id} hostId={id} s={s} active={isActive(s)} />
                ))}
              </div>
            );
          })}
          {old.length > 0 && (
            <div className="project">
              <button className="project-name old-toggle" onClick={() => setShowOld(!showOld)}>
                {tr(`更早的 ${old.length} 个会话`, `${old.length} older session(s)`)}
                <span className={`chev ${showOld ? 'open' : ''}`}>
                  <Icon name="chevron" size={11} />
                </span>
              </button>
              {showOld && old.map((s) => <SessionRow key={s.id} hostId={id} s={s} active={isActive(s)} showDir />)}
            </div>
          )}
          {connected && sessions && !sessions.some((s) => !s.parentId) && (
            <div className="muted small pad-x">{tr('还没有会话，点 + 新建，或从历史里继续', 'No sessions yet. Click + to create one or resume from history.')}</div>
          )}
        </div>
      )}
    </div>
  );
}

export function Sidebar() {
  const hosts = useStore((s) => s.hosts);
  const view = useStore((s) => s.view);
  const open = useStore((s) => s.sidebarOpen);
  const transport = useStore((s) => s.transport);
  const firstConnected = hosts.find((h) => h.status.status === 'connected')?.config.id;
  const [filter, setFilter] = useState('');
  // The filter box is also the entry to message search: Enter or clicking the row below goes to "Search messages"
  const searchMessages = () => navigate({ kind: 'search', q: filter.trim() || undefined });
  // Width is adjustable by dragging the right edge: default is also the minimum 244px, max 480px, double-click the edge to reset; remembered per device
  const [sideW, setSideW] = useState(() => clampSide(lsGet<number>('side-w', SIDE_MIN)));
  const dragSide = (e: React.PointerEvent) => {
    e.preventDefault();
    const x0 = e.clientX;
    const w0 = sideW;
    let w = w0;
    const move = (ev: PointerEvent) => {
      w = clampSide(w0 + ev.clientX - x0);
      setSideW(w);
    };
    const up = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
      document.body.classList.remove('resizing');
      lsSet('side-w', w);
    };
    document.body.classList.add('resizing');
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  };
  return (
    <>
      <div className={`scrim ${open ? 'show' : ''}`} onClick={() => setState({ sidebarOpen: false })} />
      <aside className={`sidebar ${open ? 'open' : ''}`} style={{ '--side-w': `${sideW}px` } as React.CSSProperties}>
        <div
          className="side-resizer hide-mobile"
          title={tr('拖动调整宽度，双击还原', 'Drag to resize, double-click to reset')}
          onPointerDown={dragSide}
          onDoubleClick={() => (setSideW(SIDE_MIN), lsSet('side-w', SIDE_MIN))}
        />
        <div className="side-top">
          <button
            className="nav-item"
            disabled={!firstConnected}
            title={firstConnected ? '' : tr('先连上一台主机', 'Connect to a host first')}
            onClick={() => firstConnected && setState({ newSession: { hostId: firstConnected } })}
          >
            <Icon name="edit" size={15} /> {tr('新的会话', 'New Session')}
          </button>
          <button className={`nav-item ${view.kind === 'home' ? 'active' : ''}`} onClick={() => navigate({ kind: 'home' })}>
            <Icon name="home" size={15} /> {tr('总览', 'Overview')}
          </button>
        </div>
        <div className="side-search">
          <Icon name="search" size={14} />
          <input
            value={filter}
            placeholder={tr('搜索会话和消息', 'Search sessions & messages')}
            onChange={(e) => setFilter(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Escape') setFilter('');
              if (e.key === 'Enter' && filter.trim()) searchMessages();
            }}
          />
          {filter ? (
            <button className="icon-btn" title={tr('清除', 'Clear')} onClick={() => setFilter('')}>
              <Icon name="x" size={13} />
            </button>
          ) : (
            <button
              className="kbd-btn hide-mobile"
              title={tr('命令面板：跳到任意会话、执行操作', 'Command Palette: jump to session or run action')}
              onClick={() => setState({ palette: true })}
            >
              {MOD}K
            </button>
          )}
        </div>
        {filter.trim() && (
          <button className={`nav-item search-msgs ${view.kind === 'search' ? 'active' : ''}`} onClick={searchMessages}>
            <Icon name="search" size={14} />
            <span className="truncate">{tr(`在消息里搜索「${filter.trim()}」`, `Search messages for "${filter.trim()}"`)}</span>
            <span className="kbd-hint">Enter</span>
          </button>
        )}
        <div className="hosts-scroll">
          {hosts.map((h) => (
            <HostBlock key={h.config.id} h={h} filter={filter} />
          ))}
          {!filter && (
            <button className={`add-host ${view.kind === 'hosts' ? 'active' : ''}`} onClick={() => navigate({ kind: 'hosts' })}>
              <Icon name="plus" size={13} /> {tr('添加 / 管理主机', 'Add / Manage Hosts')}
            </button>
          )}
        </div>
        <div className="sidebar-foot">
          <div className="brand">
            <img src="/icon.svg" alt="" width={20} height={20} />
            <span className="truncate">Yi Console</span>
            {transport === 'closed' && <span className="pill pill-err">{tr('离线', 'Offline')}</span>}
          </div>
          <button
            className={`icon-btn ${view.kind === 'hosts' ? 'active' : ''}`}
            title={tr('主机管理', 'Hosts')}
            onClick={() => navigate({ kind: 'hosts' })}
          >
            <Icon name="server" size={16} />
          </button>
          <button
            className={`icon-btn ${view.kind === 'prefs' ? 'active' : ''}`}
            title={tr('系统设置（外观、语言、通知、网络代理）', 'Preferences (Appearance, Language, Notifications, Proxy)')}
            onClick={() => navigate({ kind: 'prefs' })}
          >
            <Icon name="settings" size={16} />
          </button>
        </div>
      </aside>
    </>
  );
}
